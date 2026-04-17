# Pure Rust VM Completion Plan

**Date:** 2026-04-10
**Goal:** Eliminate all C code from the Sedona VM, making `pure-rust-vm` the default
**Estimated Effort:** 5 phases, ~1-2 weeks total
**Prerequisite Knowledge:** PURE_RUST_PLAN.md (Phases A-D complete, Phase S in progress)

---

## Current State Summary

- **Interpreter:** 240/240 opcodes — COMPLETE, zero stubs
- **Native methods:** 88 implemented, **37 disabled** (commented out in `native_mod.rs`)
- **Server:** Uses C FFI `SvmRunner`, not pure Rust `RustSvmRunner`
- **Feature flag:** `pure-rust-vm` is opt-in, not default
- **C path:** build.rs still compiles vm.c + kit C files when feature is off
- **Tests:** 167 unit tests, zero integration tests with real scode

---

## Phase 1: Enable Disabled Native Methods

**What:** Uncomment the 37 disabled native methods and verify they compile and pass tests.

**Files to modify:**

1. **`crates/sandstar-svm/src/native_mod.rs:23`** — Uncomment:
   ```rust
   crate::native_component::register_kit0_component(table);
   ```

2. **`crates/sandstar-svm/src/native_mod.rs:26`** — Uncomment:
   ```rust
   crate::native_inet::register_kit2(table);
   ```

**Reference patterns:**
- `native_component.rs` — 20 methods: Component.invokeVoid/Bool/Int/Long/Float/Double/Buf, Component.getBool/Int/Long/Float/Double/Buf, Component.doSetBool/Int/Long/Float/Double, Type.malloc, Test.doMain
- `native_inet.rs` — 17 methods: TcpSocket (5), TcpServerSocket (3), UdpSocket (8), Crypto.sha1 (1)

**Verification:**
- [ ] `cargo build -p sandstar-svm` compiles without errors
- [ ] `cargo build -p sandstar-svm --features pure-rust-vm` compiles without errors
- [ ] `cargo test -p sandstar-svm` — all 167+ tests pass
- [ ] `cargo clippy -p sandstar-svm -- -D warnings` — zero warnings

**Anti-patterns:**
- Do NOT modify the method implementations — just enable registration
- Do NOT change function signatures or types

---

## Phase 2: Wire Server to Use RustSvmRunner

**What:** Replace the C FFI `SvmRunner` with the pure Rust `RustSvmRunner` in the server's main.rs when `pure-rust-vm` feature is enabled.

**Files to modify:**

1. **`crates/sandstar-server/src/main.rs`** (lines 305-342) — Add feature-gated runner selection:
   - When `pure-rust-vm`: Use `RustSvmRunner` from `rust_runner.rs`
   - When not `pure-rust-vm`: Use existing `SvmRunner` from `runner.rs` (C FFI)
   - Both runners share the same bridge pattern (ENGINE_BRIDGE, WRITE_QUEUE, TAG_WRITE_QUEUE)

**Reference patterns:**
- `rust_runner.rs` API: `RustSvmRunner::new(scode_path)`, `.start()`, `.stop()`, `.is_running()`
- `runner.rs` API: `SvmRunner::new(scode_path)`, `.start()`, `.stop()`, `.is_running()`
- Both implement the same lifecycle pattern — the switch is at construction time

2. **`crates/sandstar-server/Cargo.toml`** — Forward the `pure-rust-vm` feature to sandstar-svm:
   ```toml
   [features]
   pure-rust-vm = ["sandstar-svm/pure-rust-vm"]
   ```

**Verification:**
- [ ] `cargo build -p sandstar-server --features svm` — compiles (C path)
- [ ] `cargo build -p sandstar-server --features svm,pure-rust-vm` — compiles (Rust path)
- [ ] `cargo test --workspace` — all tests pass
- [ ] Manual test: run server with `--sedona` using a real kits.scode from `SedonaRepo/`

**Anti-patterns:**
- Do NOT remove the C runner yet — keep both paths working
- Do NOT change the bridge architecture (ENGINE_BRIDGE, queues) — both runners use it

---

## Phase 3: Integration Testing with Real Scode

**What:** Add integration tests that load real `.scode` files and verify the pure Rust VM executes them correctly.

**Files to create/modify:**

1. **`crates/sandstar-svm/tests/integration_tests.rs`** — New file:
   - Test: Load `SedonaRepo/2026-03-11_21-56-18/app/kits.scode` with `RustSvmRunner`
   - Test: Verify image header parsing (magic 0x5ED0BA07)
   - Test: Verify native method dispatch for Kit 0 (sys), Kit 4 (EacIo)
   - Test: Run main method with MAX_INSTRUCTIONS timeout (expect clean Yield/Hibernate)
   - Test: Verify `SvmRunner` (C) and `RustSvmRunner` (Rust) produce same result on same scode

2. **`crates/sandstar-svm/tests/native_tests.rs`** — New file:
   - Test: Kit 0 sys methods (malloc/free, string formatting, ticks)
   - Test: Kit 0 component methods (slot resolution, get/set)
   - Test: Kit 2 inet methods (TCP connect, UDP open/bind)
   - Test: Kit 4 EacIo methods (channel read via bridge snapshot)

**Reference patterns:**
- `test_utils.rs` `ScodeBuilder` for synthetic scode
- `image_loader.rs` `load_scode(path)` for real files
- `rust_runner.rs` tests (lines 450-597) for runner lifecycle

**Verification:**
- [ ] `cargo test -p sandstar-svm` — all old + new tests pass
- [ ] `cargo test -p sandstar-svm --features pure-rust-vm` — same tests pass
- [ ] Native method tests exercise all 88 registered methods
- [ ] Real scode loads without `BadImage` errors

**Anti-patterns:**
- Do NOT hardcode absolute paths to scode files — use relative paths from `CARGO_MANIFEST_DIR`
- Do NOT skip tests on Windows — use `#[cfg(unix)]` only for Linux-specific hardware tests

---

## Phase 4: Make pure-rust-vm the Default

**What:** Flip the feature flag so pure Rust is default, C FFI is opt-in.

**Files to modify:**

1. **`crates/sandstar-svm/Cargo.toml`** — Change default features:
   ```toml
   [features]
   default = ["pure-rust-vm"]
   pure-rust-vm = []
   c-ffi-vm = []  # New: opt-in for C path
   ```

2. **`crates/sandstar-svm/build.rs`** — Invert the gate:
   ```rust
   // Only compile C code when c-ffi-vm is explicitly requested
   if !cfg!(feature = "c-ffi-vm") {
       println!("cargo:warning=Building with pure Rust VM (no C code)");
       return;
   }
   ```

3. **`crates/sandstar-server/Cargo.toml`** — Update feature forwarding:
   ```toml
   [features]
   svm = ["dep:sandstar-svm"]  # Now gets pure-rust-vm by default
   c-ffi-vm = ["sandstar-svm/c-ffi-vm"]  # Explicit opt-in for C
   ```

4. **`crates/sandstar-server/src/main.rs`** — Invert the runner selection:
   - Default: `RustSvmRunner`
   - `#[cfg(feature = "c-ffi-vm")]`: `SvmRunner` (C FFI)

5. **`.github/workflows/ci.yml`** — Update CI:
   - Default test job now tests pure Rust VM
   - Optional: add a job that tests `c-ffi-vm` for backward compat

**Verification:**
- [ ] `cargo build -p sandstar-svm` — compiles with NO C code (check build output)
- [ ] `cargo build -p sandstar-server --features svm` — pure Rust VM
- [ ] `cargo test --workspace` — all tests pass with pure Rust VM
- [ ] `cargo build -p sandstar-server --features svm,c-ffi-vm` — C path still works
- [ ] ARM cross-compile: `cargo arm-build --features svm` — succeeds without Zig CC for C

**Anti-patterns:**
- Do NOT delete the C runner yet — keep it as `c-ffi-vm` opt-in for one release cycle
- Do NOT modify .cargo/config.toml ARM C compiler settings yet — still needed for c-ffi-vm

---

## Phase 5: Remove C FFI Path (Final Cleanup)

**What:** Delete all C code, FFI declarations, and C-only build infrastructure. This is the point of no return.

**Files to delete:**
- `crates/sandstar-svm/csrc/` — All 27 C source files (6,839 lines)
- `crates/sandstar-svm/src/ffi.rs` — Raw FFI declarations (29 lines)
- `crates/sandstar-svm/src/runner.rs` — C FFI runner (319 lines)

**Files to modify:**

1. **`crates/sandstar-svm/build.rs`** — Reduce to no-op or delete entirely:
   ```rust
   fn main() {
       // Pure Rust VM — no C compilation needed
   }
   ```

2. **`crates/sandstar-svm/Cargo.toml`** — Remove:
   - `cc` build-dependency
   - `c-ffi-vm` feature
   - `pure-rust-vm` feature (no longer needed — it's the only path)

3. **`crates/sandstar-svm/src/lib.rs`** — Remove:
   - `pub mod ffi;`
   - `pub mod runner;` (the C runner)
   - Any `#[cfg(feature = "c-ffi-vm")]` gates

4. **`crates/sandstar-svm/src/bridge.rs`** — Remove:
   - All 22 `#[no_mangle] extern "C"` functions (lines ~700-1349)
   - The `ffi_safe!` macro (only used by C FFI functions)
   - Keep: `ENGINE_BRIDGE`, `WRITE_QUEUE`, `TAG_WRITE_QUEUE`, `ChannelInfo`, `ChannelSnapshot`, `SvmWrite`, `SvmTagWrite`, `set_engine_bridge`, `set_write_queue`, `drain_writes`, etc.

5. **`crates/sandstar-server/src/main.rs`** — Remove:
   - `#[cfg(feature = "c-ffi-vm")]` branches
   - `SvmRunner` imports (only `RustSvmRunner` remains)

6. **`crates/sandstar-server/Cargo.toml`** — Remove:
   - `c-ffi-vm` feature

7. **`.cargo/config.toml`** — Remove (if no other crate needs C):
   - `CC_armv7_unknown_linux_gnueabihf` environment variable references
   - Zig CC linker settings for C code (keep Rust linker settings)

8. **`.github/workflows/ci.yml`** — Remove:
   - Zig installation step (if only needed for C cross-compile)
   - `c-ffi-vm` test job

**Verification:**
- [ ] `cargo build --workspace` — compiles with ZERO C code
- [ ] `cargo test --workspace` — all tests pass
- [ ] `cargo clippy --workspace -- -D warnings` — zero warnings
- [ ] ARM cross-compile succeeds without any C compiler
- [ ] Binary size decreased (no vm.c + kit C objects linked)
- [ ] Grep: zero occurrences of `extern "C"` in sandstar-svm (except test code if any)
- [ ] Grep: zero occurrences of `#[no_mangle]` in sandstar-svm
- [ ] Grep: zero occurrences of `cc::Build` in build.rs

**Anti-patterns:**
- Do NOT delete bridge.rs entirely — the engine bridge (ChannelSnapshot, write queues) is still needed
- Do NOT delete types.rs Cell union — the pure Rust VM uses it
- Do NOT remove .cargo/config.toml ARM settings if other crates still need a C linker

---

## Definition of Done

When all 5 phases are complete:

1. `cargo build -p sandstar-svm` compiles **zero C files**
2. `cargo build -p sandstar-server --features svm` produces a **pure Rust binary**
3. All **2,319+ workspace tests** pass
4. ARM cross-compile **does not require Zig CC** for the SVM crate
5. The `csrc/` directory (6,839 lines of C) is **deleted**
6. `ffi.rs` (FFI declarations) is **deleted**
7. `runner.rs` (C FFI runner) is **deleted**
8. Server runs with `--sedona` flag using **RustSvmRunner**
9. Real `.scode` files from `SedonaRepo/` load and execute correctly
10. All 88 native methods are **registered and active**

---

## Risk Mitigation

| Risk | Mitigation |
|------|-----------|
| Component methods (20) fail when enabled | Test with real scode first; keep C path as fallback |
| Inet methods (17) cause issues on ARM | Test TCP/UDP in isolation; keep intentional-stub fallback |
| Real scode fails on Rust VM | A/B test: run same scode on C runner and Rust runner, compare |
| ARM binary size regression | Measure before/after; Rust VM should be smaller than C VM + libs |
| CI breaks during transition | Keep both feature paths in CI until Phase 5 |

---

*This plan can be executed in 5 consecutive sessions. Each phase is self-contained with its own verification checklist.*
