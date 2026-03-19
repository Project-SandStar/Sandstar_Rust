# Sedona VM Architecture: Deep Analysis for Rust Conversion

## Document Purpose

This document provides a comprehensive analysis of the Sedona Virtual Machine as implemented in the Sandstar codebase. Unlike the previous 12 research documents (00–11), which assumed the Sedona VM would **stay as C** with FFI into Rust, this document analyzes what it would actually take to **rewrite the Sedona VM itself in Rust**.

**Related document:** [13_SEDONA_VM_RUST_PORTING_STRATEGY.md](13_SEDONA_VM_RUST_PORTING_STRATEGY.md) — the companion Rust conversion strategy.

---

## Table of Contents

1. [Codebase Inventory](#1-codebase-inventory)
2. [Core VM Architecture](#2-core-vm-architecture)
3. [The Cell Type System](#3-the-cell-type-system)
4. [Scode Binary Format](#4-scode-binary-format)
5. [Instruction Set Architecture (240 Opcodes)](#5-instruction-set-architecture-240-opcodes)
6. [Stack Frame Layout & Calling Convention](#6-stack-frame-layout--calling-convention)
7. [Native Method Dispatch](#7-native-method-dispatch)
8. [Memory Model](#8-memory-model)
9. [Computed Goto Optimization](#9-computed-goto-optimization)
10. [Platform Abstraction Layer](#10-platform-abstraction-layer)
11. [Framework Kits & Sedona Language](#11-framework-kits--sedona-language)
12. [Sedona Compiler (sedonac)](#12-sedona-compiler-sedonac)
13. [Unsafe Patterns Catalog](#13-unsafe-patterns-catalog)
14. [Key Challenges for Rust Conversion](#14-key-challenges-for-rust-conversion)

---

## 1. Codebase Inventory

### Source File Breakdown

| Component | Files | Lines | Language |
|-----------|-------|-------|----------|
| **Core VM** | 4 | 2,866 | C |
| `vm.c` | 1 | 1,281 | C |
| `sedona.h` | 1 | 430 | C/H |
| `scode.h` | 1 | 1,085 | C/H |
| `errorcodes.h` | 1 | 70 | C/H |
| **Native Method Implementations** | 24 | 4,880 | C |
| `sys` kit natives | 14 | 1,744 | C |
| `inet` kit natives | 5 | 1,430 | C |
| `EacIo` kit natives (engineio.c) | 1 | 1,082 | C |
| `serial` kit natives | 2 | 542 | C |
| `datetimeStd` kit natives | 1 | 82 | C |
| **Platform natives** | 1 | 38 | C |
| **Total C/H in EacIo/src** | ~30 | **8,926** | C |
| **Sedona Language Source (.sedona)** | ~200+ | **39,294** | Sedona |
| `sys` kit | — | 10,050 | Sedona |
| `control` kit | — | 7,132 | Sedona |
| `sox` kit | — | 3,015 | Sedona |
| `EacIo` kit | — | 2,777 | Sedona |
| `func` kit | — | 2,146 | Sedona |
| `web` kit | — | 2,058 | Sedona |
| `logic` kit | — | 1,377 | Sedona |
| `math` kit | — | 1,302 | Sedona |
| `inet` kit | — | 1,120 | Sedona |
| `hvac` kit | — | 850 | Sedona |
| `types` kit | — | 816 | Sedona |
| `serial` kit | — | 460 | Sedona |
| Other kits | — | ~6,191 | Sedona |
| **Sedona Compiler (sedonac)** | ~100+ | **34,118** | Java |
| **Total "Sedona VM system"** | — | **~82,338** | Mixed |

### What "100K lines" Really Means

The commonly cited "~100K lines" for the Sedona VM is the sum of:
- Core VM C code: 8,926 lines
- Sedona language framework: 39,294 lines
- Sedona compiler (Java): 34,118 lines
- Total: ~82,338 lines (close to 100K with platform-specific files)

**The core interpreter itself is only 1,281 lines of C** (`vm.c`). This is crucial for scoping the Rust conversion.

---

## 2. Core VM Architecture

### Overview

The Sedona VM is a **stack-based bytecode interpreter** designed for embedded systems. It was created by Tridium Inc. for building automation (HVAC, lighting, fire/life safety) and licensed under the Academic Free License 3.0.

### Design Principles

1. **Minimal footprint** — Runs on devices with as little as 32KB RAM
2. **Deterministic execution** — No garbage collector; explicit malloc/free
3. **Platform independence** — Same scode runs on Windows, Linux, QNX, bare metal
4. **Native method extensibility** — C functions callable from Sedona bytecode
5. **Component-based** — First-class support for the component programming model

### Architecture Diagram

```
┌─────────────────────────────────────────────────┐
│                 Sedona Application                │
│         (.sedona source → .scode bytecode)        │
├─────────────────────────────────────────────────┤
│                  Sedona VM (vm.c)                 │
│  ┌──────────┐  ┌──────────┐  ┌──────────────┐  │
│  │ Code Seg │  │Stack Seg │  │ Static Data  │  │
│  │ (scode)  │  │          │  │   Segment    │  │
│  └──────────┘  └──────────┘  └──────────────┘  │
│           │                                      │
│  ┌────────▼────────────────────────────────┐    │
│  │         Opcode Dispatch Loop             │    │
│  │  (switch/case or computed goto)          │    │
│  └────────┬────────────────────────────────┘    │
│           │                                      │
│  ┌────────▼────────────────────────────────┐    │
│  │     Native Method Table (2D array)       │    │
│  │  nativeTable[kitId][methodId] → fn ptr   │    │
│  └──────────────────────────────────────────┘   │
├─────────────────────────────────────────────────┤
│          Native Method Implementations           │
│  ┌──────┐ ┌──────┐ ┌──────┐ ┌──────┐ ┌──────┐ │
│  │ sys  │ │ inet │ │serial│ │EacIo │ │ plat │ │
│  └──────┘ └──────┘ └──────┘ └──────┘ └──────┘ │
├─────────────────────────────────────────────────┤
│           Operating System (Linux/QNX/Win32)     │
└─────────────────────────────────────────────────┘
```

### Entry Points

```c
// Primary entry: initialize VM and run main method
int vmRun(SedonaVM* vm);

// Resume after hibernate/yield
int vmResume(SedonaVM* vm);

// Call a specific method (the main dispatch loop)
int vmCall(SedonaVM* vm, uint16_t method, Cell* args, int argc);

// Signal VM to stop
void stopVm();
```

`vmRun()` calls `vmInit()` then `vmEntry(vm, 16)` (offset 16 = main method block index in scode header).
`vmResume()` calls `vmEntry(vm, 24)` (offset 24 = resume method block index).
`vmEntry()` sets up the initial stack frame and calls `vmCall()`.

---

## 3. The Cell Type System

### The Cell Union

The fundamental data type in the Sedona VM is the `Cell` — a 32-bit union:

```c
typedef union {
  int32_t ival;    // 32-bit signed int (also used for bool, byte, short)
  float   fval;    // 32-bit float
  void*   aval;    // address pointer (32-bit on ARM, 64-bit on x86_64)
} Cell;
```

**Critical detail**: On a 32-bit system (BeagleBone ARM), `sizeof(Cell) == 4`. On a 64-bit system, `sizeof(Cell) == 8` because `void*` is 8 bytes. This affects the entire stack layout.

### Cell Constants

```c
Cell zeroCell;       // {.ival = 0} — also used as false, NULL, 0.0f
Cell oneCell;        // {.ival = 1} — also used as true
Cell negOneCell;     // {.ival = -1}
```

### Primitive Type IDs

```c
#define VoidTypeId    0
#define BoolTypeId    1
#define ByteTypeId    2
#define ShortTypeId   3
#define IntTypeId     4
#define LongTypeId    5    // 64-bit, occupies 2 stack slots
#define FloatTypeId   6
#define DoubleTypeId  7    // 64-bit, occupies 2 stack slots
#define BufTypeId     8    // byte buffer (pointer)
```

### 64-bit Value Handling

Long and double values occupy **two consecutive Cell stack slots**. This is because each Cell is 32 bits on the target platform:

```c
// Loading a long 0:
Case LoadL0: ++sp; *((int64_t*)sp) = 0; ++sp; ++cp; EndInstr;
//           ^-- first slot      ^-- second slot (sp now points here)

// Long addition:
Case LongAdd: sp -= 2; *(int64_t*)(sp-1) = *(int64_t*)(sp-1) + *(int64_t*)(sp+1); ++cp; EndInstr;
```

### Null Value Encoding

Sedona has special null representations:
```c
#define NULLBOOL   2                          // bool null = 2 (not 0 or 1)
#define NULLFLOAT  0x7fc00000                 // IEEE 754 quiet NaN
#define NULLDOUBLE 0x7ff8000000000000ll       // IEEE 754 quiet NaN (double)
```

### NaN Semantics (Sedona-Specific)

Sedona deviates from IEEE 754: **NaN == NaN evaluates to TRUE** (not false as in standard IEEE):

```c
Case FloatEq:
  --sp;
  if (ISNANF(sp->fval) && ISNANF((sp+1)->fval))   // special case for Sedona
    sp->ival = TRUE;
  else
    sp->ival = sp->fval == (sp+1)->fval;
  ++cp;
  EndInstr;
```

This is critical behavior that must be preserved in any Rust port.

---

## 4. Scode Binary Format

### Image Header (First 16 bytes)

```
Offset  Size  Field
------  ----  -----
0       4     Magic number: 0x5ED0BA07
4       1     Major version: 1
5       1     Minor version: 5
6       1     Block size: 4 (SCODE_BLOCK_SIZE)
7       1     Reference/pointer size: sizeof(void*) — 4 on ARM
8       4     Code image size (bytes)
12      4     Static data size (bytes, allocated via malloc at init)
16      2     Main method block index
18      ...   (padding/reserved)
24      2     Resume method block index
```

### Block Addressing

The scode uses a **block-based addressing scheme**. The entire code section is divided into blocks of `SCODE_BLOCK_SIZE` (4 bytes). A 16-bit block index can address up to 2^16 × 4 = 256KB of code.

```c
#define SCODE_BLOCK_SIZE 4
#define block2addr(cb, block) ((cb) + (block << 2))
```

Every method, type, kit, slot, and constant string is addressed by its 16-bit block index within the scode image.

### Method Encoding

Each method in scode starts with:
```
Byte 0: numParams (uint8_t) — number of parameter words
Byte 1: numLocals (uint8_t) — number of local variable words
Byte 2+: opcodes...
```

### Type Metadata (in scode)

Types are encoded as pairs of block indices:
```
Offset 0: typeId (uint8_t)
Offset 2: typeName (block index → string)
Offset 4: kit (block index → kit struct)
Offset 6: base type (block index → parent type)
Offset 8: sizeof (uint16_t) — instance size in bytes
Offset 10: init method (uint16_t → block index)
```

### Virtual Method Tables

Each object's first field (at offset 0) is a block index pointing to its vtable. The vtable is an array of uint16_t block indices, one per virtual method.

---

## 5. Instruction Set Architecture (240 Opcodes)

### Opcode Categories

| Category | Range | Count | Description |
|----------|-------|-------|-------------|
| Literals | 0–28 | 29 | Load constants: ints, longs, floats, doubles, null, strings, types |
| Parameters | 29–36 | 8 | Load/store method parameters |
| Locals | 37–56 | 20 | Load/store local variables |
| Int ops | 57–76 | 20 | Integer compare, arithmetic, bitwise |
| Long ops | 77–94 | 18 | 64-bit integer operations |
| Float ops | 95–105 | 11 | 32-bit float compare and arithmetic |
| Double ops | 106–116 | 11 | 64-bit double compare and arithmetic |
| Casts | 117–128 | 12 | Type conversions between int/long/float/double |
| Object compare | 129–130 | 2 | Pointer equality |
| General compare | 131–132 | 2 | EqZero, NotEqZero |
| Stack manipulation | 133–141 | 9 | Pop, Dup, Dup variations |
| Near jumps | 142–145 | 4 | 8-bit relative jumps |
| Far jumps | 146–149 | 4 | 16-bit relative jumps |
| Int compare jumps (near) | 150–155 | 6 | Combined compare-and-jump |
| Int compare jumps (far) | 156–161 | 6 | Combined compare-and-jump (16-bit) |
| Storage | 162–220 | 59 | Field/array load/store (8/16/32/64-bit, ref, const, inline) |
| Method calls | 221–229 | 9 | Call, CallVirtual, CallNative, Return |
| Misc | 230–239 | 10 | InitArray, InitVirt, Assert, Switch, etc. |

### Opcode Encoding

Most opcodes are single-byte. Some take inline operands:

```
Single byte:     [opcode]
With u1 arg:     [opcode] [u8]
With u2 arg:     [opcode] [u16le]      — little-endian on ARM
With u4 arg:     [opcode] [u32le]
Call:            [opcode] [u16 block] [u8 numParams]
CallNative:      [opcode] [u8 kitId] [u8 methodId] [u8 numParams]
Switch:          [opcode] [u16 count] [u16 jump0] ... [u16 jumpN]
```

### Storage Opcodes (Most Complex)

The storage opcodes (162–220) are the most numerous. They handle field access at different sizes and with different offset encodings:

- **Load/Store 8-bit** (byte/bool): `Load8BitFieldU1/U2/U4`, `Load8BitArray`, `Store8BitFieldU1/U2/U4`, `Store8BitArray`
- **Load/Store 16-bit** (short): Same pattern
- **Load/Store 32-bit** (int/float): Same pattern
- **Load/Store 64-bit** (long/double): Same pattern
- **Load/Store Ref** (pointers): Same pattern, width matches `sizeof(void*)`
- **Load Const** (block index → address): `LoadConstFieldU1/U2`, `LoadConstArray`, `LoadConstStatic`
- **Load Inline** (address of embedded object): `LoadInlineFieldU1/U2/U4`
- **Load Param0 Inline**: Optimized `this.field` access
- **Load Data Inline**: Static field access via data base pointer

The `U1/U2/U4` suffix indicates the encoding width of the field offset:
- `U1`: offset fits in uint8_t (0–255)
- `U2`: offset fits in uint16_t (0–65535)
- `U4`: offset fits in uint32_t

### Opcode Implementation Pattern

Each opcode follows a tight pattern in `vm.c`:

```c
Case LoadIntU1:     (++sp)->ival = *(cp+1);            cp += 2; EndInstr;
Case LoadIntU2:     (++sp)->ival = *(uint16_t*)(cp+1); cp += 3; EndInstr;
```

Where:
- `sp` is the stack pointer (pre-increment push)
- `cp` is the code pointer (advanced past opcode + operands)
- `EndInstr` is either `continue` (switch) or `goto nextInstr` (computed goto)

---

## 6. Stack Frame Layout & Calling Convention

### Frame Structure

```
            ┌──────────────┐  ← sp (top of stack)
            │ stack temp 2 │
            │ stack temp 1 │
            │ stack temp 0 │
            │ local n      │  ← sp at start of call
            │ local 1      │
            │ local 0      │  ← lp (local pointer)
            │ method addr  │  ← fp+2 (pointer to method bytecode)
            │ prev fp      │  ← fp+1 (previous frame pointer, or NULL)
            │ return cp    │  ← fp (frame pointer; return address)
            │ param n      │
            │ param 1      │
            │ param 0      │  ← pp (param pointer)
            └──────────────┘
```

### Call Sequence (non-virtual)

```c
Case Call:
  u2 = *(uint16_t*)(cp+1);      // 1. Read target method block index
  addr = block2addr(cb, u2);     // 2. Convert to memory address
  (++sp)->aval = cp+3;           // 3. Push return address (next instruction)
call:
  (++sp)->aval = fp;             // 4. Push previous frame pointer
  (++sp)->aval = addr;           // 5. Push method address (for debug)
  fp = sp-2;                     // 6. Set new frame pointer
  numParams = addr[0];           // 7. Read param count from method header
  numLocals = addr[1];           // 8. Read local count from method header
  pp = fp-numParams;             // 9. Set param pointer
  lp = fp+3;                     // 10. Set local pointer
  sp += numLocals;               // 11. Allocate space for locals
  cp = addr+2;                   // 12. Set code pointer to first opcode
```

### Virtual Call Sequence

```c
Case CallVirtual:
  numParams = *(cp+3);              // 1. Read param count from bytecode
  addr = (sp-numParams+1)->aval;    // 2. Get 'this' pointer from stack
  u2 = *(uint16_t*)addr;            // 3. First field = vtable block index
  addr = block2addr(cb, u2);        // 4. Resolve vtable address
  u2 = *(uint16_t*)(cp+1);         // 5. Read method index from bytecode
  u2 = ((uint16_t*)addr)[u2];      // 6. Lookup method in vtable
  addr = block2addr(cb, u2);        // 7. Resolve method address
  (++sp)->aval = cp+4;              // 8. Push return address
  goto call;                        // 9. Reuse common call logic
```

### Return Sequence

```c
Case ReturnPop:
  cell = *sp;                    // 1. Save return value
  sp = fp-numParams;             // 2. Pop to param base
  cp = fp[0].aval;               // 3. Restore code pointer
  if (cp == 0) return cell.ival; // 4. If main method, exit VM
  fp = fp[1].aval;               // 5. Restore frame pointer
  addr = fp[2].aval;             // 6. Get caller's method address
  numParams = addr[0];           // 7. Restore caller's param count
  numLocals = addr[1];           // 8. Restore caller's local count
  pp = fp-numParams;             // 9. Restore param pointer
  lp = fp+3;                     // 10. Restore local pointer
  *sp = cell;                    // 11. Push return value
```

### Register Variables

The VM uses 7 `register` variables for maximum performance:

```c
register Cell* sp;      // stack pointer
register uint8_t* cp;   // code pointer
register uint8_t* cb;   // code base address
register Cell* pp;      // param 0 pointer
register Cell* lp;      // local 0 pointer
register Cell* fp;      // frame pointer
register uint8_t* db;   // static data base
```

---

## 7. Native Method Dispatch

### Dispatch Table Structure

Native methods are organized in a 2D array indexed by kit ID and method ID:

```c
NativeMethod** nativeTable;  // nativeTable[kitId][methodId]
```

Each `NativeMethod` has the signature:
```c
typedef Cell (*NativeMethod)(SedonaVM* vm, Cell* params);
```

For methods returning 64-bit values:
```c
typedef int64_t (*NativeMethodWide)(SedonaVM* vm, Cell* params);
```

### Three Call Variants

```c
Case CallNative:      // Returns Cell (32-bit)
  native = nativeTable[*(cp+1)][*(cp+2)];
  vm->sp = sp;
  cell = native(vm, sp-u2+1);
  sp -= u2-1;
  *sp = cell;

Case CallNativeWide:  // Returns int64_t (64-bit)
  native = nativeTable[*(cp+1)][*(cp+2)];
  vm->sp = sp;
  s8 = ((NativeMethodWide)native)(vm, sp-u2+1);
  sp -= u2-1;
  *(int64_t*)sp = s8; ++sp;

Case CallNativeVoid:  // Returns nothing
  native = nativeTable[*(cp+1)][*(cp+2)];
  vm->sp = sp;
  native(vm, sp-u2+1);
  sp -= u2;
```

### Native Table Generation

The Sedona compiler (`sedonac`) generates `nativetable.c` automatically:

```c
// From GenNativeTable.java:
// Scans all kits for native methods
// Generates forward declarations
// Creates per-kit arrays: NativeMethod kitNatives0[] = {...}
// Creates master table: NativeMethod* nativeTable[] = {kitNatives0, kitNatives1, ...}
```

### Native Method Examples

**Memory management (sys_Sys.c):**
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

**Component field access (sys_Component.c):**
```c
Cell sys_Component_getBool(SedonaVM* vm, Cell* params) {
  uint8_t* self = params[0].aval;
  void* slot = params[1].aval;
  uint16_t typeId = getTypeId(vm, getSlotType(vm, slot));
  uint16_t offset = getSlotHandle(vm, slot);
  Cell ret;
  if (typeId != BoolTypeId)
    return accessError(vm, "getBool", self, slot);
  ret.ival = getByte(self, offset);
  return ret;
}
```

**Float/bit conversion (sys_Sys.c):**
```c
Cell sys_Sys_floatToBits(SedonaVM* vm, Cell* params) {
  return *params;  // Same bits, just reinterpret
}
```

### Native Method Inventory

| Kit | Native Methods | Key Operations |
|-----|---------------|----------------|
| `sys` | ~30 | malloc/free, copy, string formatting, component access, file I/O |
| `inet` | ~15 | TCP/UDP sockets, SHA1 crypto |
| `serial` | ~8 | Serial port open/close/read/write |
| `EacIo` | ~20 | Engine I/O bridge (IPC with Sandstar engine) |
| `datetimeStd` | ~5 | Date/time operations |
| **Total** | **~78** | |

---

## 8. Memory Model

### Three Memory Segments

```
1. Code Segment (read-only)
   - Loaded from scode file
   - Contains: bytecode, type metadata, vtables, constant strings
   - Addressed via block indices

2. Stack Segment (read-write)
   - Pre-allocated fixed-size buffer
   - Contains: call frames, locals, temporaries
   - Grows upward (sp increments)

3. Static Data Segment (read-write)
   - Malloc'd at init based on scode header
   - Contains: static fields for all types
   - Accessed via data base pointer (db)
```

### Heap Allocation

The VM supports dynamic allocation through native methods:
```c
// Sedona: Sys.malloc(size) / Sys.free(obj)
// Implemented in sys_Sys.c
void* mem = malloc(num);
if (mem != NULL) memset(mem, 0, num);
```

**There is no garbage collector.** Memory management is explicit. Sedona applications in building automation typically allocate during init and never free, so leaks are rare in practice.

### Object Layout

Objects in memory are flat C structs. Fields are accessed by byte offset:

```c
// getByte: *(((uint8_t*)self) + offset)
// getShort: *(uint16_t*)(((uint8_t*)self) + offset)
// getInt: *(int32_t*)(((uint8_t*)self) + offset)
// getRef: *(void**)(((uint8_t*)self) + offset)
```

The first field of every object (offset 0) is a `uint16_t` vtable block index. Components also have a `uint16_t` component ID at offset 2.

### No Alignment Guarantees

The VM performs unaligned reads throughout:
```c
u2 = *(uint16_t*)(cp+1);        // Bytecode operand (potentially unaligned)
*(int64_t*)(sp-1) = ...;         // 64-bit stack access (Cell-aligned only)
```

On ARM, this works because:
1. The scode compiler ensures block-aligned method starts
2. The ARM Linux kernel handles unaligned access faults transparently (slow but functional)
3. Stack accesses are Cell-aligned (4 bytes on ARM)

---

## 9. Computed Goto Optimization

### Switch vs Computed Goto

The VM supports two dispatch modes:

**Switch dispatch (portable, ANSI C):**
```c
for (;;) {
  switch (*cp) {
    case Nop: ++cp; continue;
    case LoadI0: (++sp)->ival = 0; ++cp; continue;
    // ... 240 cases
  }
}
```

**Computed goto dispatch (GCC only, ~8% faster):**
```c
static void* opcodeLabels[] = OpcodeLabelsArray;  // Array of label addresses
nextInstr: goto *opcodeLabels[*cp];               // Direct jump, no bounds check

Case Nop: ++cp; goto nextInstr;
Case LoadI0: (++sp)->ival = 0; ++cp; goto nextInstr;
```

### Why Computed Goto is Faster

1. **No range check**: Switch generates `if (opcode > 239) goto default;`
2. **Better branch prediction**: Each goto has its own branch history entry
3. **Less pipeline stalling**: Direct jump vs computed branch via jump table

### Macro Abstraction

```c
#ifdef COMPUTED_GOTO
  #define EndInstr goto nextInstr
  #define Case
#else
  #define EndInstr continue
  #define Case case
#endif
```

This allows the same opcode implementation code to compile for both modes.

---

## 10. Platform Abstraction Layer

### Platform-Specific Definitions (sedona.h)

The `sedona.h` header provides platform abstraction for three targets:

| Feature | Windows (`_WIN32`) | QNX (`__QNX__`) | Unix (`__UNIX__`) |
|---------|-------------------|-----------------|-------------------|
| Integer types | Custom typedefs | `<stdint.h>` | `<stdint.h>` |
| Bool | `unsigned __int8` | `<stdbool.h>` | `<stdbool.h>` |
| Endianness | Little | Auto-detect | Auto-detect |
| ISNAN | `_isnan()` | `isnan()` | `isnan()`/`isnanf()` |
| Block size | 4 | 4 | 4 |
| Debug | Yes | Yes | Yes |

### Platform-Specific Native Methods

| Platform | File | Lines | Functions |
|----------|------|-------|-----------|
| Unix | `sys_Sys_unix.c` | 100 | ticks, sleep, platform init |
| Unix | `sys_PlatformService_unix.c` | ~40 | hibernation support |
| Win32 | `sys_Sys_win32.c` | 77 | ticks, sleep, platform init |
| Win32 | `sys_PlatformService_win32.c` | 60 | hibernation support |
| Standard | `sys_File_std.c` | 380 | File I/O (POSIX/Win32) |
| Standard | `sys_FileStore_std.c` | 380 | File system ops |
| Standard | `sys_Sys_std.c` | 110 | String formatting |
| Standard | `sys_StdOutStream_std.c` | 45 | Console output |
| Non-std | `sys_Sys_nonstd.c` | 182 | Fallback implementations |

---

## 11. Framework Kits & Sedona Language

### Kit Architecture

Each kit is a self-contained package with:
```
kitName/
├── kit.xml          ← Kit metadata, dependencies, native declarations
├── *.sedona         ← Sedona source files (compiled to scode)
├── native/          ← C native method implementations
│   ├── kitName_ClassName.c
│   └── platform-specific/
└── test/            ← Test files
```

### Kit Dependency Graph

```
sys (core)
├── control (PID, ramp, timer, etc.)
├── func (math functions)
├── logic (boolean logic)
├── math (advanced math)
├── types (type utilities)
├── inet (TCP/UDP sockets)
│   └── sox (Sedona Object Exchange protocol)
│       └── web (HTTP server)
├── serial (UART)
├── hvac (HVAC-specific components)
└── EacIo (Sandstar custom: engine bridge)
    └── platUnix (platform services)
```

### The Sedona Language

Sedona is a Java-like language compiled to scode bytecode:

```java
// Example: AnalogInput component (EacIo/AnalogInput.sedona)
class AnalogInput extends Component {
  @config property int channel
  @readonly property float out
  @readonly property Buf(64) channelName

  virtual override void execute() {
    if (channel != 0 && enabled == true) {
      out := analogInPoint.get(channel);
    }
  }
}
```

Key Sedona language features:
- **Components**: First-class with lifecycle (`start`, `execute`, `stop`, `changed`)
- **Properties**: `@config` (user-configurable), `@readonly`, `@action` (callable)
- **Slots**: Fields and methods are unified as "slots" with metadata
- **Links**: Data flow connections between component properties
- **Buf**: Fixed-size byte buffers (no dynamic strings)
- **No generics, no exceptions, no garbage collection**

### Kits.xml Application Manifest

```xml
<sedonaCode endian="little" blockSize="4" refSize="4"
            main="sys::Sys.main" debug="true" test="true">
  <depend on="sys 1.2+" />
  <depend on="EacIo 1.2.30" />
  <depend on="control 1.2+" />
  <!-- ... more dependencies -->
</sedonaCode>
```

---

## 12. Sedona Compiler (sedonac)

### Overview

`sedonac` is a Java-based compiler (~34,118 lines) that:
1. Parses `.sedona` source files
2. Resolves types, slots, and dependencies across kits
3. Generates scode bytecode images
4. Generates native method tables (`nativetable.c`)
5. Generates stubs for simulator builds

### Key Compiler Steps

| Step | Class | Description |
|------|-------|-------------|
| `ReadKits` | Parse | Read kit.xml and .sedona files |
| `Resolve` | Semantic | Resolve type references across kits |
| `Normalize` | Transform | Flatten inheritance, compute field offsets |
| `CheckErrors` | Validate | Type checking, slot validation |
| `CodeGen` | Generate | Emit scode bytecode |
| `GenNativeTable` | Generate | Emit nativetable.c with dispatch arrays |
| `AssembleImage` | Link | Combine all kits into single scode image |

### Native Table Generation (GenNativeTable.java)

The compiler scans all kits for methods marked `native` and generates:

```c
// Forward declarations
extern Cell sys_Sys_malloc(SedonaVM* vm, Cell* params);
extern Cell sys_Sys_free(SedonaVM* vm, Cell* params);
// ...

// Per-kit table
NativeMethod kitNatives0[] = {
  sys_Sys_malloc,
  sys_Sys_free,
  // ...
};

// Master table
NativeMethod* nativeTable[] = {
  kitNatives0,   // kit 0: sys
  kitNatives1,   // kit 1: control
  // ...
};
```

---

## 13. Unsafe Patterns Catalog

### Pattern 1: Raw Pointer Arithmetic Everywhere

```c
// Bytecode read — unaligned 16-bit from code stream
u2 = *(uint16_t*)(cp+1);

// Object field access — byte offset into raw pointer
sp->ival = *(int32_t*)(((uint8_t*)sp->aval) + *(uint16_t*)(cp+1));

// Stack pointer arithmetic
*(int64_t*)(sp-1) = *(int64_t*)(sp-1) + *(int64_t*)(sp+1);
```

**Risk**: Any incorrect offset, null pointer, or out-of-bounds access is undefined behavior.

### Pattern 2: Type Punning via Union

```c
Cell cell;
cell.ival = 0x7fc00000;  // Write as int
float f = cell.fval;     // Read as float — UB in strict C, works in practice
```

### Pattern 3: Unchecked Array Indexing

```c
native = nativeTable[*(cp+1)][*(cp+2)];  // What if kitId or methodId is out of range?
```

The debug build has `isNativeIdValid()` but release builds do not check.

### Pattern 4: Stack Overflow Risk

```c
#ifdef SCODE_DEBUG
  if (sp >= maxStackAddr)
    return handleStackOverflow(vm, *cp, fp, sp);
#endif
```

Stack overflow checking is **only enabled in debug builds**.

### Pattern 5: Global Mutable State

```c
static int gStopByUser = 0;      // Signal from external thread
Cell zeroCell, oneCell, negOneCell;  // Global Cell constants
static char strbuf[32];           // Shared string buffer in sys_Sys_std.c
```

The `gStopByUser` flag is accessed from multiple threads without synchronization. The `strbuf` in `sys_Sys_std.c` is a static buffer shared across all native calls — not thread-safe.

### Pattern 6: Unaligned Memory Access

```c
// 16-bit read from bytecode (potentially unaligned address)
u2 = *(uint16_t*)(cp+1);

// 32-bit read from field at arbitrary byte offset
sp->ival = *(int32_t*)(((uint8_t*)sp->aval) + offset);
```

### Pattern 7: sprintf to Static Buffers

```c
static char buf[64];
sprintf(buf, "%s::%s", kitName, typeName);  // No bounds checking
return buf;  // Returning pointer to static — not reentrant
```

### Pattern 8: Implicit Fall-Through (Intentional but Fragile)

```c
Case LoadParam0Call:
  *(++sp) = *(pp);
  // fall thru   ← intentional fall-through to Case Call

Case Call:
  // ...
```

---

## 14. Key Challenges for Rust Conversion

### Challenge 1: The Cell Union

The Cell type is a union of `i32`, `f32`, and `*mut u8`. In Rust, this maps to an `enum` (safe but larger) or a union (requires `unsafe`). Since the VM accesses Cell members at full interpreter speed, this choice is performance-critical.

### Challenge 2: Raw Pointer-Based Object Model

Every field access in the VM is `*(T*)(base + offset)`. Rust's borrow checker cannot reason about these raw pointer offsets. The entire storage opcode section (~59 opcodes) must operate in `unsafe` blocks.

### Challenge 3: 64-bit Values on 32-bit Stack

Long and double occupy 2 Cell slots. The code casts `Cell*` to `int64_t*` and reads/writes 8 bytes. In Rust, this requires careful `unsafe` pointer manipulation.

### Challenge 4: Computed Goto

Rust has no computed goto equivalent. The alternatives are:
- `match` statement (closest to switch, may optimize to jump table)
- Dispatch table of function pointers (overhead per opcode)
- Cranelift/LLVM JIT (complex, overkill for this VM)

### Challenge 5: Native Method ABI

All ~78 native methods follow `fn(vm: *mut SedonaVM, params: *mut Cell) -> Cell`. These must remain callable from the interpreter. If the VM is Rust, native methods could be Rust functions, but they still need to accept raw Cell pointers for stack access.

### Challenge 6: The Sedona Compiler

`sedonac` (34K lines of Java) generates scode bytecode and `nativetable.c`. If the VM moves to Rust, the compiler must generate compatible bytecode and Rust-compatible native tables instead.

### Challenge 7: Existing Scode Compatibility

Thousands of existing Sedona applications compile to scode bytecode. The Rust VM must execute the **exact same scode format** to maintain compatibility with existing applications and the sedonac compiler.

### Challenge 8: Pointer-Size Sensitivity

The scode format encodes `sizeof(void*)` in the header. ARM (32-bit) uses 4-byte pointers, meaning Cell = 4 bytes. On 64-bit hosts, Cell = 8 bytes. The scode is NOT portable between architectures. The Rust VM must match the target architecture's pointer size.

---

## Summary

| Aspect | Scope |
|--------|-------|
| Core interpreter | 1,281 lines of C (vm.c) |
| Supporting headers | 1,585 lines (sedona.h, scode.h, errorcodes.h) |
| Native methods | 4,880 lines across 24 C files |
| Total C to convert | ~8,926 lines |
| Sedona source (unchanged) | 39,294 lines (.sedona files, not converted) |
| Sedona compiler (unchanged) | 34,118 lines (Java, not converted) |
| Opcodes | 240 (all must be implemented) |
| Native methods | ~78 (all must be ported) |
| `unsafe` code needed | Extensive (interpreter core, all storage ops, native calls) |
| Estimated Rust lines | 4,000–6,000 (core VM) + 3,000–4,000 (native methods) |

The Sedona VM is a well-structured, compact interpreter. The core challenge for Rust conversion is that its fundamental operations — union-typed Cell, raw pointer field access, unaligned reads, computed goto — are inherently `unsafe` in Rust's type system. The companion document ([13_SEDONA_VM_RUST_PORTING_STRATEGY.md](13_SEDONA_VM_RUST_PORTING_STRATEGY.md)) details the specific Rust implementation strategy.
