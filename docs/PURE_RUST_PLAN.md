# Pure Rust Sandstar — Implementation Plan

**Date:** 2026-03-20
**Goal:** Eliminate all C code, enable Sedona Application Editor connectivity
**Total Effort:** ~54 dev-days (~8,300 lines of new Rust)
**Current C Code:** 6,839 lines in `crates/sandstar-svm/csrc/`

---

## Overview

Three major workstreams, executed in order:

| Workstream | What | New Lines | Dev-Days | Result |
|------------|------|-----------|----------|--------|
| **Phase A** | Pure Rust VM interpreter | ~2,500 | 15 | Rust bytecode execution |
| **Phase B-D** | Native methods in Rust | ~1,920 | 17 | Zero C kit files |
| **Phase S** | SOX protocol server | ~1,800 | 10 | Sedona Editor connects |
| **Total** | | **~6,220** | **42** | **Pure Rust + Sedona Editor** |

---

## Phase A: Pure Rust VM Interpreter (15 dev-days)

### New Files to Create (in order)

| Step | File | Lines | Purpose |
|------|------|-------|---------|
| A1 | `src/opcodes.rs` | 280 | Opcode enum (240 variants, #[repr(u8)], TryFrom) |
| A2 | `src/vm_error.rs` | 80 | VmError enum (BadImage, StackOverflow, etc.) |
| A3 | `src/image_loader.rs` | 120 | .scode file loader with header validation |
| A4 | `src/vm_stack.rs` | 200 | Stack with push/pop/wide ops, frame management |
| A5 | `src/vm_memory.rs` | 150 | Scode + data segment, field accessors |
| A6 | `src/native_table.rs` | 250 | Rust native method registration table |
| A7 | `src/vm_interpreter.rs` | 1,050 | Main dispatch loop (236 opcodes in 17 groups) |
| A8 | `src/test_utils.rs` | 150 | Test scode builder, mock natives |
| A9 | `src/rust_runner.rs` | 200 | RustSvmRunner (parallel to FFI runner) |
| A10 | Update `lib.rs` | 20 | Module declarations, `pure-rust-vm` feature |

### Opcode Groups (inside vm_interpreter.rs)

| Group | Opcodes | Count | Lines | Complexity |
|-------|---------|-------|-------|------------|
| A: Literals | 0-28 | 29 | 80 | Low |
| B: Params | 29-36 | 8 | 30 | Low |
| C: Locals | 37-56 | 20 | 40 | Low |
| D: Int ops | 57-76 | 20 | 40 | Low |
| E: Long ops | 77-94 | 18 | 40 | Medium (wide) |
| F: Float ops | 95-105 | 11 | 40 | Medium (NaN) |
| G: Double ops | 106-116 | 11 | 40 | Medium (NaN) |
| H: Casts | 117-128 | 12 | 25 | Low |
| I: Obj compare | 129-130 | 2 | 5 | Low |
| J: General compare | 131-132 | 2 | 5 | Low |
| K: Stack manip | 133-141 | 9 | 20 | Low |
| L: Near branch | 142-145 | 4 | 30 | Low |
| M: Far branch | 146-149 | 4 | 30 | Low |
| N: Int compare+branch | 150-161 | 12 | 50 | Low |
| O: Field load/store | 162-220 | 59 | 180 | **High** (unsafe) |
| P: Method calls | 221-229 | 9 | 120 | **High** (frames) |
| Q: Misc | 230-238 | 9 | 40 | Medium |

### Key Design Decisions

- **Cell stays as union** (not enum) — required for scode binary compatibility
- **Stack uses index-based access** (not raw pointers) — bounds checked
- **Feature flag `pure-rust-vm`** — both C and Rust VM coexist during transition
- **Unsafe confined to**: Cell accessors, field load/store, native calls
- **NaN handling**: FloatEq/DoubleEq treat NaN==NaN as true (Sedona convention)

---

## Phase B-D: Native Methods in Rust (17 dev-days)

### Execution Order

| Phase | Kit | Methods | New File | Lines | Days |
|-------|-----|---------|----------|-------|------|
| D | Kit 9 (datetime) | 3 | `native_datetime.rs` | 60 | 1 |
| B1 | Kit 0 (easy sys) | 25 | `native_sys.rs` | 440 | 5 |
| B2 | Kit 0 (file I/O) | 11 | `native_file.rs` | 250 | 3 |
| B3 | Kit 0 (component) | 19 | `native_component.rs` + `scode_helpers.rs` | 600 | 5 |
| C | Kit 2 (network) | 17 | `native_inet.rs` + `native_sha1.rs` | 530 | 3 |

### Method Inventory (80 total)

**Kit 0 — sys (60 methods):**
- String formatting: intStr, hexStr, longStr, longHexStr, floatStr, doubleStr (6)
- Memory: malloc, free, copy, compareBytes, setBytes, andBytes, orBytes (7)
- Bit conversion: floatToBits, bitsToFloat, doubleToBits, bitsToDouble (4)
- Platform: platformType, ticks, sleep, rand, scodeAddr (5)
- Component reflection: getBool/Int/Long/Float/Double/Buf, doSetBool/Int/Long/Float/Double (11)
- Component invoke: invokeVoid/Bool/Int/Long/Float/Double/Buf (7)
- StdOut: doWrite, doWriteBytes, doFlush (3)
- FileStore: doSize/Open/Read/ReadBytes/Write/WriteBytes/Tell/Seek/Flush/Close/rename (11)
- PlatformService: doPlatformId, getPlatVersion, getNativeMemAvailable (3)
- Type: malloc (1)
- Str: fromBytes (1)
- Test: doMain (1)

**Kit 2 — inet (17 methods):**
- TCP client: connect, finishConnect, write, read, close (5)
- TCP server: bind, accept, close (3)
- UDP: open, bind, join, send, receive, close, maxPacketSize, idealPacketSize (8)
- Crypto: sha1 (1)

**Kit 9 — datetimeStd (3 methods):**
- doNow, doSetClock, doGetUtcOffset

### After All Phases Complete

`build.rs` simplifies to only compile `vm.c` + `nativetable.c`. All kit native methods are pure Rust, working on ALL platforms (Linux, Windows, ARM).

---

## Phase S: SOX Protocol Server (10 dev-days)

### New Files to Create

| Step | File | Lines | Purpose |
|------|------|-------|---------|
| S1 | `sox/mod.rs` | 30 | Module + `run_sox_server()` entry |
| S2 | `sox/dasp.rs` | 450 | DASP transport (UDP, sessions, reliability) |
| S3 | `sox/sox_auth.rs` | 80 | SHA-1 digest authentication |
| S4 | `sox/sox_protocol.rs` | 350 | SOX binary message parser/builder |
| S5 | `sox/sox_handlers.rs` | 600 | Virtual component tree + all command handlers |
| S6 | Modify `args.rs` | +10 | `--sox`, `--sox-port` flags |
| S7 | Modify `main.rs` | +20 | Spawn SOX server task |
| S8 | Modify `Cargo.toml` | +1 | Add `sha1` dependency |

### DASP Handshake (4-way over UDP port 1876)

```
Client                    Server
  |--- HELLO ------------->|
  |<-- CHALLENGE ----------|  (nonce + SHA-1 algorithm)
  |--- AUTHENTICATE ------>|  (SHA-1 digest of nonce+user+pass)
  |<-- WELCOME ------------|  (session established)
  |                        |
  |--- SOX commands ------>|  (inside DASP DATAGRAMs)
  |<-- SOX responses ------|
  |<-- COV events ---------|  (server push)
```

### SOX Commands Required for Sedona Editor

| Command | Code | Purpose | Priority |
|---------|------|---------|----------|
| readSchema | `v` | Kit list + checksums | **Must have** |
| readVersion | `y` | Platform info | **Must have** |
| readComp | `c` | Component tree + values | **Must have** |
| subscribe | `s` | Register for COV events | **Must have** |
| unsubscribe | `u` | Remove COV registration | **Must have** |
| event | `e` | Push changed values | **Must have** |
| write | `w` | Write slot value | **Must have** |
| fileOpen/Read | `f`/`g` | Download kit manifests | **Should have** |
| invoke | `i` | Action invocation | Nice to have |
| add/delete | `a`/`d` | Component mutations | Later |

### Virtual Component Tree

Since the engine uses channels (not Sedona components), we present a virtual tree:

```
App (compId=0)
├── service (1)
│   ├── sox (2)
│   ├── users (3)
│   └── plat (4)
├── io (5)
│   ├── channel_1113 (100) → maps to engine channel 1113
│   ├── channel_1713 (101) → maps to engine channel 1713
│   └── ... one per polled channel
└── control (6)
    ├── zone_cooling (200) → maps to PID loop
    └── ... one per control loop
```

---

## Execution Timeline

```
Week 1-2:   Phase A1-A6 (VM foundation: opcodes, stack, memory, image loader)
Week 3:     Phase A7 (opcode dispatch — the big one)
Week 4:     Phase A8-A9 (tests, runner) + Phase D (datetime, trivial)
Week 5:     Phase B1 (easy sys methods)
Week 6:     Phase B2 (file I/O) + start B3 (component)
Week 7:     Phase B3 (component reflection — hardest)
Week 8:     Phase C (network methods)
Week 9-10:  Phase S (SOX protocol server)
Week 10:    Integration testing with Sedona Application Editor
```

---

## Definition of Done

- [ ] `cargo build --no-default-features` compiles without C compiler
- [ ] `cargo test --workspace` passes 900+ tests
- [ ] Feature flag `pure-rust-vm` enables Rust interpreter
- [ ] Sedona Application Editor connects via SOX (UDP 1876)
- [ ] Editor displays component tree with live channel values
- [ ] Editor can write setpoints that reach the engine
- [ ] All 80 native methods work on both Linux ARM and Windows x86_64
- [ ] Zero `csrc/*.c` files compiled when `pure-rust-vm` is active
