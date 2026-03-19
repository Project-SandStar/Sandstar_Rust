# Sedona VM Rust Porting Strategy

## Document Purpose

This is the companion document to [12_SEDONA_VM_ARCHITECTURE_ANALYSIS.md](12_SEDONA_VM_ARCHITECTURE_ANALYSIS.md). While document 12 analyzes the existing C implementation, this document provides a detailed, actionable strategy for converting the Sedona VM to Rust.

**Scope**: Converting the core VM interpreter (1,281 lines), supporting infrastructure (1,585 lines), and native method implementations (4,880 lines) from C to Rust — approximately 8,926 total lines of C.

---

## Table of Contents

1. [Conversion Decision Matrix](#1-conversion-decision-matrix)
2. [The Cell Type in Rust](#2-the-cell-type-in-rust)
3. [Opcode Dispatch Strategy](#3-opcode-dispatch-strategy)
4. [Stack Implementation](#4-stack-implementation)
5. [Memory Segment Management](#5-memory-segment-management)
6. [Storage Opcodes (The Hard Part)](#6-storage-opcodes-the-hard-part)
7. [Native Method System](#7-native-method-system)
8. [Scode Loader & Validator](#8-scode-loader--validator)
9. [Platform Abstraction](#9-platform-abstraction)
10. [Framework Kit Native Methods](#10-framework-kit-native-methods)
11. [Sedona Compiler Integration](#11-sedona-compiler-integration)
12. [Safety Boundary Design](#12-safety-boundary-design)
13. [Performance Strategy](#13-performance-strategy)
14. [Testing Strategy](#14-testing-strategy)
15. [Migration Phases](#15-migration-phases)
16. [Risk Analysis & Mitigations](#16-risk-analysis--mitigations)
17. [Estimated Effort](#17-estimated-effort)

---

## 1. Conversion Decision Matrix

### What to Convert vs Keep vs Eliminate

| Component | Lines | Decision | Rationale |
|-----------|-------|----------|-----------|
| Core VM interpreter (`vm.c`) | 1,281 | **Convert to Rust** | Heart of the system; benefits from Rust safety at boundaries |
| VM headers (`sedona.h`, `scode.h`, `errorcodes.h`) | 1,585 | **Convert to Rust** | Type definitions become Rust types |
| `sys` kit native methods | 1,744 | **Convert to Rust** | Memory management, string ops, component access |
| `inet` kit native methods | 1,430 | **Convert to Rust** | TCP/UDP → `std::net` or `tokio` |
| `EacIo` kit native (engineio.c) | 1,082 | **Convert to Rust** | Already converting engine to Rust (doc 07) |
| `serial` kit native methods | 542 | **Convert to Rust** | → `serialport` crate |
| `datetimeStd` kit native | 82 | **Convert to Rust** | → `chrono` crate |
| Sedona source (.sedona, ~39K lines) | 39,294 | **Keep as-is** | Compiled to scode by sedonac; unchanged |
| Sedona compiler (Java, ~34K lines) | 34,118 | **Modify minimally** | Generate Rust-compatible native tables |
| Scode binary format | — | **Keep compatible** | Must run existing scode images |

### Why Convert (vs FFI-Only from Doc 06)?

Document 06 recommended keeping the Sedona VM as C with FFI into Rust. Converting the VM itself to Rust provides:

| Benefit | FFI Approach (Doc 06) | Full Rust VM |
|---------|----------------------|--------------|
| Memory safety in interpreter | No (C code) | Partial (safe Rust at boundaries) |
| Single language codebase | Two languages (C + Rust) | One language |
| Debugging experience | Mixed GDB/Rust debugging | Unified Rust tooling |
| Build system | CMake + Cargo | Cargo only |
| Stack overflow detection | Debug-only `#ifdef` | Always-on bounds checking |
| Null pointer protection | Debug-only `#ifdef` | Compile-time `Option<T>` where possible |
| Code maintenance | Two styles | Consistent Rust idioms |

**Trade-off**: The interpreter core is inherently `unsafe` in Rust (raw pointer manipulation). The benefit is not eliminating all `unsafe`, but rather **containing it** within well-tested modules with safe Rust boundaries.

---

## 2. The Cell Type in Rust

### Option A: Rust Union (Recommended)

```rust
/// The fundamental stack unit of the Sedona VM.
/// Must match C `Cell` layout: 32-bit on ARM, 64-bit on x86_64.
#[repr(C)]
#[derive(Clone, Copy)]
pub union Cell {
    pub ival: i32,
    pub fval: f32,
    pub aval: *mut u8,
}

impl Cell {
    pub const ZERO: Cell = Cell { ival: 0 };
    pub const ONE: Cell = Cell { ival: 1 };
    pub const NEG_ONE: Cell = Cell { ival: -1 };
}

// Reading a field from a union requires unsafe in Rust
impl Cell {
    #[inline(always)]
    pub unsafe fn as_int(&self) -> i32 { self.ival }

    #[inline(always)]
    pub unsafe fn as_float(&self) -> f32 { self.fval }

    #[inline(always)]
    pub unsafe fn as_ptr(&self) -> *mut u8 { self.aval }
}
```

**Why union over enum**: An enum-based Cell would add a discriminant tag (1+ bytes), changing `sizeof(Cell)` from 4 to 8 on 32-bit ARM. This would break scode compatibility and double stack memory usage.

### Option B: Newtype with Transmute (Alternative)

```rust
/// Cell as a raw 32-bit value with accessor methods.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Cell(u32);

impl Cell {
    pub fn from_int(v: i32) -> Self { Cell(v as u32) }
    pub fn from_float(v: f32) -> Self { Cell(v.to_bits()) }
    pub fn from_ptr(p: *mut u8) -> Self {
        // Only valid on 32-bit; on 64-bit this needs to be u64
        Cell(p as usize as u32)
    }

    pub fn as_int(self) -> i32 { self.0 as i32 }
    pub fn as_float(self) -> f32 { f32::from_bits(self.0) }
    pub fn as_ptr(self) -> *mut u8 { self.0 as usize as *mut u8 }
}
```

**Problem**: On 64-bit systems, `*mut u8` is 8 bytes but `u32` is 4 bytes. This doesn't work on the development host (x86_64). The union approach naturally handles both via `repr(C)`.

### Recommendation: Use Union

The `#[repr(C)]` union is the correct choice because:
1. Matches the C layout exactly
2. Works on both 32-bit ARM and 64-bit host
3. Zero-cost abstraction (no runtime overhead)
4. All field accesses are already `unsafe` in the interpreter loop anyway

### Null Constants

```rust
pub const NULL_BOOL: i32 = 2;
pub const NULL_FLOAT: u32 = 0x7fc00000;
pub const NULL_DOUBLE: u64 = 0x7ff8000000000000;
```

### NaN Equality

```rust
/// Sedona-specific float comparison: NaN == NaN is TRUE
#[inline(always)]
fn sedona_float_eq(a: f32, b: f32) -> bool {
    if a.is_nan() && b.is_nan() {
        true  // Sedona treats NaN == NaN as true
    } else {
        a == b
    }
}

/// Sedona-specific double comparison: NaN == NaN is TRUE
#[inline(always)]
fn sedona_double_eq(a: f64, b: f64) -> bool {
    if a.is_nan() && b.is_nan() {
        true
    } else {
        a == b
    }
}
```

---

## 3. Opcode Dispatch Strategy

### The Problem

C has computed goto (GCC extension, ~8% faster than switch). Rust has no equivalent. The options:

### Option A: Match Statement (Recommended)

```rust
loop {
    if stop_flag.load(Ordering::Relaxed) {
        return Err(VmError::StopByUser);
    }

    // Bounds check (always-on, unlike C's debug-only check)
    if sp >= max_stack_addr {
        return Err(VmError::StackOverflow);
    }

    let opcode = unsafe { *cp };
    match opcode {
        NOP => { cp = unsafe { cp.add(1) }; }
        LOAD_IM1 => {
            sp = unsafe { sp.add(1) };
            unsafe { (*sp).ival = -1; }
            cp = unsafe { cp.add(1) };
        }
        LOAD_I0 | LOAD_NULL => {
            sp = unsafe { sp.add(1) };
            unsafe { (*sp).ival = 0; }
            cp = unsafe { cp.add(1) };
        }
        // ... all 240 opcodes
        _ => return Err(VmError::UnknownOpcode(opcode)),
    }
}
```

**LLVM typically compiles a dense match with 240 arms into a jump table**, which is equivalent to a computed goto in terms of machine code. The compiler handles the optimization.

### Option B: Function Pointer Dispatch Table

```rust
type OpcodeHandler = unsafe fn(&mut VmState) -> Result<(), VmError>;

static DISPATCH_TABLE: [OpcodeHandler; 240] = [
    op_nop,           // 0
    op_load_im1,      // 1
    op_load_i0,       // 2
    // ...
];

loop {
    let opcode = unsafe { *cp } as usize;
    unsafe { DISPATCH_TABLE[opcode](&mut state)?; }
}
```

**Pros**: Very clean, each opcode is its own function, easy to test individually.
**Cons**: Function call overhead per opcode (push/pop frame), defeats `register` variable optimization. Benchmarks needed.

### Option C: Macro-Generated Match Arms

```rust
macro_rules! opcode_impl {
    ($cp:expr, $sp:expr, NOP) => {
        $cp = unsafe { $cp.add(1) };
    };
    ($cp:expr, $sp:expr, LOAD_I0) => {
        $sp = unsafe { $sp.add(1) };
        unsafe { (*$sp).ival = 0; }
        $cp = unsafe { $cp.add(1) };
    };
    // ... etc
}
```

This generates the same code as Option A but allows defining opcodes separately for readability.

### Recommendation

Start with **Option A (match)** for the initial port. The Rust compiler (LLVM backend) will generate an efficient jump table for a dense 240-case match. If benchmarking shows a performance gap vs C computed goto, try Option B or investigate inline assembly.

### Performance Note: Register Variables

C's `register` hint is ignored by modern compilers anyway. LLVM's register allocator for both C and Rust will make the same decisions. The key variables (`sp`, `cp`, `cb`, etc.) will be kept in registers regardless.

---

## 4. Stack Implementation

### Safe Wrapper Around Raw Stack

```rust
/// Sedona VM stack with bounds checking.
pub struct VmStack {
    /// Base address of stack memory
    base: *mut Cell,
    /// Maximum stack address (for overflow detection)
    max: *mut Cell,
    /// Current stack pointer
    sp: *mut Cell,
    /// Total size in bytes
    size: usize,
}

impl VmStack {
    pub fn new(size_bytes: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(
            size_bytes,
            std::mem::align_of::<Cell>()
        ).unwrap();
        let base = unsafe { std::alloc::alloc_zeroed(layout) as *mut Cell };
        let max = unsafe { base.add(size_bytes / std::mem::size_of::<Cell>()) };
        VmStack { base, max, sp: base, size: size_bytes }
    }

    /// Push a Cell onto the stack. Returns error on overflow.
    #[inline(always)]
    pub unsafe fn push(&mut self, val: Cell) -> Result<(), VmError> {
        self.sp = self.sp.add(1);
        if self.sp >= self.max {
            return Err(VmError::StackOverflow);
        }
        *self.sp = val;
        Ok(())
    }

    /// Pop a Cell from the stack.
    #[inline(always)]
    pub unsafe fn pop(&mut self) -> Cell {
        let val = *self.sp;
        self.sp = self.sp.sub(1);
        val
    }
}

impl Drop for VmStack {
    fn drop(&mut self) {
        let layout = std::alloc::Layout::from_size_align(
            self.size,
            std::mem::align_of::<Cell>()
        ).unwrap();
        unsafe { std::alloc::dealloc(self.base as *mut u8, layout); }
    }
}
```

**Key improvement over C**: Stack overflow checking is always-on (not debug-only). The `#[inline(always)]` ensures no function call overhead.

### 64-bit Value Access

```rust
impl VmStack {
    /// Read a 64-bit value from two consecutive stack cells.
    #[inline(always)]
    pub unsafe fn read_wide(&self, addr: *const Cell) -> i64 {
        *(addr as *const i64)
    }

    /// Write a 64-bit value to two consecutive stack cells.
    #[inline(always)]
    pub unsafe fn write_wide(&self, addr: *mut Cell, val: i64) {
        *(addr as *mut i64) = val;
    }
}
```

---

## 5. Memory Segment Management

### VM State Structure

```rust
/// Complete state of a Sedona VM instance.
pub struct SedonaVm {
    // Memory segments
    code: CodeSegment,
    stack: VmStack,
    static_data: StaticData,

    // Execution state (set during vmCall, not persisted)
    // Note: These are passed as local variables during execution,
    // not stored in the struct, for performance.

    // Configuration
    args: Vec<String>,

    // Callbacks
    on_assert_failure: Option<Box<dyn Fn(&str, u16)>>,

    // Counters
    assert_successes: u32,
    assert_failures: u32,

    // Native method dispatch
    native_table: NativeTable,

    // Control
    stop_flag: Arc<AtomicBool>,
}
```

### Code Segment (Read-Only)

```rust
/// Scode image loaded into memory.
pub struct CodeSegment {
    /// Raw scode bytes (owned)
    data: Vec<u8>,
    /// Block size (always 4 for Sandstar)
    block_size: u8,
    /// Pointer size recorded in scode (4 for ARM, 8 for x86_64)
    ref_size: u8,
}

impl CodeSegment {
    /// Convert block index to memory address.
    #[inline(always)]
    pub fn block_to_addr(&self, block: u16) -> *const u8 {
        unsafe { self.data.as_ptr().add((block as usize) << 2) }
    }

    /// Get the base address of the code segment.
    pub fn base(&self) -> *const u8 {
        self.data.as_ptr()
    }

    /// Get the code size.
    pub fn size(&self) -> usize {
        self.data.len()
    }
}
```

### Static Data Segment

```rust
/// Dynamically allocated static data for all Sedona types.
pub struct StaticData {
    data: Vec<u8>,
}

impl StaticData {
    pub fn new(size: usize) -> Self {
        StaticData { data: vec![0u8; size] }
    }

    pub fn base(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }
}
```

### Improvement: RAII Ownership

In C, the static data segment is `malloc`'d and could be leaked. In Rust, `Vec<u8>` is automatically freed when `StaticData` drops. The `CodeSegment` owns its data via `Vec<u8>` — no manual `free()` needed.

---

## 6. Storage Opcodes (The Hard Part)

### The Challenge

59 storage opcodes perform raw pointer arithmetic. They are the densest `unsafe` code in the entire VM. Example from C:

```c
Case Load32BitFieldU2:
  sp->ival = *(int32_t*)(((uint8_t*)sp->aval) + *(uint16_t*)(cp+1));
  cp += 3;
  EndInstr;
```

### Rust Translation Pattern

```rust
// Direct translation (maximally unsafe)
LOAD_32BIT_FIELD_U2 => {
    let obj = unsafe { (*sp).aval };
    let offset = unsafe { *(cp.add(1) as *const u16) } as usize;
    unsafe { (*sp).ival = *(obj.add(offset) as *const i32) };
    cp = unsafe { cp.add(3) };
}
```

### Helper Functions to Encapsulate Unsafety

```rust
/// Read a u8 offset from the bytecode stream at cp+1.
#[inline(always)]
unsafe fn read_offset_u1(cp: *const u8) -> usize {
    *cp.add(1) as usize
}

/// Read a u16 offset from the bytecode stream at cp+1 (little-endian).
#[inline(always)]
unsafe fn read_offset_u2(cp: *const u8) -> usize {
    *(cp.add(1) as *const u16) as usize
}

/// Read a u32 offset from the bytecode stream at cp+1 (little-endian).
#[inline(always)]
unsafe fn read_offset_u4(cp: *const u8) -> usize {
    *(cp.add(1) as *const u32) as usize
}

/// Read a field from an object at a byte offset.
#[inline(always)]
unsafe fn read_field<T: Copy>(obj: *const u8, offset: usize) -> T {
    *(obj.add(offset) as *const T)
}

/// Write a field to an object at a byte offset.
#[inline(always)]
unsafe fn write_field<T: Copy>(obj: *mut u8, offset: usize, val: T) {
    *(obj.add(offset) as *mut T) = val;
}
```

### Macro for Storage Opcodes

Given the repetitive nature of the 59 storage opcodes, a macro can reduce boilerplate:

```rust
macro_rules! load_field {
    ($sp:expr, $cp:expr, $field_ty:ty, $offset_size:expr, $cp_advance:expr) => {{
        let obj = unsafe { (*$sp).aval };
        let offset = match $offset_size {
            1 => unsafe { *$cp.add(1) as usize },
            2 => unsafe { *($cp.add(1) as *const u16) as usize },
            4 => unsafe { *($cp.add(1) as *const u32) as usize },
            _ => unreachable!(),
        };
        unsafe {
            (*$sp).ival = *(obj.add(offset) as *const $field_ty) as i32;
        }
        $cp = unsafe { $cp.add($cp_advance) };
    }};
}

// Usage:
LOAD_8BIT_FIELD_U1 => load_field!(sp, cp, u8, 1, 2),
LOAD_8BIT_FIELD_U2 => load_field!(sp, cp, u8, 2, 3),
LOAD_8BIT_FIELD_U4 => load_field!(sp, cp, u8, 4, 5),
LOAD_16BIT_FIELD_U1 => load_field!(sp, cp, u16, 1, 2),
// ... etc
```

### Debug-Mode Bounds Checking

Unlike C which only checks in `#ifdef SCODE_DEBUG`, Rust can add bounds checking in debug builds automatically:

```rust
#[cfg(debug_assertions)]
unsafe fn checked_field_read<T: Copy>(obj: *const u8, offset: usize, obj_size: usize) -> T {
    assert!(offset + std::mem::size_of::<T>() <= obj_size,
            "Field read at offset {} exceeds object size {}", offset, obj_size);
    *(obj.add(offset) as *const T)
}
```

---

## 7. Native Method System

### Trait-Based Native Methods

```rust
/// Signature for native methods returning a 32-bit Cell.
pub type NativeMethodFn = unsafe fn(vm: &mut SedonaVm, params: *mut Cell) -> Cell;

/// Signature for native methods returning a 64-bit value.
pub type NativeMethodWideFn = unsafe fn(vm: &mut SedonaVm, params: *mut Cell) -> i64;
```

### Native Table Structure

```rust
/// Dispatch table for native methods.
pub struct NativeTable {
    /// 2D array: kits[kit_id] = &[method_fn_ptr]
    kits: Vec<Vec<NativeMethodFn>>,
}

impl NativeTable {
    /// Look up a native method by kit ID and method ID.
    #[inline(always)]
    pub fn get(&self, kit_id: u8, method_id: u8) -> Option<NativeMethodFn> {
        self.kits
            .get(kit_id as usize)
            .and_then(|kit| kit.get(method_id as usize))
            .copied()
    }

    /// Look up a native method (unchecked, for hot path).
    #[inline(always)]
    pub unsafe fn get_unchecked(&self, kit_id: u8, method_id: u8) -> NativeMethodFn {
        *self.kits
            .get_unchecked(kit_id as usize)
            .get_unchecked(method_id as usize)
    }
}
```

### CallNative Implementation

```rust
CALL_NATIVE => {
    let kit_id = unsafe { *cp.add(1) };
    let method_id = unsafe { *cp.add(2) };
    let num_params = unsafe { *cp.add(3) } as usize;

    #[cfg(debug_assertions)]
    {
        if native_table.get(kit_id, method_id).is_none() {
            return Err(VmError::MissingNative(kit_id, method_id));
        }
    }

    let native = unsafe { native_table.get_unchecked(kit_id, method_id) };

    // Save sp before calling out (native methods may read vm.sp)
    vm.stack.sp = sp;

    // Call native method
    let result = unsafe { native(vm, sp.sub(num_params - 1)) };

    // Pop params, push result
    sp = unsafe { sp.sub(num_params - 1) };
    unsafe { *sp = result; }
    cp = unsafe { cp.add(4) };
}
```

### Example Native Method in Rust

**C original:**
```c
Cell sys_Sys_malloc(SedonaVM* vm, Cell* params) {
    size_t num = (size_t)params[0].ival;
    void* mem = malloc(num);
    if (mem != NULL) memset(mem, 0, num);
    Cell ret;
    ret.aval = mem;
    return ret;
}
```

**Rust port:**
```rust
/// Sedona native: Sys.malloc(int size) -> byte[]
unsafe fn sys_sys_malloc(vm: &mut SedonaVm, params: *mut Cell) -> Cell {
    let num = (*params).ival as usize;
    let layout = match std::alloc::Layout::from_size_align(num, 1) {
        Ok(l) => l,
        Err(_) => return Cell::ZERO,
    };
    let mem = std::alloc::alloc_zeroed(layout);
    if mem.is_null() {
        Cell::ZERO
    } else {
        Cell { aval: mem }
    }
}

/// Sedona native: Sys.free(Obj mem)
unsafe fn sys_sys_free(vm: &mut SedonaVm, params: *mut Cell) -> Cell {
    let mem = (*params).aval;
    if !mem.is_null() {
        // We need to know the size to dealloc properly.
        // In practice, Sedona tracks this via the type's sizeof.
        // For now, we can use a global allocator that tracks sizes.
        // Alternative: Use Box or Vec to track allocation size.
        // This is a known challenge — see Section 12.
    }
    Cell::ZERO
}
```

### The malloc/free Challenge

C's `free()` doesn't need the allocation size. Rust's `dealloc()` does. Solutions:

1. **Use a size-tracking allocator**: Wrap allocations in a HashMap<*mut u8, Layout>
2. **Use `libc::malloc`/`libc::free`**: Call C allocator directly (simplest, maintains compatibility)
3. **Use `Vec<u8>`**: Return `Box::into_raw()` and track sizes

**Recommendation**: Use `libc::malloc`/`libc::free` for Sedona heap allocations. This maintains exact C compatibility and avoids the size-tracking problem.

```rust
unsafe fn sys_sys_malloc(vm: &mut SedonaVm, params: *mut Cell) -> Cell {
    let num = (*params).ival as usize;
    let mem = libc::calloc(1, num); // calloc zeroes memory
    Cell { aval: mem as *mut u8 }
}

unsafe fn sys_sys_free(vm: &mut SedonaVm, params: *mut Cell) -> Cell {
    let mem = (*params).aval;
    if !mem.is_null() {
        libc::free(mem as *mut libc::c_void);
    }
    Cell::ZERO
}
```

---

## 8. Scode Loader & Validator

### Safe Image Validation

```rust
#[derive(Debug)]
pub struct ScodeHeader {
    pub magic: u32,
    pub major_version: u8,
    pub minor_version: u8,
    pub block_size: u8,
    pub ref_size: u8,
    pub code_size: u32,
    pub static_data_size: u32,
    pub main_method: u16,
    pub resume_method: u16,
}

impl ScodeHeader {
    const MAGIC: u32 = 0x5ED0BA07;
    const MAJOR_VERSION: u8 = 1;
    const MINOR_VERSION: u8 = 5;

    pub fn parse(data: &[u8]) -> Result<Self, VmError> {
        if data.len() < 26 {
            return Err(VmError::BadImageMagic);
        }

        let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
        if magic != Self::MAGIC {
            return Err(VmError::BadImageMagic);
        }

        let major = data[4];
        let minor = data[5];
        if major != Self::MAJOR_VERSION || minor != Self::MINOR_VERSION {
            return Err(VmError::BadImageVersion);
        }

        let block_size = data[6];
        if block_size != 4 { // SCODE_BLOCK_SIZE
            return Err(VmError::BadImageBlockSize);
        }

        let ref_size = data[7];
        if ref_size != std::mem::size_of::<*const u8>() as u8 {
            return Err(VmError::BadImageRefSize);
        }

        let code_size = u32::from_le_bytes(data[8..12].try_into().unwrap());
        if code_size as usize != data.len() {
            return Err(VmError::BadImageCodeSize);
        }

        let static_data_size = u32::from_le_bytes(data[12..16].try_into().unwrap());
        let main_method = u16::from_le_bytes(data[16..18].try_into().unwrap());
        let resume_method = u16::from_le_bytes(data[24..26].try_into().unwrap());

        Ok(ScodeHeader {
            magic, major_version: major, minor_version: minor,
            block_size, ref_size, code_size, static_data_size,
            main_method, resume_method,
        })
    }
}
```

**Improvement over C**: The parser uses safe slice indexing and `try_into()`. No raw pointer casts or unchecked reads.

### File Loading

```rust
pub fn load_scode(path: &std::path::Path) -> Result<CodeSegment, VmError> {
    let data = std::fs::read(path).map_err(|_| VmError::MallocImage)?;
    let header = ScodeHeader::parse(&data)?;

    Ok(CodeSegment {
        data,
        block_size: header.block_size,
        ref_size: header.ref_size,
    })
}
```

---

## 9. Platform Abstraction

### Replacing sedona.h Platform Defines

```rust
/// Platform-specific configuration.
/// Replaces the #ifdef _WIN32 / __QNX__ / __UNIX__ blocks in sedona.h.
pub struct PlatformConfig {
    pub endianness: Endianness,
    pub block_size: u8,
    pub pointer_size: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Endianness {
    Little,
    Big,
}

impl PlatformConfig {
    pub fn detect() -> Self {
        PlatformConfig {
            endianness: if cfg!(target_endian = "little") {
                Endianness::Little
            } else {
                Endianness::Big
            },
            block_size: 4,
            pointer_size: std::mem::size_of::<*const u8>() as u8,
        }
    }
}
```

### NaN Macros → Rust Functions

```rust
#[inline(always)]
pub fn is_nan_f32(f: f32) -> bool { f.is_nan() }

#[inline(always)]
pub fn is_nan_f64(d: f64) -> bool { d.is_nan() }
```

### Ticks and Sleep (Platform Service)

**C (sys_Sys_unix.c):**
```c
int64_t sys_Sys_ticks(SedonaVM* vm, Cell* params) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ((int64_t)ts.tv_sec * 1000000000LL) + ts.tv_nsec;
}
```

**Rust:**
```rust
unsafe fn sys_sys_ticks(_vm: &mut SedonaVm, _params: *mut Cell) -> i64 {
    let now = std::time::Instant::now();
    // Return nanoseconds since epoch (matching Sedona convention)
    now.elapsed().as_nanos() as i64  // Or use libc::clock_gettime directly
}
```

---

## 10. Framework Kit Native Methods

### Kit-by-Kit Conversion Plan

#### sys Kit (1,744 lines → ~1,200 Rust)

| C File | Functions | Rust Approach |
|--------|-----------|---------------|
| `sys_Sys.c` | malloc, free, copy, compareBytes, setBytes, floatToBits, etc. | Direct `unsafe` port using `libc` and `std::ptr` |
| `sys_Component.c` | getBool, getInt, getFloat, setBool, setInt, invokeBool, etc. | Port with `read_field`/`write_field` helpers |
| `sys_Type.c` | Type.malloc (allocate + init instance) | `libc::calloc` + `vm.call()` |
| `sys_Test.c` | Test assertion support | Direct port |
| `sys_Str.c` | fromBytes | Trivial pointer arithmetic |
| `sys_Sys_std.c` | intStr, hexStr, longStr, floatStr, doubleStr | `format!()` or `itoa`/`ryu` crates for speed |
| `sys_File_std.c` | File I/O: open, close, read, write, seek | `std::fs::File` wrapped in `unsafe` Cell interface |
| `sys_FileStore_std.c` | File system operations | `std::fs` functions |
| `sys_StdOutStream_std.c` | Console output | `print!()` or `std::io::stdout()` |
| `sys_Sys_unix.c` | ticks, sleep, exit | `std::time`, `std::thread::sleep`, `std::process::exit` |
| `sys_PlatformService_unix.c` | Hibernate support | `std::process::exit` |

#### inet Kit (1,430 lines → ~800 Rust)

| C File | Functions | Rust Approach |
|--------|-----------|---------------|
| `inet_TcpSocket_std.c` | TCP client: connect, read, write, close | `std::net::TcpStream` |
| `inet_TcpServerSocket_std.c` | TCP server: bind, listen, accept | `std::net::TcpListener` |
| `inet_UdpSocket_std.c` | UDP: bind, send, recv | `std::net::UdpSocket` |
| `inet_util_std.c` | IP address parsing, socket options | `std::net::IpAddr` |
| `inet_sha1.c` | SHA1 hash computation | `sha1` crate (much safer) |
| `inet_Crypto_sha1.c` | Crypto wrapper | Thin wrapper over `sha1` crate |

#### serial Kit (542 lines → ~300 Rust)

| C File | Functions | Rust Approach |
|--------|-----------|---------------|
| `serial_SerialPort_default.c` | Serial: open, close, read, write | `serialport` crate |

#### EacIo Kit (1,082 lines → ~600 Rust)

| C File | Functions | Rust Approach |
|--------|-----------|---------------|
| `engineio.c` | IPC with Sandstar engine: read/write channels, metadata | Already designed in doc 07 (IPC Bridge) |

#### datetimeStd Kit (82 lines → ~60 Rust)

| C File | Functions | Rust Approach |
|--------|-----------|---------------|
| `datetimeStd_DateTimeServiceStd.c` | Get/set system time | `chrono` crate |

---

## 11. Sedona Compiler Integration

### Current State

The Sedona compiler (`sedonac`, Java) generates:
1. **Scode bytecode** — Binary format executed by VM
2. **nativetable.c** — C source with native method dispatch arrays

### Required Changes

The scode bytecode format does **not change**. The Rust VM executes the same binary format.

For native method tables, there are two options:

#### Option A: Generate Rust Native Table (Recommended)

Modify `GenNativeTable.java` to optionally emit Rust instead of C:

```rust
// Generated by sedonac (Rust mode)
use crate::native::*;

pub fn build_native_table() -> NativeTable {
    let mut table = NativeTable::new();

    // Kit 0: sys
    table.add_kit(0, vec![
        sys_sys_malloc as NativeMethodFn,
        sys_sys_free as NativeMethodFn,
        sys_sys_copy as NativeMethodFn,
        // ...
    ]);

    // Kit 1: control (no natives)
    table.add_kit(1, vec![]);

    // Kit 2: inet
    table.add_kit(2, vec![
        inet_tcp_socket_connect as NativeMethodFn,
        // ...
    ]);

    table
}
```

**Changes to sedonac**: Add a `--rust` flag to `GenNativeTable.java` that emits Rust module instead of C file. This is a ~50-line change to the Java compiler.

#### Option B: Load Native Table Dynamically

Keep generating `nativetable.c`, compile it as a C shared library, and load it via FFI:

```rust
// Load C native table
let lib = libloading::Library::new("libnativetable.so")?;
let get_table: Symbol<fn() -> *mut *mut NativeMethodFn> = lib.get(b"getNativeTable")?;
```

**Not recommended**: Adds complexity and defeats the purpose of the Rust port.

### Recommendation

**Option A**: Modify `GenNativeTable.java` to emit Rust. This is a small change (~50 lines of Java) and produces a clean, type-safe native table.

---

## 12. Safety Boundary Design

### The Unsafe Core

The interpreter loop is inherently `unsafe`. Rather than pretending it can be safe, the strategy is to **isolate unsafety** into a minimal core with safe Rust boundaries.

### Module Architecture

```
sedona_vm/
├── src/
│   ├── lib.rs              ← Public API (safe)
│   ├── vm.rs               ← SedonaVm struct (safe interface)
│   ├── cell.rs             ← Cell type (repr(C) union)
│   ├── stack.rs            ← VmStack (safe wrapper, unsafe internals)
│   ├── code.rs             ← CodeSegment (safe scode parser)
│   ├── static_data.rs      ← StaticData (safe RAII wrapper)
│   ├── native_table.rs     ← NativeTable (safe dispatch)
│   ├── error.rs            ← VmError enum
│   ├── opcodes.rs          ← Opcode constants
│   ├── interpreter.rs      ← THE UNSAFE CORE: vmCall loop
│   ├── getters.rs          ← Field accessor helpers (unsafe)
│   ├── debug.rs            ← Debug/diagnostic (safe where possible)
│   └── native/             ← Native method implementations
│       ├── mod.rs
│       ├── sys.rs           ← sys kit natives
│       ├── sys_component.rs ← Component field access
│       ├── sys_file.rs      ← File I/O
│       ├── inet.rs          ← TCP/UDP/SHA1
│       ├── serial.rs        ← Serial port
│       ├── eacio.rs         ← Engine I/O bridge
│       └── datetime.rs      ← Date/time
├── Cargo.toml
└── tests/
    ├── interpreter_tests.rs
    ├── stack_tests.rs
    └── native_tests.rs
```

### Safety Invariants

The `unsafe` code in `interpreter.rs` relies on these invariants:

1. **Code segment is valid scode**: Validated by `ScodeHeader::parse()` before execution
2. **Stack is properly sized**: Allocated by `VmStack::new()` with overflow checking
3. **Native table is complete**: Built by `build_native_table()` covering all kit/method IDs
4. **Block indices are within code bounds**: Validated lazily (out-of-bounds access will segfault)
5. **Object pointers are non-null**: Checked in debug builds via `OpcodePointerOffsets`

### Where Safety Is Gained

Even though the interpreter core is `unsafe`, the Rust port improves safety in:

| Area | C Behavior | Rust Behavior |
|------|-----------|---------------|
| Stack overflow | Debug-only check | Always checked (configurable) |
| Null pointers | Debug-only check | Always checked in debug, optional in release |
| VM lifetime | Manual malloc/free | RAII Drop trait |
| String formatting | sprintf to static buffer | `format!()` — safe, no buffer overflow |
| File I/O | Manual fd tracking | `std::fs::File` with Drop |
| Socket I/O | Manual fd tracking | `std::net` types with Drop |
| Native table bounds | Unchecked in release | Checked in debug, unchecked in release |
| Stop flag | Non-atomic global | `AtomicBool` with proper ordering |
| Thread safety | Not addressed | `Send`/`Sync` on `SedonaVm` |

---

## 13. Performance Strategy

### Benchmark Targets

The Rust VM must match or beat C VM performance. Key metrics:

| Metric | C Baseline | Rust Target |
|--------|-----------|-------------|
| Opcode dispatch | ~5-10 ns/opcode | ≤ 10 ns/opcode |
| Native call overhead | ~20 ns | ≤ 25 ns |
| Stack push/pop | ~1 ns | ≤ 2 ns |
| Scode load time | ~1 ms | ≤ 2 ms |
| Memory overhead | 0 bytes (same as C) | ≤ 64 bytes (Vec metadata) |

### Optimization Techniques

1. **`#[inline(always)]`** on all opcode helpers and Cell accessors
2. **Dense match → jump table**: LLVM generates jump tables for dense integer matches
3. **Profile-guided optimization (PGO)**: Build with `cargo pgo` using real scode workloads
4. **Link-time optimization (LTO)**: `lto = true` in `Cargo.toml` release profile
5. **Target-specific features**: `target-cpu=cortex-a8` for ARM NEON and scheduling

### Cargo.toml Release Profile

```toml
[profile.release]
opt-level = 3
lto = true
codegen-units = 1
panic = "abort"         # No unwinding overhead on embedded
strip = true            # Minimize binary size

[profile.release.build-override]
opt-level = 3
```

### Computed Goto Alternative: Direct Threaded Code

If the match-based dispatch is measurably slower than C's computed goto, consider:

```rust
// Decode scode into a pre-resolved jump table at load time
struct DecodedOp {
    handler: unsafe fn(&mut VmState),
    // operands pre-decoded
}

// At load time, convert bytecode to DecodedOp array
// At runtime, iterate the array instead of decoding bytecodes
```

This is a form of **direct threaded code** — more memory but potentially faster than bytecode interpretation. Only implement if benchmarks show a need.

---

## 14. Testing Strategy

### Unit Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cell_size() {
        // Cell must be exactly pointer-sized
        assert_eq!(std::mem::size_of::<Cell>(), std::mem::size_of::<*const u8>());
    }

    #[test]
    fn test_cell_union_int() {
        let c = Cell { ival: 42 };
        assert_eq!(unsafe { c.ival }, 42);
    }

    #[test]
    fn test_cell_union_float() {
        let c = Cell { fval: 3.14 };
        assert!((unsafe { c.fval } - 3.14).abs() < 0.001);
    }

    #[test]
    fn test_nan_equality() {
        assert!(sedona_float_eq(f32::NAN, f32::NAN));      // Sedona: true
        assert!(!sedona_float_eq(f32::NAN, 1.0));           // false
        assert!(sedona_float_eq(1.0, 1.0));                 // true
    }

    #[test]
    fn test_scode_header_parse() {
        let mut data = vec![0u8; 256];
        // Write magic
        data[0..4].copy_from_slice(&0x5ED0BA07u32.to_le_bytes());
        data[4] = 1; // major
        data[5] = 5; // minor
        data[6] = 4; // block size
        data[7] = std::mem::size_of::<*const u8>() as u8;
        data[8..12].copy_from_slice(&256u32.to_le_bytes());
        data[12..16].copy_from_slice(&1024u32.to_le_bytes());

        let header = ScodeHeader::parse(&data).unwrap();
        assert_eq!(header.magic, 0x5ED0BA07);
        assert_eq!(header.static_data_size, 1024);
    }

    #[test]
    fn test_stack_push_pop() {
        let mut stack = VmStack::new(4096);
        unsafe {
            stack.push(Cell { ival: 42 }).unwrap();
            let val = stack.pop();
            assert_eq!(val.ival, 42);
        }
    }

    #[test]
    fn test_stack_overflow() {
        let mut stack = VmStack::new(64); // Very small stack
        for i in 0..100 {
            let result = unsafe { stack.push(Cell { ival: i }) };
            if result.is_err() {
                assert!(i > 0); // Should overflow after some pushes
                return;
            }
        }
        panic!("Expected stack overflow");
    }
}
```

### Integration Tests: Run Existing Scode

The most critical test is running existing scode images on the Rust VM and comparing results with the C VM:

```rust
#[test]
fn test_run_existing_scode() {
    let scode = load_scode(Path::new("test_data/app.scode")).unwrap();
    let mut vm = SedonaVm::new(scode, 64 * 1024);
    let result = vm.run();
    assert!(result.is_ok());
}
```

### Fuzzing

The scode parser and interpreter are prime targets for fuzzing:

```rust
// cargo-fuzz target
fuzz_target!(|data: &[u8]| {
    if let Ok(code) = CodeSegment::try_from(data) {
        let mut vm = SedonaVm::new(code, 8192);
        let _ = vm.run(); // Should not crash, only return errors
    }
});
```

### Comparison Testing

Run the same Sedona application on both C and Rust VMs, compare:
- Final stack state
- Assert success/failure counts
- Native method call sequence
- Return values

---

## 15. Migration Phases

### Phase 1: Skeleton VM (2-3 weeks)

**Goal**: Rust VM that loads scode and executes simple bytecode (no native methods).

**Deliverables**:
- `Cell` type with `repr(C)` union
- `ScodeHeader` parser with validation
- `VmStack` with overflow checking
- `StaticData` segment
- `CodeSegment` with `block_to_addr`
- `vmCall` loop with ~60 core opcodes:
  - Literals (29 opcodes)
  - Parameters (8)
  - Locals (20)
  - Stack manipulation (9)
  - Near/far jumps (8)
  - Int arithmetic (20) — most commonly used

**Test**: Simple Sedona programs that only do integer math and control flow.

### Phase 2: Arithmetic & Casting (1-2 weeks)

**Goal**: Complete numeric operations.

**Deliverables**:
- Long ops (18 opcodes) with 2-Cell wide handling
- Float ops (11 opcodes) with NaN semantics
- Double ops (11 opcodes) with NaN semantics
- Type casts (12 opcodes)
- Object compare (2 opcodes)
- General compare (2 opcodes)

**Test**: Sedona programs with float/double math, type conversions.

### Phase 3: Storage Opcodes (2-3 weeks)

**Goal**: Field and array access.

**Deliverables**:
- All 59 storage opcodes (8/16/32/64-bit fields, arrays, refs, const, inline)
- Field read/write helper functions
- Macro-generated opcode implementations

**Test**: Sedona programs that access component properties.

### Phase 4: Method Calling (1-2 weeks)

**Goal**: Complete calling convention.

**Deliverables**:
- `Call` opcode with frame setup
- `CallVirtual` with vtable dispatch
- `ReturnPop`, `ReturnPopWide`, `ReturnVoid`
- `LoadParam0Call` optimization
- Misc opcodes: `InitArray`, `InitVirt`, `InitComp`, `Assert`, `Switch`, `MetaSlot`

**Test**: Multi-method Sedona programs with inheritance.

### Phase 5: sys Kit Natives (2-3 weeks)

**Goal**: Core system native methods.

**Deliverables**:
- `sys_Sys`: malloc, free, copy, string formatting, ticks, sleep
- `sys_Component`: Get/set fields by slot, invoke actions
- `sys_Type`: Type.malloc (allocate instances)
- `sys_File_std`: File I/O
- `sys_FileStore_std`: File system ops
- Platform-specific: Unix ticks, sleep, platform service

**Test**: Full Sedona `sys` kit tests pass.

### Phase 6: Network & I/O Natives (2-3 weeks)

**Goal**: Networking and serial native methods.

**Deliverables**:
- `inet`: TCP client/server, UDP, SHA1
- `serial`: Serial port via `serialport` crate
- `datetimeStd`: Date/time via `chrono`

**Test**: SOX (Sedona Object Exchange) communication works.

### Phase 7: EacIo Integration (1-2 weeks)

**Goal**: Connect to Sandstar engine via IPC.

**Deliverables**:
- `engineio.rs`: Port IPC bridge (leverages doc 07 work)
- Native table generation (modify sedonac or hand-write)

**Test**: Full Sandstar system runs with Rust VM.

### Phase 8: Optimization & Hardening (2-3 weeks)

**Goal**: Match C VM performance, production-ready.

**Deliverables**:
- PGO build with real workload
- Benchmark suite comparing Rust vs C VM
- Fuzz testing
- ARM cross-compilation validation
- Stack overflow / null pointer handling in release builds

### Total Estimated Timeline: 14-21 weeks

---

## 16. Risk Analysis & Mitigations

### Risk 1: Performance Regression

**Probability**: Medium (30%)
**Impact**: High — VM runs control loops at 100ms-1s intervals
**Mitigation**:
- LLVM generates efficient jump tables for dense match
- `#[inline(always)]` on hot-path functions
- PGO + LTO in release builds
- If still slow, implement direct threaded code
- **Fallback**: Keep C VM as option, Rust VM as experimental

### Risk 2: Scode Compatibility Bugs

**Probability**: Medium (40%)
**Impact**: High — Subtle behavior differences can cause control logic errors
**Mitigation**:
- Comparison testing: Run identical scode on both C and Rust VMs
- Test with production Sandstar scode images
- Byte-for-byte verification of stack states
- NaN equality behavior explicitly tested

### Risk 3: Cross-Compilation Issues

**Probability**: Low (15%)
**Impact**: Medium — Must run on ARM Cortex-A8
**Mitigation**:
- `armv7-unknown-linux-gnueabihf` is Rust Tier 1 target
- Cell union `repr(C)` guarantees same layout as C
- Use `libc` crate for POSIX functions
- Test on actual BeagleBone hardware early (Phase 5)

### Risk 4: malloc/free Compatibility

**Probability**: Medium (25%)
**Impact**: Medium — Sedona apps allocate during init
**Mitigation**:
- Use `libc::malloc`/`libc::free` for Sedona heap allocations
- Do not mix Rust allocator with Sedona allocator
- Track allocations if needed for debugging

### Risk 5: Unsafe Code Bugs

**Probability**: High (50%)
**Impact**: High — Undefined behavior in interpreter
**Mitigation**:
- Miri (`cargo miri`) for detecting UB in tests
- AddressSanitizer via `-Zsanitizer=address`
- Extensive unit tests for each opcode
- Fuzz testing with random scode
- Code review focused on pointer arithmetic

### Risk 6: Native Method Coverage

**Probability**: Low (10%)
**Impact**: Medium — Missing native causes VM error
**Mitigation**:
- `GenNativeTable.java` already enumerates all required natives
- Stub missing natives with `unimplemented!()` for early detection
- Port natives incrementally, test each kit separately

---

## 17. Estimated Effort

### Line Count Estimates

| Component | C Lines | Estimated Rust Lines | Rationale |
|-----------|---------|---------------------|-----------|
| `interpreter.rs` (vmCall loop) | 900 | 1,100 | Match arms more verbose; helper fns |
| `cell.rs` | 50 | 80 | Union + impl methods |
| `stack.rs` | 20 | 100 | Safe wrapper + overflow checks |
| `code.rs` (scode loader) | 50 | 120 | Safe parser with validation |
| `static_data.rs` | 10 | 30 | RAII wrapper |
| `native_table.rs` | 30 | 80 | Safe dispatch table |
| `error.rs` | 70 | 60 | Rust enum vs C defines |
| `opcodes.rs` | 300 | 250 | Constants (simpler in Rust) |
| `getters.rs` | 100 | 120 | Field accessors |
| `debug.rs` | 80 | 100 | Debug/diagnostic |
| `vm.rs` (public API) | 50 | 150 | Init, run, resume, config |
| `native/sys.rs` | 500 | 400 | malloc/free/copy/format |
| `native/sys_component.rs` | 434 | 350 | Component field access |
| `native/sys_file.rs` | 760 | 500 | File I/O → `std::fs` |
| `native/inet.rs` | 1,430 | 800 | TCP/UDP/SHA1 → `std::net` + `sha1` |
| `native/serial.rs` | 542 | 300 | → `serialport` crate |
| `native/eacio.rs` | 1,082 | 600 | IPC bridge (from doc 07) |
| `native/datetime.rs` | 82 | 60 | → `chrono` |
| Tests | 0 | 800 | Unit + integration tests |
| **TOTAL** | **~6,490** | **~5,800** | **~11% reduction** |

### Effort by Phase

| Phase | Duration | FTE Weeks |
|-------|----------|-----------|
| 1. Skeleton VM | 2-3 weeks | 2-3 |
| 2. Arithmetic & Casting | 1-2 weeks | 1-2 |
| 3. Storage Opcodes | 2-3 weeks | 2-3 |
| 4. Method Calling | 1-2 weeks | 1-2 |
| 5. sys Kit Natives | 2-3 weeks | 2-3 |
| 6. Network & I/O Natives | 2-3 weeks | 2-3 |
| 7. EacIo Integration | 1-2 weeks | 1-2 |
| 8. Optimization & Hardening | 2-3 weeks | 2-3 |
| **TOTAL** | **14-21 weeks** | **14-21** |

### Dependencies

```
Cargo.toml dependencies for sedona_vm:

[dependencies]
libc = "0.2"              # malloc/free, POSIX functions
serialport = "4"          # Serial port access
sha1 = "0.10"             # SHA1 for inet kit
chrono = "0.4"            # Date/time for datetimeStd kit

[dev-dependencies]
criterion = "0.5"         # Benchmarking
```

---

## Summary: Should You Do It?

### Arguments For

1. **Single-language codebase**: Eliminate C/Rust boundary complexity
2. **Unified tooling**: One build system (Cargo), one debugger, one profiler
3. **Always-on safety**: Stack overflow and null pointer checks in release builds
4. **RAII everywhere**: No memory leaks from forgotten `free()` calls
5. **Better testing**: Rust's test infrastructure far superior to C's
6. **Fuzzing**: `cargo-fuzz` can find scode bugs that manual testing misses
7. **AtomicBool for stop flag**: Eliminates data race in `gStopByUser`
8. **Modern networking**: `std::net` replaces raw socket code

### Arguments Against

1. **Heavy `unsafe` usage**: The interpreter core will be ~80% `unsafe` blocks
2. **Performance risk**: Match-based dispatch may be slower than computed goto
3. **14-21 weeks of effort**: Significant investment for a working VM
4. **Stable C code**: The VM has been running for years with few bugs
5. **Team learning curve**: Requires Rust expertise, especially `unsafe` Rust
6. **Two porting fronts**: This is in addition to the ~27K line Sandstar migration

### Recommendation

**The VM conversion is a Phase 3+ task** — do it only after the main Sandstar migration (docs 00-11) is complete and stable. The Sandstar engine, Haystack REST API, and IPC bridge benefit far more from Rust's safety guarantees than the VM interpreter does.

If the main migration succeeds and the team has bandwidth, the VM conversion is worthwhile for the single-language codebase benefit alone. Start with Phase 1 (skeleton VM) as a proof of concept to validate performance before committing to the full 14-21 week effort.

### Priority Ordering

```
Priority 1 (Do first):  Sandstar Engine + REST API → Rust (docs 00-11)
Priority 2 (Do second): Sedona VM FFI into Rust (doc 06)
Priority 3 (Optional):  Sedona VM itself → Rust (this document + doc 12)
Priority 4 (Future):    Sedona compiler Rust mode (modify sedonac)
```
