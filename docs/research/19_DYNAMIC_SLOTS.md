# Dynamic Slots: Overcoming Sedona's Frozen Slot Model

## Document Purpose

This document analyzes the fundamental limitation of Sedona's compile-time-frozen slot architecture and proposes a hybrid static+dynamic slot system for the Rust port. The primary motivation is **point discovery**: when discovering LoRaWAN devices, Modbus registers, BACnet objects, or other protocol endpoints at runtime, we need to attach arbitrary key-value metadata as dynamic slots on point components.

**Related documents:**
- [12_SEDONA_VM_ARCHITECTURE_ANALYSIS.md](12_SEDONA_VM_ARCHITECTURE_ANALYSIS.md) — Sedona VM internals
- [13_SEDONA_VM_RUST_PORTING_STRATEGY.md](13_SEDONA_VM_RUST_PORTING_STRATEGY.md) — Rust VM conversion plan
- [14_SEDONA_VM_SCALABILITY_LIMITS.md](14_SEDONA_VM_SCALABILITY_LIMITS.md) — Scalability constraints
- [18_SEDONA_DRIVER_FRAMEWORK_V2.md](18_SEDONA_DRIVER_FRAMEWORK_V2.md) — Driver Framework v2 with learn/discovery

---

## Table of Contents

1. [The Problem: Frozen Slots](#1-the-problem-frozen-slots)
2. [Why Slots Are Frozen: Five Interlocking Constraints](#2-why-slots-are-frozen-five-interlocking-constraints)
3. [Slot Architecture Deep Dive](#3-slot-architecture-deep-dive)
4. [Use Cases Requiring Dynamic Slots](#4-use-cases-requiring-dynamic-slots)
5. [How Other Frameworks Solve This](#5-how-other-frameworks-solve-this)
6. [Proposed Solution: Hybrid Static+Dynamic Slot Model](#6-proposed-solution-hybrid-staticdynamic-slot-model)
7. [Rust Implementation Design](#7-rust-implementation-design)
8. [Protocol Integration (SOX/ROX)](#8-protocol-integration-soxrox)
9. [Persistence and Serialization](#9-persistence-and-serialization)
10. [Integration with Driver Framework v2](#10-integration-with-driver-framework-v2)
11. [Memory Budget Analysis](#11-memory-budget-analysis)
12. [Migration Strategy](#12-migration-strategy)
13. [Risk Analysis](#13-risk-analysis)

---

## 1. The Problem: Frozen Slots

### What Are Slots?

In the Sedona Framework, **slots** are the externally visible members of a component class — properties (configuration/runtime values) and actions (invocable commands). They are the "wiring points" that integrators use to assemble applications.

```java
// Sedona: slots are declared at compile time
class AnalogInput extends Component {
  @config property int channel      // slot 0: fixed at offset 58
  @readonly property float out      // slot 1: fixed at offset 62
  @readonly property Buf(64) name   // slot 2: fixed at offset 66
}
```

### The Limitation

Every slot — its name, type, byte offset within the component instance, and metadata — is determined at compile time by `sedonac` and baked into the scode binary image. **There is no mechanism to add, remove, or modify slots at runtime.**

This means:

1. **No runtime tag attachment** — Cannot add `devEUI: "A81758FFFE0312AB"` to a discovered LoRaWAN point
2. **No protocol-specific metadata** — Cannot store Modbus register address, BACnet object ID, or MQTT topic path on a point
3. **No discovery enrichment** — When `Driver::on_learn()` discovers a point with 15 metadata fields, those fields have nowhere to live on the component
4. **No Haystack tag model** — Project Haystack's entire value proposition is dynamic key-value tags on records; Sedona cannot participate

### Current Workaround: ExposeTags

The existing `ExposeTags` component (`EacIo/sedonaSrc/ExposeTags.sedona`) is the closest thing to dynamic properties. It has compile-time slots (`BoolVal`, `NumberVal`, `StringVal`) and reads tag values from the Haystack layer via native methods:

```java
BoolVal := eacio.getBoolTagValue(channel, Tag.toStr());
NumberVal := eacio.getNumberTagValue(channel, Tag.toStr());
```

This is the "meta-morphing" pattern: fixed typed mailbox slots carrying different runtime values depending on a `@config` tag name string. It works for a small number of known tags but **does not scale** to arbitrary discovery metadata.

---

## 2. Why Slots Are Frozen: Five Interlocking Constraints

### Constraint 1: Byte Offsets Baked Into Bytecode

The `sedonac` compiler computes the exact byte offset of each property within a type at compile time. These offsets are hardcoded as immediate operands in scode bytecode instructions:

```c
// vm.c line 946 — Load32BitFieldU2 opcode
Case Load32BitFieldU2:
  sp->ival = *(int32_t*)(((uint8_t*)sp->aval) + *(uint16_t*)(cp+1));
  cp += 3;
  EndInstr;
```

The `*(uint16_t*)(cp+1)` reads the 16-bit offset directly from the instruction stream — **no name lookup, no hash table, no indirection**. A single pointer dereference at a hardcoded offset.

Adding a runtime slot would not update these hardcoded offsets in already-compiled bytecode.

**Key code:** `vm.c` lines 920–1003 (59 storage opcodes with baked-in offsets)

### Constraint 2: Type.sizeof Is Compile-Time Fixed

`Type.sizeof` stores the exact number of bytes needed for an instance. `Type.malloc()` allocates exactly that many bytes:

```c
// sys_Type.c lines 12-33
Cell sys_Type_malloc(SedonaVM* vm, Cell* params) {
  uint8_t* self = params[0].aval;
  size_t size   = getTypeSizeof(self);   // Fixed at compile time
  void* mem = (void*)malloc(size);       // Exact allocation
  memset(mem, 0, size);                  // Zero-fill
  // ... run initializer ...
}
```

Adding a runtime property would require more memory than was allocated. There is no resize mechanism, and resizing would invalidate all existing pointers.

**Key code:** `sedona.h` line 412 (`getTypeSizeof`), `sys_Type.c` lines 12–33

### Constraint 3: Slot Metadata Lives in Read-Only Scode

The `Slot[]` array is `const inline` inside Type metadata, residing in the scode code segment:

```java
// Type.sedona
const class Type {
  const inline Slot[] slots    // Embedded in read-only scode
  const byte slotsLen          // Max 255
}
```

```c
// sedona.h line 340
const uint8_t* codeBaseAddr;   // Read-only code segment
```

There is no API, opcode, or native method that writes to the scode segment. You cannot append a Slot entry to the inline array.

**Key code:** `Slot.sedona` line 13 (`const class Slot`), `Type.sedona` line 114 (`const inline Slot[] slots`)

### Constraint 4: 8-bit Slot ID Space (Max 255)

`Slot.id` and `Type.slotsLen` are `const byte` (0–255). The SOX protocol uses a single `u1` byte for slot IDs in its wire format:

```
SOX read property: { u1 'r', u1 replyNum, u2 compId, u1 propId }
```

Dynamic slots would need to either fit within the remaining 255-slot namespace or change the wire protocol (breaking all SOX clients).

**Key code:** `SoxCommands.sedona` line 226

### Constraint 5: No Dynamic Collections in the VM

The Sedona VM provides no hash table, growable map, property bag, or dictionary abstraction. The only "dynamic" storage is `Buf` (fixed-size byte buffer requiring compile-time size declaration: `inline Buf(64) myBuffer`).

The three memory segments (scode code, stack, static data) plus explicit `malloc`/`free` provide no mechanism for associating arbitrary key-value pairs with a component.

### Summary: The Slot Access Performance Hierarchy

| Access Pattern | Mechanism | Speed | Key Code |
|---|---|---|---|
| Compiled field access | Bytecode offset (baked in) | ~1–2 ns | `vm.c:920–1003` |
| Reflective get/set | `getSlotHandle()` → byte offset | ~5–10 ns | `sys_Component.c:36–143` |
| SOX read/write | `type.slot(id)` → O(1) array | ~20 ns | `SoxCommands.sedona:221–516` |
| Name-based lookup | `type.findSlot(name)` → O(n) scan | ~100–500 ns | `Type.sedona:68–71` |
| **Dynamic slot (proposed)** | **HashMap/BTreeMap lookup** | **~50–200 ns** | **New** |

---

## 3. Slot Architecture Deep Dive

### Slot Metadata Structure (8 bytes in scode)

```
Byte 0:   id       (u8)   — Index in type's slot list
Byte 1:   flags    (u8)   — ACTION=0x01, CONFIG=0x02, AS_STR=0x04, OPERATOR=0x08
Bytes 2-3: name    (u16)  — Block index → string constant in scode
Bytes 4-5: type    (u16)  — Block index → Type metadata
Bytes 6-7: handle  (u16)  — Byte offset (properties) or vtable index (actions)
```

C macros from `sedona.h`:
```c
#define getSlotName(vm, self)   getConst(vm, self, 2)
#define getSlotType(vm, self)   getConst(vm, self, 4)
#define getSlotHandle(vm, self) getShort(self, 6)
```

### Component Instance Memory Layout

```
Offset  0:  uint16_t vtable         // Virtual method table block index
Offset  2:  uint16_t compId         // Component ID
Offset  4:  Str(32) name            // 32-byte inline name buffer
Offset 36:  short parent            // Parent component ID
Offset 38:  short children          // First child ID
Offset 40:  short nextSibling       // Next sibling ID
Offset 42:  Link* linksTo           // Linked list head
Offset 46:  Link* linksFrom         // Linked list head
Offset 50:  byte[4] watchFlags      // 4-byte watch bitmask
Offset 54:  int meta                // Security groups + wiresheet coords
Offset 58+: [subclass fields]       // Properties defined by subclasses
```

Total size = `Type.sizeof`, fixed at compile time.

### How Property Access Works (Reflective Path)

```c
// sys_Component.c — getFloat example
Cell sys_Component_getFloat(SedonaVM* vm, Cell* params) {
  uint8_t* self   = params[0].aval;              // Component instance
  uint8_t* slot   = params[1].aval;              // Slot metadata (in scode)
  uint16_t typeId = getTypeId(vm, getSlotType(vm, slot));
  uint16_t offset = getSlotHandle(vm, slot);      // FIXED byte offset

  if (typeId != FloatTypeId)
    return accessError(vm, "getFloat", self, slot);

  ret.ival = getInt(self, offset);  // *(int32_t*)(self + offset)
  return ret;
}
```

Step 4 is the critical bottleneck: `getSlotHandle()` reads a `uint16_t` at byte offset 6 of the Slot metadata — a value computed by `sedonac` and frozen in the scode binary.

---

## 4. Use Cases Requiring Dynamic Slots

### 4.1 LoRaWAN Device Discovery

When a LoRaWAN Network Server (e.g., ChirpStack, TTN) reports a device join, the following metadata is available:

```yaml
# Per-device metadata from LoRaWAN Network Server
devEUI: "A81758FFFE0312AB"
appEUI: "70B3D57ED0041234"
joinEUI: "0000000000000001"
deviceClass: "C"                   # A, B, or C
activationType: "OTAA"             # OTAA or ABP
lorawanVersion: "1.0.3"
regionalParams: "US915"
dataRate: "DR3"
txPower: 14                        # dBm
adr: true                          # Adaptive Data Rate
fCntUp: 12847                      # Uplink frame counter
fCntDown: 3291                     # Downlink frame counter
lastSeenAt: "2026-02-26T14:30:00Z"
rssi: -72                          # dBm
snr: 8.5                           # dB
gatewayId: "A840411EE1804150"
payloadCodec: "cayenne_lpp"        # Decoder name
batteryLevel: 254                  # 0-254 (device reported)
margin: 10                         # Link margin from DevStatusAns
```

**Volume:** 15–20 tags per device, 50–200 devices per gateway, refreshed on each uplink.

A Sedona component for a LoRaWAN point has no slots to store any of this. Today these would simply be lost.

### 4.2 Modbus Register Discovery

```yaml
# Per-register metadata from Modbus device scan
address: 40001                     # Holding register address
functionCode: 3                    # Read Holding Registers
dataType: "float32"                # IEEE 754 float
byteOrder: "ABCD"                  # Big-endian
scaleFactor: 0.1
offset: 0
unit: "kWh"
description: "Total Active Energy"
pollGroup: "fast"                  # 1s poll group
```

### 4.3 BACnet Object Discovery

```yaml
# Per-object metadata from BACnet Who-Is / ReadPropertyMultiple
objectType: "analog-input"
objectInstance: 1
objectName: "Zone Temperature"
description: "Room 201 Temp Sensor"
presentValue: 72.4
statusFlags: [false, false, false, false]
units: "degrees-fahrenheit"
covIncrement: 0.5
outOfService: false
reliability: "no-fault-detected"
eventState: "normal"
```

### 4.4 MQTT Topic Discovery

```yaml
# Per-topic metadata from MQTT broker
topic: "building/floor2/ahu3/sat"
qos: 1
retained: true
payloadFormat: "json"
jsonPath: "$.temperature"
unit: "°F"
lastMessage: "2026-02-26T14:28:00Z"
```

### 4.5 XetoOp Tag Discovery

The existing XetoOp (`/xeto?type=analog`) discovers tags by scanning live records. Currently, tags from SkySpark's Rec Engine (`reAutoTagMatch`, `reTagTitle`, etc.) leak through because there's no dynamic metadata to distinguish "intrinsic Sandstar tags" from "externally-injected tags." Dynamic slots would allow tagging each discovered property with its origin/source.

---

## 5. How Other Frameworks Solve This

### 5.1 Project Haystack — Dict (Pure Dynamic Tags)

Haystack's core data model is the **Dict**: an unordered map of string keys to typed values. Every record (point, equip, site) is a Dict. There are no static slots — everything is dynamic.

```
// Haystack Zinc record — all tags are dynamic
id: @p:demo:r:abc, dis: "Zone Temp", point, sensor, temp, air,
  kind: "Number", unit: "°F", equipRef: @p:demo:r:ahu1
```

**libhaystack (Rust)** implements `Dict` as `BTreeMap<String, Value>`:
- Keys: `String` (heap-allocated)
- Values: `Value` enum (Marker, Bool, Number, Str, Ref, DateTime, etc.)
- Ordered by key for deterministic serialization
- Full serde support (JSON, Zinc, Trio encoding)

**Trade-off:** Maximum flexibility but no compile-time type safety on individual fields. No fixed-offset access optimization.

### 5.2 Haxall/SkySpark — Fantom Dict + Folio Database

Haxall extends Haystack with a persistent database (Folio) where every record is a Dict. The `learn()` operation returns a Grid of Dicts:

```fantom
// Haxall connector learn() returns dynamic tag grid
override Grid onLearn(Obj? arg) {
  rows := Etc.makeDicts([
    ["dis": "Zone Temp", "point": Marker.val, "kind": "Number",
     "bacnetObjectType": "analog-input", "bacnetObjectId": 1]
  ])
  return Etc.makeListGrid(null, rows)
}
```

Discovered tags are committed to Folio as first-class Dict records. No schema restriction — any tag can be added to any record at any time.

### 5.3 Niagara/Tridium — BObject + Dynamic Properties

Niagara uses a Java-based component model (`BObject`) with:
- **Static slots** defined in class declarations (like Sedona)
- **Dynamic slots** added via `BDynamicSlot` at runtime
- **Frozen/thawed** lifecycle: components can be "thawed" to accept new dynamic slots, then "frozen" for execution

```java
// Niagara: adding a dynamic slot at runtime
BComponent comp = ...;
comp.add("devEUI", BString.make("A81758FFFE0312AB"));
comp.add("rssi", BDouble.make(-72.0));
```

This is the closest model to what we need. Niagara proves that a hybrid static+dynamic slot system works in production building automation.

### 5.4 BACnet — Proprietary Properties

BACnet objects have a fixed set of standard properties (Present_Value, Status_Flags, etc.) plus **proprietary properties** in the range 512–4194303. Any vendor can add proprietary properties to any object type.

Wire protocol supports both:
- Standard property access by well-known numeric ID
- Proprietary property access by vendor-assigned numeric ID

### 5.5 Comparison Table

| Framework | Static Slots | Dynamic Slots | Storage | Wire Protocol |
|-----------|-------------|---------------|---------|---------------|
| **Sedona** | Yes (frozen) | No | Fixed-offset | SOX (slot ID) |
| **Haystack** | No | Yes (Dict) | BTreeMap | Zinc/JSON/Trio |
| **Niagara** | Yes | Yes (BDynamic) | Java HashMap | Fox protocol |
| **BACnet** | Yes (standard) | Yes (proprietary) | Object memory | BACnet APDU |
| **Proposed Rust** | Yes (scode compat) | Yes (DynSlots) | Hybrid | SOX + ROX |

---

## 6. Proposed Solution: Hybrid Static+Dynamic Slot Model

### Design Principles

1. **Zero-cost for static slots** — Existing scode bytecode with fixed offsets continues to work at full speed
2. **Opt-in dynamic slots** — Only components that need dynamic metadata pay the cost
3. **Haystack-native storage** — Dynamic slots use `Dict` (BTreeMap<String, Value>) for direct Haystack compatibility
4. **Protocol-transparent** — Dynamic slots are visible via ROX (Trio encoding) and optionally SOX
5. **Persistent** — Dynamic slots survive restarts via database serialization
6. **Memory-bounded** — Configurable limits prevent unbounded growth on embedded devices

### Architecture: Three-Layer Slot Model

```
┌─────────────────────────────────────────────────────────────────┐
│                    Component Instance                            │
├─────────────────────────────────────────────────────────────────┤
│  Layer 1: Static Slots (Sedona-compatible)                      │
│  ┌──────────────────────────────────────────────────────┐       │
│  │  [vtable][compId][name(32)][parent][children][...]   │       │
│  │  [subclass fields at fixed offsets]                   │       │
│  │  Access: *(ptr + offset) — ~1-2 ns                   │       │
│  │  Source: sedonac compiler, frozen in scode            │       │
│  └──────────────────────────────────────────────────────┘       │
│                                                                  │
│  Layer 2: Dynamic Slots (New — Haystack Dict)                   │
│  ┌──────────────────────────────────────────────────────┐       │
│  │  Option<Box<Dict>>  — None if no dynamic slots       │       │
│  │  BTreeMap<String, Value> when present                 │       │
│  │  Access: map.get("key") — ~50-200 ns                 │       │
│  │  Source: runtime discovery, REST API, protocol learn  │       │
│  └──────────────────────────────────────────────────────┘       │
│                                                                  │
│  Layer 3: Computed/Virtual Slots (Read-only derived)            │
│  ┌──────────────────────────────────────────────────────┐       │
│  │  Lazily computed from Layer 1 + Layer 2               │       │
│  │  Examples: "effectiveUnit", "fullPath", "statusText"  │       │
│  │  Access: closure invocation — ~100-500 ns             │       │
│  │  Source: registered computation functions              │       │
│  └──────────────────────────────────────────────────────┘       │
└─────────────────────────────────────────────────────────────────┘
```

### Key Insight: Side-Car Pattern

Rather than modifying the Sedona component instance memory layout (which would break scode compatibility), dynamic slots live in a **side-car data structure** indexed by component ID:

```
┌─────────────┐     ┌──────────────────────────────┐
│ Sedona VM   │     │ DynSlotStore                  │
│             │     │                               │
│ comps[0] ───┤     │ comp_id → Option<Dict>        │
│ comps[1] ───┤     │   0 → None                    │
│ comps[2] ───┤     │   1 → None                    │
│ comps[3] ───┤     │   2 → Some({"devEUI": "A8..", │
│ ...         │     │          "rssi": -72, ...})    │
│             │     │   3 → Some({"address": 40001,  │
│             │     │          "dataType": "f32"})   │
└─────────────┘     └──────────────────────────────┘
     Static               Dynamic (side-car)
   (scode-compatible)    (Rust-native)
```

**Why side-car?**

1. **No scode modification** — The Sedona VM and all compiled bytecode work unchanged
2. **No pointer invalidation** — Component instances stay at their original addresses
3. **Lazy allocation** — Components without dynamic slots have zero overhead (just a `None` check)
4. **Independent lifecycle** — Dynamic slots can be added/removed without touching the VM
5. **Easy persistence** — The entire DynSlotStore can be serialized independently

---

## 7. Rust Implementation Design

### 7.1 Core Types

```rust
use libhaystack::haystack::val::{Dict, Value};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Unique component identifier (matches Sedona compId)
pub type CompId = u16;

/// Dynamic slot store — the side-car for all components
pub struct DynSlotStore {
    /// Component ID → dynamic tag dictionary
    slots: HashMap<CompId, Dict>,

    /// Maximum number of dynamic tags per component
    max_tags_per_comp: usize,

    /// Maximum total dynamic tag count across all components
    max_total_tags: usize,

    /// Current total tag count
    total_tags: usize,

    /// String interner for tag names (memory optimization)
    interner: StringInterner,
}

/// Thread-safe handle for concurrent access
pub type DynSlotStoreHandle = Arc<RwLock<DynSlotStore>>;
```

### 7.2 String Interner (Memory Optimization for ARM7)

Tag names like `devEUI`, `rssi`, `dataRate` repeat across many components. Instead of allocating separate `String` heap objects for each, we intern them:

```rust
use std::collections::HashMap;

/// Intern pool for dynamic slot key names
/// Reduces memory: 100 components × 15 tags each = 1500 strings
/// Without interning: 1500 × ~40 bytes = 60 KB
/// With interning: ~200 unique names × ~40 bytes + 1500 × 4 bytes = 14 KB
pub struct StringInterner {
    /// String → index
    map: HashMap<String, u32>,
    /// Index → string (for reverse lookup)
    strings: Vec<String>,
}

impl StringInterner {
    pub fn intern(&mut self, s: &str) -> InternedKey {
        if let Some(&idx) = self.map.get(s) {
            return InternedKey(idx);
        }
        let idx = self.strings.len() as u32;
        self.strings.push(s.to_owned());
        self.map.insert(s.to_owned(), idx);
        InternedKey(idx)
    }

    pub fn resolve(&self, key: InternedKey) -> &str {
        &self.strings[key.0 as usize]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InternedKey(u32);
```

**Memory savings on ARM7 (512 MB):**
- 200 devices × 15 tags = 3,000 tag instances
- Unique tag names: ~50 (devEUI, rssi, snr, etc. shared across devices)
- Without interning: 3,000 × 40 bytes = 120 KB in strings alone
- With interning: 50 × 40 bytes + 3,000 × 4 bytes = 14 KB
- **Savings: ~88%**

### 7.3 DynSlotStore API

```rust
impl DynSlotStore {
    /// Create a new store with configurable limits
    pub fn new(max_tags_per_comp: usize, max_total_tags: usize) -> Self {
        Self {
            slots: HashMap::new(),
            max_tags_per_comp,
            max_total_tags,
            total_tags: 0,
            interner: StringInterner::new(),
        }
    }

    // ── Single Tag Operations ──────────────────────────────

    /// Get a single dynamic tag value
    pub fn get(&self, comp_id: CompId, key: &str) -> Option<&Value> {
        self.slots.get(&comp_id)?.get(key)
    }

    /// Set a single dynamic tag (returns previous value if any)
    pub fn set(
        &mut self,
        comp_id: CompId,
        key: String,
        value: Value,
    ) -> Result<Option<Value>, DynSlotError> {
        let dict = self.slots.entry(comp_id).or_insert_with(Dict::new);

        // Check per-component limit
        if !dict.contains_key(&key) {
            if dict.len() >= self.max_tags_per_comp {
                return Err(DynSlotError::PerComponentLimitExceeded {
                    comp_id,
                    limit: self.max_tags_per_comp,
                });
            }
            if self.total_tags >= self.max_total_tags {
                return Err(DynSlotError::TotalLimitExceeded {
                    limit: self.max_total_tags,
                });
            }
            self.total_tags += 1;
        }

        // Intern the key name
        self.interner.intern(&key);

        Ok(dict.insert(key, value))
    }

    /// Remove a single dynamic tag
    pub fn remove(&mut self, comp_id: CompId, key: &str) -> Option<Value> {
        let dict = self.slots.get_mut(&comp_id)?;
        let removed = dict.remove(key);
        if removed.is_some() {
            self.total_tags -= 1;
            if dict.is_empty() {
                self.slots.remove(&comp_id);
            }
        }
        removed
    }

    // ── Bulk Operations ────────────────────────────────────

    /// Get all dynamic tags for a component (returns empty Dict if none)
    pub fn get_all(&self, comp_id: CompId) -> Option<&Dict> {
        self.slots.get(&comp_id)
    }

    /// Set all dynamic tags for a component at once (replaces existing)
    pub fn set_all(
        &mut self,
        comp_id: CompId,
        dict: Dict,
    ) -> Result<(), DynSlotError> {
        if dict.len() > self.max_tags_per_comp {
            return Err(DynSlotError::PerComponentLimitExceeded {
                comp_id,
                limit: self.max_tags_per_comp,
            });
        }

        // Adjust total count
        let old_count = self.slots.get(&comp_id).map_or(0, |d| d.len());
        let new_total = self.total_tags - old_count + dict.len();
        if new_total > self.max_total_tags {
            return Err(DynSlotError::TotalLimitExceeded {
                limit: self.max_total_tags,
            });
        }
        self.total_tags = new_total;

        // Intern all keys
        for key in dict.keys() {
            self.interner.intern(key);
        }

        self.slots.insert(comp_id, dict);
        Ok(())
    }

    /// Merge dynamic tags (add/update without removing existing)
    pub fn merge(
        &mut self,
        comp_id: CompId,
        tags: Dict,
    ) -> Result<(), DynSlotError> {
        let dict = self.slots.entry(comp_id).or_insert_with(Dict::new);

        let new_keys = tags.keys().filter(|k| !dict.contains_key(*k)).count();
        if dict.len() + new_keys > self.max_tags_per_comp {
            return Err(DynSlotError::PerComponentLimitExceeded {
                comp_id,
                limit: self.max_tags_per_comp,
            });
        }
        if self.total_tags + new_keys > self.max_total_tags {
            return Err(DynSlotError::TotalLimitExceeded {
                limit: self.max_total_tags,
            });
        }

        self.total_tags += new_keys;
        for (key, value) in tags {
            self.interner.intern(&key);
            dict.insert(key, value);
        }
        Ok(())
    }

    /// Remove all dynamic tags for a component
    pub fn clear(&mut self, comp_id: CompId) {
        if let Some(dict) = self.slots.remove(&comp_id) {
            self.total_tags -= dict.len();
        }
    }

    // ── Query Operations ───────────────────────────────────

    /// Find all components that have a specific dynamic tag
    pub fn find_by_tag(&self, key: &str) -> Vec<CompId> {
        self.slots.iter()
            .filter(|(_, dict)| dict.contains_key(key))
            .map(|(&id, _)| id)
            .collect()
    }

    /// Find components where a dynamic tag equals a specific value
    pub fn find_by_tag_value(&self, key: &str, value: &Value) -> Vec<CompId> {
        self.slots.iter()
            .filter(|(_, dict)| dict.get(key) == Some(value))
            .map(|(&id, _)| id)
            .collect()
    }

    // ── Statistics ─────────────────────────────────────────

    /// Number of components with dynamic slots
    pub fn component_count(&self) -> usize {
        self.slots.len()
    }

    /// Total dynamic tags across all components
    pub fn total_tag_count(&self) -> usize {
        self.total_tags
    }

    /// Memory estimate in bytes
    pub fn estimated_memory(&self) -> usize {
        let map_overhead = self.slots.len() * 64; // HashMap entry overhead
        let dict_overhead: usize = self.slots.values()
            .map(|d| d.len() * 80) // BTreeMap node + key + value estimate
            .sum();
        let interner_overhead = self.interner.strings.iter()
            .map(|s| s.len() + 24) // String + heap + HashMap entry
            .sum::<usize>();
        map_overhead + dict_overhead + interner_overhead
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DynSlotError {
    #[error("component {comp_id} exceeded max {limit} dynamic tags")]
    PerComponentLimitExceeded { comp_id: CompId, limit: usize },

    #[error("total dynamic tag limit {limit} exceeded")]
    TotalLimitExceeded { limit: usize },

    #[error("tag name '{name}' is reserved (conflicts with static slot)")]
    ReservedName { name: String },

    #[error("component {comp_id} not found")]
    ComponentNotFound { comp_id: CompId },
}
```

### 7.4 Unified Slot Access (Static + Dynamic)

The `ComponentProxy` provides a unified interface that first checks static slots (fast path), then falls back to dynamic slots:

```rust
/// Unified view of a component's static + dynamic slots
pub struct ComponentProxy<'a> {
    /// Sedona VM instance pointer (for static slot access)
    vm: &'a SedonaVm,
    /// Component instance pointer
    comp: *const u8,
    /// Component ID
    comp_id: CompId,
    /// Dynamic slot store reference
    dyn_store: &'a DynSlotStore,
}

impl<'a> ComponentProxy<'a> {
    /// Get a tag value — checks static slots first, then dynamic
    pub fn get(&self, name: &str) -> Option<Value> {
        // Fast path: check static slots by name
        if let Some(slot) = self.vm.find_slot(self.comp, name) {
            return Some(self.vm.read_slot_as_value(self.comp, slot));
        }

        // Slow path: check dynamic slots
        self.dyn_store.get(self.comp_id, name).cloned()
    }

    /// Set a tag value — static slots go through VM, dynamic go to store
    pub fn set(&mut self, name: &str, value: Value) -> Result<(), DynSlotError> {
        // Check if it's a static slot
        if let Some(slot) = self.vm.find_slot(self.comp, name) {
            self.vm.write_slot_from_value(self.comp, slot, &value);
            return Ok(());
        }

        // Dynamic slot
        self.dyn_store.set(self.comp_id, name.to_string(), value)?;
        Ok(())
    }

    /// Get all tags (merged static + dynamic) as a Haystack Dict
    pub fn to_dict(&self) -> Dict {
        let mut dict = Dict::new();

        // Add static slots
        for slot in self.vm.iter_slots(self.comp) {
            let name = self.vm.slot_name(slot);
            let value = self.vm.read_slot_as_value(self.comp, slot);
            dict.insert(name.to_string(), value);
        }

        // Add dynamic slots (dynamic wins on conflict — intentional)
        if let Some(dyn_dict) = self.dyn_store.get_all(self.comp_id) {
            for (key, value) in dyn_dict.iter() {
                dict.insert(key.clone(), value.clone());
            }
        }

        dict
    }
}
```

### 7.5 Data Structure Choice: BTreeMap vs HashMap

For the inner Dict storage, `libhaystack` uses `BTreeMap<String, Value>`. This is the right choice for Sandstar:

| Aspect | BTreeMap | HashMap |
|--------|----------|---------|
| Lookup (15 tags) | ~60 ns | ~40 ns |
| Insert | ~80 ns | ~50 ns |
| Iteration (ordered) | Yes (deterministic Zinc output) | No |
| Memory per entry | ~64 bytes | ~80 bytes |
| Cache behavior on ARM | Better (sequential nodes) | Worse (hash buckets scattered) |
| Serialization order | Deterministic | Random |

**Recommendation:** Use `BTreeMap` (via libhaystack `Dict`) for deterministic serialization and better ARM cache behavior with small tag sets (< 30 tags per component).

For the outer `CompId → Dict` map, use `HashMap<u16, Dict>` since component IDs are numeric and we need O(1) lookup by ID.

---

## 8. Protocol Integration (SOX/ROX)

### 8.1 ROX Protocol (Trio Encoding) — Native Fit

ROX uses Trio encoding (Haystack 4), which represents records as Dicts. Dynamic slots are **naturally supported** — they're just additional key-value pairs in the Trio output:

```
// ROX response for a component with dynamic slots
id: @comp:42
dis: "LoRaWAN Temp Sensor"
point
channel: 3100
// --- static slots above, dynamic slots below ---
devEUI: "A81758FFFE0312AB"
rssi: -72
snr: 8.5
dataRate: "DR3"
deviceClass: "C"
lastSeenAt: 2026-02-26T14:30:00Z UTC
```

No protocol changes needed — ROX already supports arbitrary tags.

### 8.2 SOX Protocol — Extension Required

SOX accesses slots by numeric ID (`u1 propId`). Dynamic slots don't have numeric IDs. Two options:

#### Option A: Virtual Slot Range (Recommended)

Reserve slot IDs 200–254 as "virtual dynamic slots" that map to the first N dynamic tags (alphabetically sorted):

```
SOX Read Property:
  compId=42, propId=0  → static slot 0 (channel)
  compId=42, propId=1  → static slot 1 (out)
  compId=42, propId=200 → dynamic slot 0 ("dataRate")
  compId=42, propId=201 → dynamic slot 1 ("devEUI")
  ...
  compId=42, propId=254 → dynamic slot 54
  compId=42, propId=255 → RESERVED (meta: count of dynamic slots)
```

Slot ID 255 returns the count of dynamic slots, allowing clients to enumerate them.

**Encoding:** Dynamic slot values are serialized as Trio strings in a `Buf` response, since SOX has no native Dict type.

#### Option B: New SOX Command (Future)

Add a new SOX command specifically for dynamic slots:

```
DynRead:  { u1 'd', u1 replyNum, u2 compId, Str tagName }
DynWrite: { u1 'D', u1 replyNum, u2 compId, Str tagName, Trio value }
DynList:  { u1 'l', u1 replyNum, u2 compId }
```

This is cleaner but requires updating all SOX clients.

### 8.3 Haystack REST API — Direct Dict Access

The Haystack REST API already returns Zinc/JSON grids. Dynamic slots are merged into the response:

```
// GET /read?filter=point
ver:"3.0"
id,dis,point,channel,devEUI,rssi,snr
@42,"LoRa Temp",M,3100,"A81758..F312AB",-72,8.5
```

No API changes needed — the `ComponentProxy::to_dict()` produces a merged Dict that serializes directly.

---

## 9. Persistence and Serialization

### 9.1 Storage Format

Dynamic slots are persisted independently from the Sedona `.sab` application image. Two options:

#### Option A: Zinc Grid File (Recommended)

```
// /home/eacio/sandstar/data/dynslots.zinc
ver:"3.0"
compId,tags
42,{devEUI:"A81758FFFE0312AB" rssi:-72 snr:8.5 dataRate:"DR3"}
43,{address:40001 functionCode:3 dataType:"float32"}
```

- Human-readable and debuggable
- Compatible with existing Zinc tooling
- Can be bulk-imported/exported via REST API

#### Option B: MessagePack Binary

```rust
// Compact binary serialization via rmp-serde
let bytes = rmp_serde::to_vec(&dyn_store.snapshot())?;
std::fs::write("/home/eacio/sandstar/data/dynslots.msgpack", bytes)?;
```

- Smaller file size (~40% of Zinc)
- Faster load/save
- Not human-readable

**Recommendation:** Use Zinc for the primary format (debuggability on embedded device), with optional MessagePack for large deployments (>500 components with dynamic slots).

### 9.2 Save Triggers

```rust
/// When to persist dynamic slots to disk
enum SaveTrigger {
    /// After any modification, debounced by 5 seconds
    Debounced { delay: Duration },
    /// On clean shutdown
    Shutdown,
    /// Periodic checkpoint
    Periodic { interval: Duration },
    /// Manual via REST API: POST /dynslots/save
    Manual,
}
```

Default configuration:
- **Debounced save:** 5 seconds after last modification
- **Periodic checkpoint:** Every 5 minutes
- **Shutdown save:** Always

### 9.3 Load on Startup

```rust
impl DynSlotStore {
    pub fn load_from_file(path: &Path) -> Result<Self, DynSlotError> {
        let content = std::fs::read_to_string(path)?;
        let grid = zinc::decode(&content)?;

        let mut store = Self::new(DEFAULT_MAX_PER_COMP, DEFAULT_MAX_TOTAL);
        for row in grid.rows() {
            let comp_id: CompId = row.get_int("compId")? as u16;
            let tags: Dict = row.get_dict("tags")?.clone();
            store.set_all(comp_id, tags)?;
        }
        Ok(store)
    }
}
```

---

## 10. Integration with Driver Framework v2

### 10.1 Learn/Discovery Flow with Dynamic Slots

The Driver Framework v2 (doc 18) defines `on_learn()` returning a `LearnGrid`. Dynamic slots connect the discovery results to persistent component metadata:

```
┌──────────────┐    on_learn()    ┌──────────────┐
│   Driver     │ ──────────────→  │  LearnGrid   │
│  (LoRaWAN)   │                  │  (discovery)  │
└──────────────┘                  └──────┬───────┘
                                         │
                            User selects points
                                         │
                                         ▼
                                  ┌──────────────┐
                                  │ PointConfig   │
                                  │ + LearnTags   │
                                  └──────┬───────┘
                                         │
                      AddPoint()         │
                    ┌────────────────────┘
                    ▼
          ┌─────────────────┐       set_all()      ┌──────────────┐
          │  Sedona Component│ ──────────────────→  │ DynSlotStore │
          │  (static slots)  │                      │ (dynamic tags)│
          │  compId=42       │                      │ comp 42: Dict│
          └─────────────────┘                       └──────────────┘
```

### 10.2 Enhanced LearnItem with Dynamic Tags

```rust
/// Discovery result item (from doc 18, enhanced)
#[derive(Debug, Clone, Serialize)]
pub struct LearnItem {
    /// Display name
    pub dis: String,

    /// Sub-path for drill-down (None = leaf point)
    pub learn: Option<String>,

    /// Static tags (become Sedona component config)
    pub static_tags: Dict,

    /// Dynamic tags (stored in DynSlotStore after point creation)
    /// These are protocol-specific metadata that don't map to Sedona slots
    pub dynamic_tags: Dict,
}
```

### 10.3 LoRaWAN Driver Example

```rust
impl Driver for LoRaWanDriver {
    async fn on_learn(&mut self, path: Option<&str>) -> Result<LearnGrid, DriverError> {
        let mut grid = LearnGrid::new();

        // Query ChirpStack API for devices
        let devices = self.chirpstack.list_devices().await?;

        for device in devices {
            grid.add(LearnItem {
                dis: device.name.clone(),
                learn: Some(format!("device/{}", device.dev_eui)),

                // Static tags → Sedona component slots
                static_tags: dict! {
                    "dis" => Value::make_str(&device.name),
                    "point" => Value::make_marker(),
                    "kind" => Value::make_str("Number"),
                },

                // Dynamic tags → DynSlotStore
                dynamic_tags: dict! {
                    "devEUI" => Value::make_str(&device.dev_eui),
                    "appEUI" => Value::make_str(&device.app_eui),
                    "deviceClass" => Value::make_str(&device.device_class),
                    "lorawanVersion" => Value::make_str(&device.lorawan_version),
                    "activationType" => Value::make_str(&device.activation_type),
                    "payloadCodec" => Value::make_str(&device.payload_codec),
                    "dataRate" => Value::make_str(&format!("DR{}", device.data_rate)),
                    "adr" => Value::make_bool(device.adr_enabled),
                },
            });
        }

        Ok(grid)
    }

    async fn on_poll(&mut self, points: &[PointId]) -> Result<Vec<PointUpdate>, DriverError> {
        let mut updates = Vec::new();

        for &point_id in points {
            let reading = self.read_point(point_id).await?;

            updates.push(PointUpdate {
                id: point_id,
                value: reading.value,
                status: PointStatus::Ok,

                // Update volatile dynamic tags on each poll
                dynamic_tag_updates: Some(dict! {
                    "rssi" => Value::make_number(reading.rssi as f64),
                    "snr" => Value::make_number(reading.snr),
                    "fCntUp" => Value::make_number(reading.frame_count as f64),
                    "lastSeenAt" => Value::make_str(&reading.timestamp.to_rfc3339()),
                }),
            });
        }

        Ok(updates)
    }
}
```

### 10.4 REST API Extensions

```rust
// New REST endpoints for dynamic slot management

// Get all dynamic tags for a component
// GET /api/dynslots/:compId
async fn get_dyn_slots(
    State(store): State<DynSlotStoreHandle>,
    Path(comp_id): Path<u16>,
) -> Result<Json<Dict>, StatusCode> {
    let store = store.read().unwrap();
    match store.get_all(comp_id) {
        Some(dict) => Ok(Json(dict.clone())),
        None => Ok(Json(Dict::new())),
    }
}

// Set/merge dynamic tags for a component
// PATCH /api/dynslots/:compId
async fn merge_dyn_slots(
    State(store): State<DynSlotStoreHandle>,
    Path(comp_id): Path<u16>,
    Json(tags): Json<Dict>,
) -> Result<StatusCode, (StatusCode, String)> {
    let mut store = store.write().unwrap();
    store.merge(comp_id, tags)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(StatusCode::OK)
}

// Delete a specific dynamic tag
// DELETE /api/dynslots/:compId/:tagName
async fn delete_dyn_slot(
    State(store): State<DynSlotStoreHandle>,
    Path((comp_id, tag_name)): Path<(u16, String)>,
) -> StatusCode {
    let mut store = store.write().unwrap();
    store.remove(comp_id, &tag_name);
    StatusCode::NO_CONTENT
}

// Query components by dynamic tag
// GET /api/dynslots?tag=devEUI&value=A81758FFFE0312AB
async fn query_dyn_slots(
    State(store): State<DynSlotStoreHandle>,
    Query(params): Query<DynSlotQuery>,
) -> Json<Vec<CompId>> {
    let store = store.read().unwrap();
    let results = match params.value {
        Some(val) => store.find_by_tag_value(&params.tag, &val),
        None => store.find_by_tag(&params.tag),
    };
    Json(results)
}
```

---

## 11. Memory Budget Analysis

### 11.1 Baseline: Current Sedona Memory Usage

On BeagleBone (512 MB RAM), the current Sandstar system uses:

| Component | Memory |
|-----------|--------|
| Sedona VM (scode + stack + static) | ~2 MB |
| Engine (channels, tables, polls) | ~4 MB |
| POCO HTTP server | ~8 MB |
| OS + system services | ~64 MB |
| **Available for dynamic slots** | **~430 MB** |

### 11.2 Dynamic Slot Memory Estimates

#### Small Deployment (50 LoRaWAN devices, 15 tags each)

```
Components with dynamic slots: 50
Tags per component: 15
Unique tag names: ~30

DynSlotStore HashMap:   50 × 64 bytes  =   3.2 KB
BTreeMap nodes:        750 × 64 bytes  =  48.0 KB
String values:         750 × 32 bytes  =  24.0 KB  (avg)
String interner:        30 × 40 bytes  =   1.2 KB
────────────────────────────────────────────────
Total:                                    76.4 KB
```

#### Medium Deployment (200 mixed devices, 20 tags each)

```
Components with dynamic slots: 200
Tags per component: 20
Unique tag names: ~80

DynSlotStore HashMap:   200 × 64 bytes =  12.8 KB
BTreeMap nodes:       4,000 × 64 bytes = 256.0 KB
String values:        4,000 × 32 bytes = 128.0 KB
String interner:         80 × 40 bytes =   3.2 KB
────────────────────────────────────────────────
Total:                                   400.0 KB
```

#### Large Deployment (1000 devices, 25 tags each)

```
Components with dynamic slots: 1,000
Tags per component: 25
Unique tag names: ~150

DynSlotStore HashMap: 1,000 × 64 bytes =   64 KB
BTreeMap nodes:      25,000 × 64 bytes = 1,600 KB
String values:       25,000 × 32 bytes =   800 KB
String interner:        150 × 40 bytes =     6 KB
────────────────────────────────────────────────
Total:                                   2,470 KB (~2.4 MB)
```

### 11.3 Memory Limits Configuration

```rust
/// Default memory limits for BeagleBone (512 MB RAM)
pub const DEFAULT_MAX_TAGS_PER_COMP: usize = 64;
pub const DEFAULT_MAX_TOTAL_TAGS: usize = 50_000;
pub const DEFAULT_MAX_MEMORY_BYTES: usize = 8 * 1024 * 1024; // 8 MB hard cap

/// Configurable via /home/eacio/sandstar/etc/config/dynslots.toml
/// [limits]
/// max_tags_per_component = 64
/// max_total_tags = 50000
/// max_memory_mb = 8
```

Even the large deployment (2.4 MB) is well within the 430 MB available, leaving enormous headroom.

---

## 12. Migration Strategy

### Phase 1: Side-Car Store (Minimal Invasive)

**Effort:** ~1 week | **Risk:** Low

1. Implement `DynSlotStore` as a standalone Rust module
2. Add REST API endpoints (`/api/dynslots/*`)
3. Persistence via Zinc file
4. No changes to Sedona VM or scode
5. Dynamic slots are only accessible via REST API and Driver Framework

**Deliverables:**
- `dynslots.rs` — Core store implementation
- `dynslots_api.rs` — Axum route handlers
- `dynslots.zinc` — Persistence file format

### Phase 2: Unified Component View

**Effort:** ~1 week | **Risk:** Low

1. Implement `ComponentProxy` for unified static+dynamic access
2. Integrate with Haystack REST ops (`/read`, `/nav`, `/xeto`)
3. Dynamic slots appear in Zinc/JSON grid responses
4. Haystack filter queries can match dynamic tags

### Phase 3: Driver Framework Integration

**Effort:** ~1 week | **Risk:** Low

1. Enhance `LearnItem` with `dynamic_tags` field
2. Auto-populate `DynSlotStore` when points are created from learn results
3. Update volatile tags (rssi, snr, etc.) on each poll cycle
4. Implement LoRaWAN driver as reference implementation

### Phase 4: SOX Protocol Extension

**Effort:** ~2 weeks | **Risk:** Medium

1. Implement virtual slot range (200–254) for SOX access
2. Add slot ID 255 as dynamic slot count meta-property
3. Test with existing SOX clients (Sedona Workbench)
4. Document wire format for dynamic slot values

### Phase 5: ROX Native Support

**Effort:** ~1 week | **Risk:** Low

1. Dynamic slots are automatically included in Trio encoding (dict merge)
2. ROX `subscribe` events include dynamic tag changes
3. roxWarp gossip protocol propagates dynamic slot diffs

---

## 13. Risk Analysis

### 13.1 Risks and Mitigations

| Risk | Impact | Probability | Mitigation |
|------|--------|-------------|------------|
| Memory pressure on BeagleBone | High | Low | Configurable limits, memory monitoring, 2.4 MB for 1000 devices is < 1% of RAM |
| SOX client compatibility | Medium | Medium | Virtual slot range is backward-compatible; clients that don't know about slots 200+ simply ignore them |
| Dynamic slot name collisions with static slots | Medium | Low | `ComponentProxy` checks static slots first; `DynSlotStore::set()` rejects names that match existing static slots |
| Persistence corruption on power loss | Medium | Low | Debounced writes + periodic checkpoints; Zinc is append-friendly for recovery |
| Thread contention on `RwLock` | Low | Low | Dynamic slot access is infrequent (per-poll, not per-scan-cycle); `RwLock` allows concurrent reads |
| Tag namespace pollution | Low | Medium | Convention: protocol-specific tags use prefixes (`lora.devEUI`, `modbus.address`, `bacnet.objectType`) |

### 13.2 What This Does NOT Change

1. **Sedona VM bytecode execution** — Static slot access via fixed offsets is untouched
2. **Scode binary format** — No changes to `.sax`/`.sab` files
3. **Sedona component lifecycle** — `start()`, `execute()`, `stop()`, `changed()` unchanged
4. **Link propagation** — Static slot links continue to work via offset-based access
5. **Existing Sedona applications** — All compiled Sedona code runs unchanged
6. **SOX basic operations** — Read/write of static slots (IDs 0–199) unchanged

### 13.3 Open Questions

1. **Tag namespacing convention** — Should we enforce prefixes (`lora.*`, `modbus.*`) or allow flat names?
2. **Dynamic slot change events** — Should dynamic tag changes trigger Sedona `changed()` callbacks?
3. **roxWarp propagation** — Should dynamic slot changes be included in roxWarp gossip diffs?
4. **XetoOp integration** — Should `/xeto?type=<type>` include dynamic tag schemas in its output?
5. **Haystack filter syntax** — Should `readAll(devEUI == "A8...")` search dynamic slots?

---

## Summary

The Sedona slot model is frozen by five interlocking constraints: bytecode-baked offsets, fixed `Type.sizeof`, read-only scode metadata, 8-bit slot ID space, and no dynamic collections. These constraints are fundamental to Sedona's deterministic, low-memory, real-time design.

Rather than fighting these constraints, the **side-car DynSlotStore** pattern adds dynamic slots alongside the existing static model. The Rust port's `ComponentProxy` provides a unified view that preserves zero-cost static slot access while enabling arbitrary tag attachment for discovery use cases.

The implementation uses libhaystack's `Dict` (BTreeMap<String, Value>) for Haystack-native compatibility, with string interning for memory efficiency on ARM7. Memory overhead is modest: ~76 KB for 50 devices, ~2.4 MB for 1000 devices — well within BeagleBone's 512 MB RAM.

This design bridges the gap between Sedona's compile-time component model and Project Haystack's dynamic tag model, enabling LoRaWAN, Modbus, BACnet, and MQTT point discovery without modifying the Sedona VM or breaking existing applications.
