# 14. Sedona VM Scalability Limits & Rust Solutions

## Overview

This document analyzes every scalability bottleneck in the current Sedona VM and Sandstar system that could cause crashes, hangs, or degraded performance as the number of components grows. We identify hard limits, soft limits, and algorithmic problems, then describe how the Rust implementation eliminates each one.

**Key Finding:** The system has no single "crash at N components" limit. Instead, multiple compounding bottlenecks create a degradation curve that becomes catastrophic somewhere between 2,000 and 10,000 components depending on tree depth, link density, and scan cycle timing.

---

## 1. Hard Limits (Compile-Time / Format Constraints)

### 1.1 Component ID: 16-bit (`short`)

**File:** `EacIo/src/sys/Component.sedona:507`
```java
short id                    // 16-bit signed: -32768 to 32767
define short nullId = 0xffff  // sentinel value
```

**File:** `EacIo/src/sys/App.sedona:840-843`
```java
int maxId = in.readU2()     // .sab file stores compId as unsigned 16-bit
initApp(maxId+1)            // pre-allocate comps array to maxId+1
```

**Limit:** Component IDs are stored as `short` (16-bit signed), with `0xffff` reserved as null sentinel. The .sab binary format uses `readU2()`/`writeI2()` (unsigned 16-bit) for serialization.

- **Theoretical max:** 65,534 component IDs (0 to 65,533, excluding 0xffff sentinel)
- **Practical max:** ~32,767 due to signed `short` in-memory representation
- **Crash mechanism:** Overflow wraps to negative, breaks `lookup()` array indexing

**Rust Solution:**
```rust
type ComponentId = u32;  // or even usize
const NULL_ID: ComponentId = u32::MAX;
// Supports 4 billion+ components — effectively unlimited
```

### 1.2 Scode Address Space: 256KB

**File:** `EacIo/src/vm/scode.h`
```c
#define SCODE_BLOCK_SIZE 4
// block2addr macro: block_index * SCODE_BLOCK_SIZE
// Block indices are 16-bit: max 65,535 × 4 = 262,140 bytes
```

**Limit:** All method addresses, type metadata, and virtual dispatch tables must fit in ~256KB of scode. With many component types, this ceiling can be hit.

- **Typical scode size:** 80-150KB for normal applications
- **At 20K components:** If using many distinct types, type metadata alone could approach 256KB
- **Crash mechanism:** `ERR_BAD_IMAGE_CODE_SIZE` on load, or corrupt pointer arithmetic

**Rust Solution:**
```rust
// Use 32-bit addressing for code/metadata
type CodeAddr = u32;  // 4GB addressable
// Or use direct Rust references — no block addressing needed
```

### 1.3 VM Stack: 16,384 Bytes (Hardcoded)

**File:** `EacIo/src/vm/main.cpp:372-373`
```cpp
vm->stackMaxSize = 16384;
vm->stackBaseAddr = (uint8_t *)malloc(vm->stackMaxSize);
```

**Limit:** The entire Sedona VM execution stack — all method calls, local variables, temporaries — must fit in 16KB. Each stack frame is ~20-40 bytes depending on method parameters and locals.

- **Max call depth:** ~400-800 frames (at 20-40 bytes each)
- **Crash mechanism:** Stack overflow corrupts adjacent heap memory (no guard page)
- **No overflow detection:** The VM does not check `sp` against stack bounds

**Rust Solution:**
```rust
struct VmStack {
    data: Vec<Cell>,
    capacity: usize,  // Configurable: 64KB, 256KB, or more
}

impl VmStack {
    fn push(&mut self, val: Cell) -> Result<(), VmError> {
        if self.data.len() >= self.capacity {
            return Err(VmError::StackOverflow);
        }
        self.data.push(val);
        Ok(())
    }
}
// Benefits:
// 1. Configurable size (not hardcoded)
// 2. Bounds checking on every push (debug mode)
// 3. Growable with Vec if needed
// 4. Stack overflow returns error instead of corrupting memory
```

### 1.4 Component Path Depth: 16

**File:** `EacIo/src/sys/Component.sedona:53`
```java
define int pathBufLen = 16  // max nesting depth
```

**File:** `EacIo/src/sys/Component.sedona:40`
```java
if (depth >= pathBufLen) return null  // silently fails
```

**Limit:** Component tree can only be 16 levels deep. Path lookup silently returns null beyond this.

**Rust Solution:**
```rust
fn path(&self, component_store: &ComponentStore) -> String {
    // Dynamic Vec — no fixed depth limit
    let mut segments: Vec<&str> = Vec::new();
    let mut current = Some(self);
    while let Some(comp) = current {
        segments.push(&comp.name);
        current = component_store.lookup(comp.parent);
    }
    segments.reverse();
    segments.join("/")
}
```

### 1.5 Watch Subscriptions: 4

**File:** `EacIo/src/sys/Watch.sedona:42`
```java
define int max = 4  // compile-time constant
```

**File:** `EacIo/src/sys/Component.sedona`
```java
inline byte[Watch.max] watchFlags  // 4 bytes per component
```

**Limit:** Only 4 concurrent watch sessions can exist. Each component carries a fixed 4-byte `watchFlags` array. With N components, this costs 4N bytes of memory that cannot be reclaimed.

- **At 20K components:** 80KB just for watch flags
- **Practical impact:** Only 4 clients can subscribe to changes simultaneously

**Rust Solution:**
```rust
// Watches are independent of component count
struct WatchManager {
    watches: Vec<Watch>,        // Growable, no fixed limit
    subscriptions: HashMap<ComponentId, HashSet<WatchId>>,  // Sparse
}
// Only components with active watchers use memory
// No per-component fixed allocation
```

### 1.6 MAX_CHANNELS: 10,000

**File:** `engine/src/channel.h:51`
```c
#define MAX_CHANNELS 10000
```

**Limit:** The engine pre-allocates an index array of `int[10000]` for O(1) channel lookup. Channel IDs above 10,000 are not addressable.

**Rust Solution:**
```rust
// Use HashMap for sparse channel mapping — no upper limit
let channels: HashMap<ChannelId, Channel> = HashMap::new();
// Or use a configurable Vec with runtime sizing
```

---

## 2. Soft Limits (Algorithmic / Memory Bottlenecks)

### 2.1 `executeTree()` — Recursive Tree Walk (CRITICAL)

**File:** `EacIo/src/sys/App.sedona:275-291`
```java
private void executeTree(Component c)
{
    // recurse children first
    if ((c.children != nullId) && c.allowChildExecute())
    {
        Component kid = lookup(c.children)
        while (kid != null)
        {
            executeTree(kid)          // ← RECURSIVE CALL
            kid = lookup(kid.nextSibling)
        }
    }
    // execute component (after children)
    c.propagateLinksTo()
    c.execute()
}
```

**Problem:** This is called every scan cycle (typically every 100ms-1s). The recursion depth equals the tree depth. Each recursive call:
- Pushes a Sedona stack frame (~20-40 bytes VM stack)
- Pushes a native C stack frame (~64-128 bytes native stack)
- With 16KB VM stack: max ~400 levels of recursion
- But `pathBufLen = 16` suggests design intent is shallow trees

**Scaling behavior:**
| Components | Tree Depth | VM Stack per cycle | Native Stack per cycle | Risk |
|------------|------------|-------------------|----------------------|------|
| 100 | 5-8 | ~200 bytes | ~640 bytes | None |
| 1,000 | 10-15 | ~600 bytes | ~1,920 bytes | Low |
| 2,000 | 15-30 | ~1,200 bytes | ~3,840 bytes | Medium |
| 5,000 | 30-50 | ~2,000 bytes | ~6,400 bytes | High |
| 10,000 | 50+ | ~4,000 bytes | ~12,800 bytes | **Stack overflow** |

**Note:** Tree depth depends on topology. A flat tree (all children of root) has depth 1 regardless of component count. A linear chain has depth = N. Real applications are typically 5-15 levels deep, but programmatically generated configurations can create deeper trees.

**Crash mechanism:** Stack overflow. The C native stack has no guard in the VM thread. Overflow silently corrupts heap memory, leading to random crashes or data corruption.

**Rust Solution:**
```rust
fn execute_tree(store: &mut ComponentStore, root_id: ComponentId) {
    // Iterative depth-first traversal — no recursion
    let mut stack: Vec<(ComponentId, bool)> = vec![(root_id, false)];

    while let Some((id, children_done)) = stack.pop() {
        let comp = match store.get(id) {
            Some(c) => c,
            None => continue,
        };

        if children_done {
            // All children executed — now execute this component
            comp.propagate_links_to(store);
            comp.execute(store);
        } else {
            // Push self back (will execute after children)
            stack.push((id, true));

            // Push children (will execute first)
            if comp.allow_child_execute() {
                let mut kid_id = comp.children;
                while kid_id != NULL_ID {
                    stack.push((kid_id, false));
                    kid_id = store.get(kid_id)
                        .map(|k| k.next_sibling)
                        .unwrap_or(NULL_ID);
                }
            }
        }
    }
}
// Benefits:
// 1. Stack is heap-allocated Vec — can grow to any depth
// 2. No recursion — immune to stack overflow
// 3. Same execution order (children before parent)
// 4. Can be monitored/profiled (stack.len() = current depth)
```

### 2.2 `allocCompId()` — O(n) Linear Scan

**File:** `EacIo/src/sys/App.sedona:395-408`
```java
private int allocCompId()
{
    int len = compsLen
    for (int i=0; i<len; ++i)       // ← O(n) scan every time
        if (comps[i] == null)
            return i
    if (!ensureCompsCapacity(len+8)) return -1
    return len
}
```

**Problem:** Every component addition scans the entire array looking for a null slot. Adding N components sequentially is O(n²) total.

| Components | Scans per add (avg) | Total scans to add all | Time at 1GHz |
|------------|--------------------|-----------------------|-------------|
| 100 | 50 | 5,000 | <1ms |
| 1,000 | 500 | 500,000 | ~1ms |
| 5,000 | 2,500 | 12,500,000 | ~12ms |
| 20,000 | 10,000 | 200,000,000 | ~200ms |

**Not a crash risk but causes slow app loading and component addition.**

**Rust Solution:**
```rust
struct ComponentStore {
    components: Vec<Option<Component>>,
    free_list: Vec<ComponentId>,  // O(1) allocation
}

impl ComponentStore {
    fn alloc_id(&mut self) -> Option<ComponentId> {
        // O(1): pop from free list
        if let Some(id) = self.free_list.pop() {
            return Some(id);
        }
        // Grow by doubling (amortized O(1))
        let id = self.components.len() as ComponentId;
        self.components.push(None);
        Some(id)
    }

    fn free_id(&mut self, id: ComponentId) {
        self.components[id as usize] = None;
        self.free_list.push(id);  // Return to free list
    }
}
```

### 2.3 `ensureCompsCapacity()` — Grow by 8 with Copy

**File:** `EacIo/src/sys/App.sedona:413-418`
```java
private bool ensureCompsCapacity(int newLen)
{
    if (compsLen >= newLen) return true
    Component[] old = comps
    Component[] temp = new Component[newLen]   // malloc new
    // implicit: copy old → temp, then comps = temp
}
```

**Problem:** Array grows by 8 slots at a time. Adding 20,000 components requires ~2,500 reallocation cycles, each copying the entire array. This is O(n²) total memory copies.

| Components | Reallocations | Total bytes copied |
|------------|--------------|-------------------|
| 100 | ~13 | ~5KB |
| 1,000 | ~125 | ~500KB |
| 5,000 | ~625 | ~12.5MB |
| 20,000 | ~2,500 | ~200MB |

**At 20K components, the system copies ~200MB of pointer data during growth.** On a 512MB BeagleBone, this creates severe memory pressure and fragmentation.

**Rust Solution:**
```rust
// Vec doubles capacity on growth (amortized O(1) push)
let mut components: Vec<Option<Component>> = Vec::with_capacity(initial_estimate);
// Adding 20K components: ~15 reallocations total (vs 2,500)
// Total bytes copied: ~320KB (vs ~200MB)
```

### 2.4 Link Traversal — Linked Lists per Component

**File:** `EacIo/src/sys/Component.sedona:227-231`
```java
internal void propagateLinksTo()
{
    for (Link link = linksTo; link != null; link = link.nextTo)
        link.propagate()
}
```

**File:** `EacIo/src/sys/Link.sedona`
```java
short fromComp      // source component ID
byte  fromSlot      // source slot index
short toComp        // destination component ID
byte  toSlot        // destination slot index
Link  nextFrom      // next link in fromComp's chain
Link  nextTo        // next link in toComp's chain
```

**Problem:** Links form two independent singly-linked lists (`linksTo` and `linksFrom`) per component. Each link is individually heap-allocated (~20 bytes). This means:

1. **Cache-hostile:** Linked list traversal thrashes CPU cache
2. **Heap fragmentation:** Each link is a separate `malloc` allocation
3. **O(L) per component per cycle:** Where L = number of links to that component
4. **Total per scan cycle:** O(N × avg_links) where N = component count

| Components | Avg Links/Comp | Link traversals per scan | Heap allocations |
|------------|---------------|------------------------|-----------------|
| 100 | 2 | 200 | 200 |
| 1,000 | 3 | 3,000 | 3,000 |
| 5,000 | 3 | 15,000 | 15,000 |
| 20,000 | 3 | 60,000 | 60,000 |

**With 60K individual malloc'd links on a 512MB ARM system, heap fragmentation becomes significant.**

**Rust Solution:**
```rust
// Arena-allocated link storage — cache-friendly, no fragmentation
struct LinkArena {
    links: Vec<Link>,  // Contiguous memory
}

// Per-component: store link indices, not pointers
struct Component {
    links_to: SmallVec<[LinkIdx; 4]>,   // Inline for ≤4 links
    links_from: SmallVec<[LinkIdx; 4]>,
}

// Propagation iterates a contiguous slice
fn propagate_links_to(&self, arena: &LinkArena, store: &ComponentStore) {
    for &idx in &self.links_to {
        arena.links[idx.0].propagate(store);
    }
}
// Benefits:
// 1. All links in contiguous memory — CPU cache friendly
// 2. No per-link malloc — zero fragmentation
// 3. SmallVec inlines up to 4 links without heap allocation
// 4. Iteration is sequential memory access, not pointer chasing
```

### 2.5 IPC Ring Buffer: 200 Messages

**File:** `EacIo/src/EacIo/native/engineio.c:59-60`
```c
#define MAX_BUFFERED_MESSAGES 200
#define MESSAGE_FLUSH_INTERVAL_US 10000  // 10ms
```

**Problem:** When the engine and Sedona/Haystack communicate via POSIX message queues, overflow messages are buffered in a 200-entry circular buffer. With many active channels, this buffer can overflow and lose messages.

| Active Channels | Messages per scan | Buffer utilization |
|----------------|------------------|--------------------|
| 100 | ~100 | 50% |
| 500 | ~500 | **Overflow** |
| 2,000 | ~2,000 | **10x overflow** |

**Lost messages mean stale sensor readings.** The system doesn't crash but silently loses data.

**Rust Solution:**
```rust
// tokio::sync::mpsc — bounded channel with backpressure
let (tx, rx) = tokio::sync::mpsc::channel::<EngineMessage>(8192);

// Sender gets backpressure if channel is full
match tx.try_send(msg) {
    Ok(()) => {},
    Err(TrySendError::Full(_)) => {
        // Log warning, apply backpressure
        metrics.increment("ipc.backpressure");
        tx.send(msg).await?;  // Block until space available
    }
    Err(TrySendError::Closed(_)) => return Err(IpcError::ChannelClosed),
}
// Benefits:
// 1. Configurable buffer size (8K, 16K, or more)
// 2. Backpressure instead of silent data loss
// 3. Async/await — no pthread mutex contention
// 4. Type-safe messages — no memcpy of raw structs
```

### 2.6 Grid Parsing — O(n × m) with No Limits

**File:** `EacIo/src/EacIo/native/haystack/points.cpp:879`
```cpp
/* TBD: What if n_rows and n_cols are zero or very big number?
   Please place a limit */
for (size_t i = 0; i < n_rows; i++)
    for (size_t j = 0; j < n_cols; j++)
        // Process cell
```

**Problem:** Zinc grid parsing during configuration load and API responses has no limit on rows × columns. The developers themselves flagged this as a known issue (see the TBD comment).

| Points | Tags/Point | Iterations | Estimated Time |
|--------|-----------|------------|---------------|
| 100 | 30 | 3,000 | <1ms |
| 1,000 | 40 | 40,000 | ~5ms |
| 5,000 | 50 | 250,000 | ~50ms |
| 20,000 | 50 | 1,000,000 | ~200ms |

**At 20K points, grid operations block the HTTP server for 200ms+.** During a full reload, multiple grid parses may chain together.

**Rust Solution:**
```rust
// Streaming grid parser with configurable limits
struct GridConfig {
    max_rows: usize,      // Default: 100,000
    max_cols: usize,      // Default: 1,000
    parse_timeout: Duration,  // Default: 5 seconds
}

// Lazy iteration — only materialize rows as needed
fn parse_grid<'a>(input: &'a str, config: &GridConfig)
    -> impl Iterator<Item = Result<Row, GridError>> + 'a
{
    ZincParser::new(input)
        .rows()
        .take(config.max_rows)
        .map(move |row| {
            if row.len() > config.max_cols {
                Err(GridError::TooManyColumns(row.len()))
            } else {
                Ok(row)
            }
        })
}
```

### 2.7 Linear Record Search — O(n) per Update

**File:** `EacIo/src/EacIo/native/haystack/points.cpp:2515-2520`
```cpp
size_t max_iterations = m_recs.size() + 10;
for (recs_t::const_iterator it = m_recs.begin();
     it != e && count < max_iterations; ++it)
    if (row->get_int("channel") == channel)
        { /* update */ break; }
```

**Problem:** `writeComponentId()` and `writeComponentType()` (called from Sedona native methods) do a linear scan of all records to find the matching channel. These are called frequently during Sedona execution.

| Points | Scans per call | Calls per cycle | Total iterations/cycle |
|--------|---------------|----------------|----------------------|
| 100 | 50 | 10 | 500 |
| 1,000 | 500 | 50 | 25,000 |
| 5,000 | 2,500 | 100 | 250,000 |
| 20,000 | 10,000 | 200 | 2,000,000 |

**Note:** A channel index (`m_channelIndex`) exists in points.cpp but isn't used in these code paths.

**Rust Solution:**
```rust
// HashMap gives O(1) lookup by channel
struct PointServer {
    records: HashMap<RecordId, Record>,
    channel_index: HashMap<ChannelId, RecordId>,  // Always maintained
    // Any lookup by channel is O(1)
}

fn write_component_id(&mut self, channel: ChannelId, comp_id: ComponentId) {
    if let Some(&rec_id) = self.channel_index.get(&channel) {
        if let Some(rec) = self.records.get_mut(&rec_id) {
            rec.component_id = Some(comp_id);
        }
    }
}
```

---

## 3. Memory Analysis at Scale

### 3.1 Per-Component Memory Budget (Current C Implementation)

| Component | Size | Notes |
|-----------|------|-------|
| `comps[]` slot | 4 bytes | Pointer in array |
| Component object header | 8 bytes | malloc header |
| Component fields | ~60 bytes | id, parent, children, siblings, watch flags, type ptr, etc. |
| Component name | 32 bytes | Fixed `Str(32)` |
| Component properties | 20-200 bytes | Varies by type (PID has ~15 float props = 60 bytes) |
| Watch flags | 4 bytes | `byte[Watch.max]` |
| **Subtotal per component** | **~130-310 bytes** | |

### 3.2 Per-Link Memory Budget

| Item | Size | Notes |
|------|------|-------|
| malloc header | 8 bytes | Per-allocation overhead |
| Link object | 12 bytes | fromComp(2) + fromSlot(1) + toComp(2) + toSlot(1) + nextFrom(4) + nextTo(4) |
| **Total per link** | **~20 bytes** | |

### 3.3 Total Memory by Component Count

Assumptions: avg 150 bytes/component, 2.5 links/component, plus overhead.

| Components | Component Memory | Links | comps[] Array | Scode | Grid Data | **Total** |
|------------|-----------------|-------|---------------|-------|-----------|-----------|
| 100 | 15 KB | 5 KB | 0.4 KB | 80 KB | 50 KB | **~150 KB** |
| 500 | 75 KB | 25 KB | 2 KB | 100 KB | 200 KB | **~400 KB** |
| 1,000 | 150 KB | 50 KB | 4 KB | 120 KB | 400 KB | **~725 KB** |
| 2,000 | 300 KB | 100 KB | 8 KB | 150 KB | 800 KB | **~1.4 MB** |
| 5,000 | 750 KB | 250 KB | 20 KB | 200 KB | 2 MB | **~3.2 MB** |
| 10,000 | 1.5 MB | 500 KB | 40 KB | 250 KB | 4 MB | **~6.3 MB** |
| 20,000 | 3 MB | 1 MB | 80 KB | **256 KB (LIMIT)** | 8 MB | **~12.3 MB** |

**On a 512MB BeagleBone, memory is NOT the primary bottleneck.** Even 20K components only need ~12MB. The real problems are:

1. **Stack overflow** from recursive `executeTree()` (can crash at any component count with deep trees)
2. **Scode 256KB limit** (may be hit before 20K with complex type hierarchies)
3. **O(n²) allocation patterns** causing extreme GC pressure and fragmentation
4. **Scan cycle time** exceeding deadline (100ms-1s) due to O(n × links) traversal

### 3.4 Heap Fragmentation Analysis

The grow-by-8 pattern in `ensureCompsCapacity()` creates a specific fragmentation pattern:

```
Allocation sequence for 2000 components:
  malloc(8 × 4 = 32)    → free
  malloc(16 × 4 = 64)   → free
  malloc(24 × 4 = 96)   → free
  ...
  malloc(2000 × 4 = 8000)

Total allocations: 250 malloc/free cycles
Total bytes allocated then freed: ~1MB
```

Each free'd block becomes a hole in the heap. After 250 cycles, the heap has 250 holes of increasing size. New allocations for components may not fit in these holes, pushing the heap watermark higher.

**On ARM with glibc malloc:** The `brk()` heap only grows, never shrinks. After the growth phase, ~1MB of heap is permanently fragmented into unusable holes.

**Rust Solution:** `Vec` doubles capacity, so 2000 components requires only ~11 reallocations. The old memory is returned to the allocator in large, reusable chunks.

---

## 4. Scan Cycle Timing Analysis

The Sedona VM executes components in a tight loop with a target scan period (configurable, typically 100ms-1000ms):

**File:** `EacIo/src/sys/App.sedona:204-207`
```java
deadline = deadline + (long)scanPeriod*1ms
executeTree(this)   // Execute all components
// Services get remaining time
```

### Time per scan cycle (estimated, ARM Cortex-A8 @ 1GHz)

| Operation | Per component | 100 comps | 1K comps | 5K comps | 20K comps |
|-----------|--------------|-----------|----------|----------|-----------|
| executeTree traversal | ~0.5 μs | 50 μs | 500 μs | 2.5 ms | 10 ms |
| propagateLinksTo | ~1 μs × links | 200 μs | 3 ms | 15 ms | 60 ms |
| component.execute() | ~2-10 μs | 0.5 ms | 5 ms | 25 ms | 100 ms |
| **Total per scan** | | **~0.75 ms** | **~8.5 ms** | **~42.5 ms** | **~170 ms** |

**At 20K components with 100ms scan period:**
- Scan cycle takes ~170ms
- Exceeds 100ms deadline by 70%
- System becomes "always behind" — never catches up
- Real-time control quality degrades
- Watchdog timeouts may trigger

**At 2K components with 100ms scan period:**
- Scan cycle takes ~17ms
- 83ms headroom for services
- System operates normally

**At 5K components with 100ms scan period:**
- Scan cycle takes ~43ms
- 57ms headroom — getting tight
- Services may not complete in time

### Rust Improvement

The Rust implementation can parallelize independent subtrees:

```rust
// Parallel execution of independent component subtrees
async fn execute_tree_parallel(store: &Arc<RwLock<ComponentStore>>, root: ComponentId) {
    let children = store.read().await.get_children(root);

    // Execute independent subtrees concurrently
    let handles: Vec<_> = children.iter()
        .map(|&child_id| {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                execute_subtree(&store, child_id).await;
            })
        })
        .collect();

    futures::future::join_all(handles).await;

    // Execute root after all children
    store.write().await.get_mut(root).unwrap().execute();
}
```

**Estimated improvement:** 2-4x speedup on multi-core systems (BeagleBone has 1 core, but future hardware may have more). Even on single-core, the iterative traversal avoids function call overhead of recursion.

---

## 5. Crash Scenarios and Root Cause Analysis

### Scenario A: Deep Component Tree (Crash at ~400-800 components)

**Trigger:** Components arranged in a linear chain (parent → child → grandchild...)
**Root cause:** `executeTree()` recursion exhausts 16KB VM stack
**Symptoms:** Segfault, stack smashing detected, random memory corruption
**Current detection:** None — no stack overflow guard

**Rust fix:** Iterative tree walk (Section 2.1)

### Scenario B: Rapid Component Addition (Hang at ~2,000-5,000 components)

**Trigger:** Loading a large .sab file or adding many components via Sox
**Root cause:** O(n²) `allocCompId()` + O(n²) `ensureCompsCapacity()` copies
**Symptoms:** Multi-second pause during load, watchdog timeout
**Current detection:** None — appears as "system unresponsive"

**Rust fix:** Free list allocation + Vec doubling (Sections 2.2, 2.3)

### Scenario C: Large Point Database (Degradation at ~5,000-10,000 points)

**Trigger:** Haystack grid with many points, frequent updates from Sedona
**Root cause:** O(n) linear search per `writeComponentId()` call + O(n×m) grid parsing
**Symptoms:** Increasing HTTP response latency, missed scan deadlines
**Current detection:** TBD comment in points.cpp acknowledges the problem

**Rust fix:** HashMap O(1) lookup + streaming grid parser (Sections 2.6, 2.7)

### Scenario D: High Link Density (Degradation proportional to links)

**Trigger:** Components with many connections (e.g., a mux with 100 inputs)
**Root cause:** Linked list traversal in `propagateLinksTo()` for every scan cycle
**Symptoms:** Scan cycle time exceeds deadline
**Current detection:** None

**Rust fix:** Arena-allocated links + SmallVec (Section 2.4)

### Scenario E: Scode Size Exceeded (Crash at load time)

**Trigger:** Application with many distinct component types
**Root cause:** 16-bit block addressing limits scode to 256KB
**Symptoms:** `ERR_BAD_IMAGE_CODE_SIZE` or silent corruption
**Current detection:** vmInit checks code size match

**Rust fix:** 32-bit or native addressing (Section 1.2)

---

## 6. Complete Rust Architecture for Scalability

### 6.1 Component Store Design

```rust
/// Scalable component storage with O(1) operations
pub struct ComponentStore {
    /// Indexed by ComponentId — O(1) lookup
    components: Vec<Option<Box<Component>>>,

    /// Free ID list for O(1) allocation
    free_ids: Vec<ComponentId>,

    /// Pre-allocated link arena — cache-friendly
    links: LinkArena,

    /// Service linked list (separate from components)
    services: Vec<ComponentId>,

    /// Configurable limits (not hardcoded)
    config: StoreConfig,
}

pub struct StoreConfig {
    pub max_components: u32,        // Default: 100,000
    pub initial_capacity: u32,      // Default: 1,000
    pub max_tree_depth: u32,        // Default: 256
    pub stack_size: usize,          // Default: 65,536 bytes
    pub max_links: u32,             // Default: 500,000
    pub scan_period: Duration,      // Default: 100ms
}
```

### 6.2 Scalable Execute Loop

```rust
pub fn execute_cycle(store: &mut ComponentStore) -> CycleStats {
    let start = Instant::now();
    let mut stats = CycleStats::default();

    // Iterative depth-first traversal (no recursion)
    let mut work_stack: Vec<WorkItem> = Vec::with_capacity(256);
    work_stack.push(WorkItem::Enter(ROOT_ID));

    while let Some(item) = work_stack.pop() {
        match item {
            WorkItem::Enter(id) => {
                // Push Execute (will run after children)
                work_stack.push(WorkItem::Execute(id));

                // Push children in reverse order (so first child executes first)
                let children: SmallVec<[ComponentId; 8]> =
                    store.get_children_reverse(id);
                for child_id in children {
                    work_stack.push(WorkItem::Enter(child_id));
                }
            }
            WorkItem::Execute(id) => {
                if let Some(comp) = store.get_mut(id) {
                    comp.propagate_links_to(&store.links);
                    comp.execute();
                    stats.components_executed += 1;
                }
            }
        }

        // Scan cycle overrun detection
        if stats.components_executed % 1000 == 0 {
            let elapsed = start.elapsed();
            if elapsed > store.config.scan_period {
                tracing::warn!(
                    elapsed_ms = elapsed.as_millis(),
                    executed = stats.components_executed,
                    "Scan cycle overrun"
                );
                stats.overrun = true;
            }
        }
    }

    stats.duration = start.elapsed();
    stats
}
```

### 6.3 Memory-Efficient Link Arena

```rust
/// All links stored contiguously for cache efficiency
pub struct LinkArena {
    links: Vec<Link>,
    free_list: Vec<LinkIdx>,
}

pub struct Link {
    pub from_comp: ComponentId,
    pub from_slot: u8,
    pub to_comp: ComponentId,
    pub to_slot: u8,
}

/// Per-component link indices (not pointers)
pub struct ComponentLinks {
    pub links_to: SmallVec<[LinkIdx; 4]>,    // Most components have ≤4 links
    pub links_from: SmallVec<[LinkIdx; 4]>,
}
```

### 6.4 Configurable at Runtime

All limits become runtime-configurable via a TOML/JSON config file:

```toml
[vm]
stack_size = 65536          # 64KB (vs hardcoded 16KB)
max_components = 100000     # 100K (vs 32K short limit)
max_tree_depth = 256        # (vs 16 pathBufLen)
scan_period_ms = 100

[engine]
max_channels = 50000        # (vs hardcoded 10,000)
ipc_buffer_size = 8192      # (vs hardcoded 200)

[haystack]
max_watches = 256           # (vs hardcoded 4)
max_history_per_point = 1000  # (vs hardcoded 120)
grid_max_rows = 100000
grid_parse_timeout_ms = 5000
```

---

## 7. Summary: Limit Comparison Table

| Bottleneck | Current Limit | Crash Risk | Rust Solution | New Limit |
|-----------|--------------|-----------|---------------|-----------|
| Component ID type | 32,767 (short) | Medium | `u32` | 4 billion |
| VM stack size | 16 KB (hardcoded) | **HIGH** | Configurable Vec | 64KB+ (configurable) |
| executeTree recursion | Stack depth (~400) | **HIGH** | Iterative traversal | Unlimited |
| Component path depth | 16 levels | Medium | Dynamic Vec | Unlimited |
| Watch subscriptions | 4 | Medium | Dynamic Vec + HashMap | Configurable (256+) |
| MAX_CHANNELS | 10,000 | Medium | HashMap | Unlimited |
| comps[] growth | +8 per realloc | Low-Medium | Vec doubling | Amortized O(1) |
| allocCompId scan | O(n) linear | Low-Medium | Free list O(1) | O(1) |
| IPC ring buffer | 200 messages | Medium | tokio::mpsc(8192) | Configurable |
| Scode address space | 256 KB | Low-Medium | 32-bit or native | 4 GB |
| Grid parsing | No limit (O(n×m)) | Medium | Streaming + limits | Configurable |
| Record lookup | O(n) linear | Medium | HashMap O(1) | O(1) |
| Link storage | Individual malloc | Low | Arena allocation | Cache-friendly |
| Heap fragmentation | Grow-by-8 pattern | Medium | Vec doubling | Minimal |
| Per-component watch bytes | 4 bytes fixed | Low | Sparse HashMap | 0 bytes if unwatched |
| Scan cycle detection | None | **HIGH** | Overrun monitoring | Real-time alerts |

---

## 8. Recommended Testing Strategy

### 8.1 Stress Test Matrix

To validate the Rust implementation handles scale, run these tests:

| Test | Components | Links | Tree Depth | Scan Period | Expected |
|------|-----------|-------|-----------|-------------|----------|
| Baseline | 100 | 200 | 5 | 100ms | <5ms cycle |
| Medium load | 1,000 | 3,000 | 10 | 100ms | <15ms cycle |
| High load | 5,000 | 15,000 | 15 | 100ms | <50ms cycle |
| Stress | 20,000 | 60,000 | 20 | 100ms | <200ms cycle |
| Deep tree | 500 | 500 | 200 | 100ms | No stack overflow |
| Wide tree | 10,000 | 10,000 | 2 | 100ms | <100ms cycle |
| Link-heavy | 1,000 | 50,000 | 5 | 100ms | <80ms cycle |
| Allocation | 20,000 | 0 | 1 | N/A | <2s load time |

### 8.2 Regression Tests

```rust
#[test]
fn test_component_id_overflow() {
    let mut store = ComponentStore::new(StoreConfig {
        max_components: 100_000,
        ..Default::default()
    });
    // Add 50,000 components — must not overflow
    for i in 0..50_000 {
        let id = store.alloc_id().expect("Should allocate");
        assert_eq!(id, i as ComponentId);
    }
}

#[test]
fn test_deep_tree_no_stack_overflow() {
    let mut store = ComponentStore::new(Default::default());
    // Create 1000-level deep tree
    let mut parent = store.add_component("root", ROOT_ID);
    for i in 0..1000 {
        parent = store.add_component(&format!("level_{}", i), parent);
    }
    // Execute must complete without stack overflow
    execute_cycle(&mut store);
}

#[test]
fn test_scan_cycle_monitoring() {
    let mut store = large_component_store(20_000);
    let stats = execute_cycle(&mut store);
    assert!(stats.duration < Duration::from_secs(1),
        "20K components should execute in under 1 second");
    if stats.overrun {
        eprintln!("Warning: scan cycle overrun at {}ms",
            stats.duration.as_millis());
    }
}
```

---

## 9. Migration Path

### Phase 1: Fix Critical Crash Risks (Week 1-2)
1. Implement iterative `execute_tree()` — eliminates stack overflow
2. Implement configurable stack size — 64KB default
3. Add stack overflow detection with graceful error

### Phase 2: Fix Performance Bottlenecks (Week 3-4)
1. Implement `ComponentStore` with Vec + free list
2. Implement `LinkArena` for cache-friendly link storage
3. Use `u32` for component IDs
4. Implement configurable limits via config file

### Phase 3: Fix Haystack Scalability (Week 5-6)
1. Replace linear record search with HashMap
2. Add grid parsing limits and streaming
3. Increase IPC buffer with tokio::mpsc
4. Add scan cycle overrun detection and metrics

### Phase 4: Stress Testing (Week 7-8)
1. Run full stress test matrix
2. Profile on BeagleBone hardware
3. Tune configuration defaults
4. Document operational limits

---

## 10. Conclusion

The current Sedona VM has no single "crash at N" limit. Instead, it has a cascade of scaling problems:

1. **Crash risks** (can happen at any scale): Recursive `executeTree()` stack overflow, 16KB hardcoded stack
2. **Performance cliffs** (2K-5K components): O(n²) allocation patterns, linear scans, scan cycle overrun
3. **Hard ceilings** (10K-32K components): 16-bit component IDs, 256KB scode, 10K channel limit

The Rust implementation eliminates all of these by:
- **Iterative traversal** instead of recursion
- **Vec with doubling** instead of grow-by-8
- **Free list** instead of linear scan
- **HashMap** instead of linear search
- **Arena allocation** instead of per-object malloc
- **Configurable limits** instead of hardcoded constants
- **Runtime monitoring** instead of silent failure

The result is a system that scales smoothly to 100K+ components on the same 512MB BeagleBone hardware, with clear diagnostics when approaching any configured limit.
