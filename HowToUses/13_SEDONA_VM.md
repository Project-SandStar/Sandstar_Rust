# 13. Sedona VM (sandstar-svm crate)

The `sandstar-svm` crate provides two modes for running Sedona VM bytecode:
a **C FFI mode** that wraps the original C `vm.c` interpreter via FFI bindings,
and a **pure Rust VM mode** that replaces the C code entirely with a safe Rust
implementation.  Both modes execute the same `.scode` bytecode images produced
by the Sedona compiler (`sedonac`).

---

## Overview

The Sedona VM is a stack-based bytecode interpreter designed for embedded
building-automation controllers.  It executes compiled Sedona applications
(`.scode` files) that contain component-based DDC (Direct Digital Control)
logic: PID controllers, schedules, math blocks, HVAC sequences, and I/O
drivers.

The interpreter processes 240 opcodes organized into 10 categories:
literals, locals, stack manipulation, branching, comparison, integer
arithmetic, long arithmetic, float arithmetic, double arithmetic, and
object/storage operations.  Native methods provide host-platform services
(file I/O, networking, date/time, hardware access) that bytecode cannot
express directly.

### Why two modes?

| Aspect | C FFI mode (`svm` feature) | Pure Rust VM mode |
|--------|----------------------------|-------------------|
| Build dependency | Requires C compiler + vm.c sources | Cargo only |
| Safety | C code is `unsafe` across FFI boundary | Bounds-checked stack and memory |
| Platforms | Linux (ARM/x86) only (C native kits need Unix headers) | Any Rust target (Windows, Linux, macOS) |
| Stack overflow | Silent memory corruption in C VM | Returns `VmError::StackOverflow` |
| Debugging | Mixed GDB + Rust debugging | Unified Rust toolchain |
| Performance | Computed-goto dispatch (very fast) | Match-based dispatch (comparable) |

In practice the pure Rust VM is the preferred mode.  The C FFI mode exists
for backward compatibility with Sedona applications that rely on unported
native method kits.

---

## Running the VM

### Pure Rust VM (default, no C code)

```bash
cargo run -p sandstar-server -- --sedona --scode-path /path/to/kits.scode
```

### C FFI mode (requires `svm` feature + C compiler)

```bash
cargo run -p sandstar-server --features svm -- --sedona --scode-path /path/to/kits.scode
```

### Running VM tests

```bash
# All VM crate tests (650+ tests)
cargo test -p sandstar-svm

# List all test names
cargo test -p sandstar-svm -- --list

# Run a specific module's tests
cargo test -p sandstar-svm vm_interpreter
cargo test -p sandstar-svm native_sys
```

---

## Architecture

```
                    scode image (kits.scode)
                           |
                    +------v------+
                    | ImageLoader |  parse header, validate magic/version
                    +------+------+
                           |
              +------------v------------+
              |        VmMemory         |  code segment (RO) + data segment (RW)
              +------------+------------+
                           |
         +-----------------v-----------------+
         |          VmInterpreter            |
         |  - fetch opcode at PC             |
         |  - match on 240 Opcode variants   |
         |  - push/pop VmStack               |
         |  - read/write VmMemory            |
         |  - call NativeTable for natives   |
         +--------+----------------+---------+
                  |                |
          +-------v------+  +-----v-------+
          |   VmStack    |  | NativeTable |
          | i32 cells    |  | kit:method  |
          | CallFrame[]  |  | -> Rust fn  |
          +--------------+  +-------------+
```

### Code Segment (read-only)

The scode image contains compiled bytecode, type metadata, virtual dispatch
tables, string constants, and slot descriptors.  It is loaded once and never
modified.  Multi-byte values are little-endian.  Block addressing converts
a 16-bit block index to a byte offset by multiplying by the block size (4).

### Data Segment (read-write)

A separately allocated, zero-initialized byte buffer that holds component
instances, static fields, and the managed heap.  Native methods read and
write slots at known offsets within this segment.

### Stack

The `VmStack` stores `i32` cells.  Floats are bit-reinterpreted (`f32::to_bits`).
Wide values (long, double) occupy two adjacent cells (lower word first).
A separate `CallFrame` stack tracks return addresses, frame pointers, and
method metadata.  Bounds checking is always on -- overflow and underflow
return `VmError` instead of corrupting memory.

### Call Frame Layout

```
  stack temp N  <- sp
  ...
  stack temp 0
  local N       <- locals_base (fp + 3)
  local 1
  local 0
  method addr   <- fp + 2  (block index)
  prev fp       <- fp + 1  (stack index, 0 = none)
  return cp     <- fp      (code offset, 0 = top-level)
  param N
  param 1
  param 0       <- pp = fp - num_params
```

---

## 240 Opcodes in 10 Categories

| Category | Opcodes | Examples |
|----------|---------|---------|
| **Literals** | 0-15 | Nop, LoadIM1, LoadI0..I5, LoadIntU1/U2, LoadL0/L1, LoadF0/F1, LoadD0/D1 |
| **Locals** | 16-47 | LoadParam0..3, LoadParamWide, StoreParam, LoadLocal0..7, StoreLocal0..7 |
| **Stack** | 48-63 | Pop, Pop2, Pop3, Dup, Dup2, DupDown2, DupDown3, Swap |
| **Branching** | 64-95 | Jump, JumpNear, JumpFar, JumpZero, JumpNonZero, JumpIntEq..Lte, Switch, Foreach |
| **Comparison** | 96-111 | IntEq, IntNeq, IntGt..Lte, EqZero, NeqZero, ObjEq, ObjNeq, Is, Cast |
| **Int Arithmetic** | 112-143 | IntAdd, IntSub, IntMul, IntDiv, IntMod, IntNeg, IntInc, IntDec, IntAnd..Xor, IntShl/Shr, IntNot |
| **Long Arithmetic** | 144-175 | LongAdd..Mod, LongNeg, LongAnd..Xor, LongShl/Shr, LongNot, LongEq..Lte |
| **Float Arithmetic** | 176-207 | FloatAdd..Div, FloatNeg, FloatEq..Lte, FloatLt, FloatGt |
| **Double Arithmetic** | 208-223 | DoubleAdd..Div, DoubleNeg, DoubleEq..Lte |
| **Storage/Object** | 224-239 | LoadDataAddr, LoadConstStr, StoreRef, Call, CallVirtual, CallNative, ReturnPop, ReturnVoid, Alloc, ArrayGet/Set, SizeOf |

NaN equality is special-cased for floats and doubles: Sedona's null-float
sentinel (`0x7FC00000`) compares equal to itself, unlike IEEE 754.

---

## Native Method Kits

Native methods bridge VM bytecode to host-platform services.  They are
registered in a `NativeTable` indexed by `(kit_id, method_id)`.

| Kit | Name | Methods | Description | Status |
|-----|------|---------|-------------|--------|
| 0 | sys | 60 | Sys (malloc, copy, ticks, sleep, intStr...), Str, Type, Component reflection, FileStore, Test, StdOutStream | Rust |
| 2 | inet | 17 | TcpSocket, TcpServerSocket, UdpSocket, Crypto (SHA-1) | Rust (handle-based) |
| 4 | EacIo | 23 | Hardware I/O: channel read/write, tag queries, point status | Rust (via bridge) |
| 9 | datetimeStd | 3 | doNow (nanos since Sedona epoch), doSetClock, getUtcOffset | Rust (chrono-free) |
| 100 | shaystack | 28 | Remote Haystack client operations | Stubs |

### Kit 0 sys: Handle-Based Memory

The C VM passes raw `void*` pointers as `i32` Cell values.  On 64-bit hosts
where pointers do not fit in 32 bits, the Rust implementation uses a global
handle table: `malloc` returns an integer handle, and `copy`/`compareBytes`
resolve handles to actual memory.  This is transparent to bytecode.

### Kit 0 Component Reflection

`Component.getBool/getInt/getFloat/getLong/getDouble` and their `doSet*`
counterparts follow a slot-resolution chain:

1. `self` (params[0]) = data-segment offset of the component instance
2. `slot` (params[1]) = code-segment offset of the Slot descriptor
3. Slot descriptor points to a Type descriptor (block index) and a handle (field offset)
4. The actual value lives at `data[self + handle]`

### Kit 2 inet: Socket Handle Store

TCP and UDP sockets are managed through a global `SocketStore` that maps
integer handle IDs to Rust `TcpStream`/`TcpListener`/`UdpSocket` objects.
Non-blocking connect, accept, read, and write mirror the C implementation's
errno-based return codes.

---

## Configuration: VmConfig

`VmConfig` makes every hardcoded Sedona limit configurable:

| Parameter | Sedona Default | BeagleBone Default | Description |
|-----------|---------------|-------------------|-------------|
| `max_stack_size` | 16 KB | 64 KB | Execution stack |
| `max_components` | 256 | 4,096 | Component count |
| `max_code_size` | 256 KB | 4 MB | Scode image limit |
| `max_data_size` | 64 KB | 1 MB | Data segment |
| `max_call_depth` | 16 | 64 | Call frame nesting |
| `max_steps_per_tick` | N/A | 1,000,000 | Infinite-loop guard |
| `address_width` | Block16 | Byte32 | 256KB vs 4GB scode |

```rust
// Strict Sedona compatibility
let cfg = VmConfig::sedona_compat();

// Relaxed for BeagleBone (512MB RAM)
let cfg = VmConfig::beaglebone();

// Custom
let cfg = VmConfig {
    max_components: 8192,
    max_call_depth: 128,
    ..VmConfig::default()
};
cfg.validate()?;
```

---

## ComponentStore: Scalable Component Storage

`ComponentStore` replaces Sedona's `App.comps[]` array (which grows by 8
slots at a time with O(n) free-slot scanning) with:

- **Free-list allocation**: O(1) `alloc()` and `free()` using a `Vec<u32>` free list
- **u32 IDs**: Supports up to 4 billion components (vs Sedona's 16-bit limit)
- **Iterative tree walk**: `execution_order()` returns components in
  depth-first order using an explicit stack instead of recursion, preventing
  stack overflow on deep component trees
- **SmallVec children**: Up to 8 children are inlined (no heap allocation)

---

## Bridge: VM-Engine Communication

The bridge module connects the VM to the Sandstar engine:

### ChannelSnapshot (VM reads engine)

A read-optimized `Arc<RwLock<HashMap>>` snapshot of engine channels.
The engine updates it periodically; native methods read channel values,
status, labels, and tags from it without locking the engine.

### SvmWrite / SvmTagWrite (VM writes engine)

Lock-free write queues (`Arc<Mutex<Vec>>`) that buffer VM write requests.
The engine drains these queues each poll cycle and applies the writes.

```rust
// Engine sets up the bridge
set_engine_bridge(snapshot);
set_write_queue(write_queue);
set_tag_write_queue(tag_write_queue);

// Native methods push writes
SvmWrite { channel: 1713, value: 72.5, level: 8 }

// Engine drains
let writes = drain_writes();
let tag_writes = drain_tag_writes();
```

---

## RustSvmRunner: Lifecycle

`RustSvmRunner` provides the high-level lifecycle for the pure Rust VM:

```rust
let mut runner = RustSvmRunner::new("/path/to/kits.scode");

// Load scode, init interpreter, run main method
runner.start()?;

// Each poll cycle: execute the resume method
loop {
    let result = runner.resume()?;
    // result is the return value from the Sedona resume method
    std::thread::sleep(std::time::Duration::from_millis(100));
}

// Shutdown
runner.stop();
```

The FFI-based `SvmRunner` provides the same interface but delegates to the
C interpreter running in a background thread with a yield/hibernate/restart
loop.

---

## Memory Model

### Cell Union

The fundamental stack unit:

```rust
#[repr(C)]
pub union Cell {
    pub ival: i32,   // integer
    pub fval: f32,   // float (bit-reinterpreted)
    pub aval: *mut c_void,  // pointer (FFI mode)
}
```

In the pure Rust VM, the stack uses plain `i32` cells with explicit
`f32::to_bits`/`f32::from_bits` conversions.  This avoids `unsafe` union
access.

### Block Addressing

Sedona uses block indices (16-bit) multiplied by the block size (4 bytes)
to address code-segment entries.  The `VmConfig::address_width` setting
controls whether the 256KB limit is enforced (`Block16`) or relaxed to 4GB
(`Byte32`).

---

## ScodeBuilder: Test Utility

`ScodeBuilder` assembles valid scode images in memory for testing individual
opcodes without a real `.scode` file:

```rust
use sandstar_svm::test_utils::ScodeBuilder;
use sandstar_svm::opcodes::Opcode;

let image = ScodeBuilder::new()
    .op(Opcode::LoadI1)       // push 1
    .op(Opcode::LoadI2)       // push 2
    .op(Opcode::IntAdd)       // pop 2, push 3
    .op(Opcode::ReturnPop)    // return top of stack
    .build();

// Or build directly into VmMemory
let memory = ScodeBuilder::new()
    .op(Opcode::LoadI0)
    .op(Opcode::ReturnPop)
    .data_size(512)
    .build_memory();
```

---

## Test Coverage

The crate has 650+ tests across all modules:

| Module | Tests | What is covered |
|--------|-------|-----------------|
| native_sys | 57 | Sys methods: malloc/free, copy, intStr/floatStr, ticks, compareBytes, Str ops |
| vm_memory | 49 | Code/data reads (u8/u16/u32/i32/u64), bounds, from_image, block addressing |
| vm_stack | 47 | Push/pop (i32, f32, i64, f64, ref), dup/swap/dupdown, overflow/underflow, call frames |
| native_table | 46 | Registration, lookup, stubs, kit iteration, with_defaults |
| native_component | 42 | getBool/getInt/getFloat/getLong, doSetBool/doSetInt, slot resolution chain |
| component_store | 36 | Alloc/free, execution_order, tree walk, capacity, free-list |
| native_inet | 33 | TCP connect/read/write/close, UDP open/bind/send/receive, SHA-1, socket handles |
| vm_interpreter | 26 | Opcode dispatch: all 10 categories, NaN equality, jump targets, method calls |
| bridge | 25 | Snapshot CRUD, write queues, tag writes, channel resolution, FFI safety |
| native_file | 21 | FileStore: open/read/write/seek/close, rename, size, exists, handle lifecycle |
| rust_runner | 19 | Start/stop/resume lifecycle, error paths, restart, drop behavior |
| opcodes | 19 | From u8 conversion, Display, category grouping, roundtrip |
| image_loader | 19 | Header parse/validate, magic/version/block-size, size mismatch, accessor methods |
| vm_error | 17 | Display for all 13 error variants, Error trait, Clone/Eq |
| vm_config | 16 | Default/sedona_compat/beaglebone configs, validation of zero values, Block16 limit |
| test_utils | 16 | ScodeBuilder: op/op_u8/op_u16/op_u32/raw, offset tracking, build/build_memory |
| native_datetime | 11 | doNow (Sedona epoch), doSetClock, getUtcOffset |
| runner | 9 | SvmRunner: not_running, missing file, stop idempotent, drop safety |

Plus 141 integration tests in `tests/interpreter_tests.rs` covering every
arithmetic, comparison, branching, and type-conversion opcode.
