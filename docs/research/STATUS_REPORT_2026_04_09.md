# Sandstar Rust Migration - Comprehensive Status Report

**Date:** 2026-04-09
**Version:** 1.4.0
**Status:** Production Deployed
**Repository:** [Project-SandStar/Sandstar_Rust](https://github.com/Project-SandStar/Sandstar_Rust)

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Migration Scope and Achievements](#2-migration-scope-and-achievements)
3. [Architecture Overview](#3-architecture-overview)
4. [Research Document Index](#4-research-document-index)
5. [Completed Phases](#5-completed-phases)
6. [Key Technical Decisions](#6-key-technical-decisions)
7. [Quantitative Results](#7-quantitative-results)
8. [Safety and Security](#8-safety-and-security)
9. [Future Roadmap](#9-future-roadmap)
10. [Risk Assessment](#10-risk-assessment)

---

## 1. Executive Summary

The Sandstar engine has been **fully rewritten from C/C++ to Rust** and deployed to production on a BeagleBone ARM device (192.168.1.3). The migration replaced ~32,314 lines of custom C/C++ and eliminated ~550,000 lines of vendored dependencies (POCO framework + Boost), producing a Rust codebase of ~25,000 lines across 7 workspace crates with **1,637 tests passing and 0 failures**.

The rewrite delivered significantly more than a straight port: TLS encryption, SCRAM-SHA-256 authentication, rate limiting, WebSocket push, I2C coalescing (270ms to 45ms), config-driven DDC control, full SOX/DASP protocol implementation, and visual wiring through the Sedona Editor — all capabilities absent from the original C/C++ system.

Twenty research documents (Docs 00-19) were produced during the migration, documenting every architectural decision, dependency mapping, safety analysis, and future enhancement plan. This report synthesizes all twenty.

---

## 2. Migration Scope and Achievements

### 2.1 Code Replaced

| Category | C/C++ Lines | Rust Lines | Reduction |
|----------|------------|------------|-----------|
| Engine core (channels, tables, polls, watches, conversions) | ~5,500 | ~1,510 | 73% |
| Hardware drivers (ADC, GPIO, I2C, PWM, UART) | ~2,760 | ~965 | 65% |
| Haystack type system (30+ value types, filter, Zinc I/O) | ~8,000 | 0 (libhaystack crate) | 100% |
| REST API (POCO HTTP, handlers, state management) | ~6,900 | ~3,500 | 49% |
| Zinc parsers (C + C++ dual implementations) | ~3,067 | ~100 (wrappers) | 97% |
| IPC bridge (POSIX message queues) | ~1,156 | ~500 | 57% |
| CLI tools (12 utilities) | ~2,266 | ~800 | 65% |
| Custom CSV/config parsers | ~1,253 | 0 (csv crate) | 100% |
| **Custom code total** | **~32,314** | **~25,000** | **~23%** |
| POCO framework (vendored) | ~500,000 | 0 | 100% |
| Boost headers (vendored) | ~50,000 | 0 | 100% |
| **Total eliminated** | **~582,314** | — | — |

### 2.2 Feature Parity and Enhancements

- **99% feature parity** with the original C/C++ system (80/80 features matched)
- **20 additional capabilities** not present in the original:
  - TLS via rustls
  - SCRAM-SHA-256 + bearer token authentication
  - Atomic sliding-window rate limiter
  - WebSocket push (Haystack-over-WebSocket)
  - Full SOX/DASP protocol (pure Rust, 20 commands)
  - 185 manifest type definitions parsed from XML
  - Config-driven DDC control (PID, sequencer, 20 component types)
  - I2C read coalescing (6x speedup)
  - Cycle detection in component link graphs
  - Component persistence with auto-save
  - Channel-to-logic bridge (ConstFloat proxy for live sensor data)
  - FileWrite + FileRename SOX commands
  - `.sax` converter for Sedona application files
  - CORS middleware (proper preflight handling)
  - Graceful shutdown (in-flight request completion)
  - Hot-reload with debounced file watching
  - Fine-grained async locking (reads don't block watches)
  - Health endpoint and metrics
  - Structured logging via `tracing`
  - Systemd integration (Type=simple for Debian 9)

---

## 3. Architecture Overview

### 3.1 Workspace Structure (7 Crates)

| Crate | Purpose | Tests |
|-------|---------|-------|
| `sandstar-engine` | Core: channels, tables, conversions, filters, PID, sequencer, 35 component types | ~200 |
| `sandstar-hal` | HAL trait definitions + MockHal | 9 |
| `sandstar-hal-linux` | Linux sysfs drivers: GPIO, ADC, I2C, PWM, UART | 82 |
| `sandstar-ipc` | Shared IPC types + length-prefixed bincode wire protocol | 4 |
| `sandstar-server` | Axum REST, WebSocket, TLS, SCRAM, SOX/DASP, config loader | ~280 |
| `sandstar-cli` | CLI binary with clap subcommands (10 commands) | — |
| `sandstar-svm` | Sedona VM FFI, bridge, runner, native methods (22 Kit 4 natives) | ~50 |

**Total: 1,637 tests passing, 0 failures**

### 3.2 Runtime Modes

| Mode | Command | Description |
|------|---------|-------------|
| Demo | `cargo run -p sandstar-server` | 5 demo channels + MockHal |
| Sedona | `cargo run -p sandstar-server -- --sedona --scode-path <path>` | Full Sedona VM integration |
| Production | `SANDSTAR_CONFIG_DIR=".../EacIo" cargo run -p sandstar-server` | 140 channels + 16 lookup tables |

### 3.3 Dependency Strategy

25 direct Rust crate dependencies replace the entirety of POCO + Boost + custom parsers:

| Domain | Key Crates |
|--------|-----------|
| HTTP/API | axum, tower, tower-http, hyper, reqwest |
| Async runtime | tokio |
| Haystack types | libhaystack (J2 Innovations / Siemens) |
| Serialization | serde, serde_json, csv |
| Observability | tracing, tracing-subscriber |
| Security | rustls, hmac, sha2, pbkdf2, base64 |
| Hardware | gpio-cdev, i2cdev, serialport |
| CLI | clap |
| XML | quick-xml |

---

## 4. Research Document Index

The 20 research documents form a complete technical record of the migration:

### Foundation & Core Migration (Docs 00-04)

| Doc | Title | Focus |
|-----|-------|-------|
| 00 | Executive Summary | Full project overview, phase timeline, crate structure, security audit |
| 01 | Engine Core Analysis | C engine (5,507 LOC) to Rust mapping — 73% reduction, data structure transformations |
| 02 | Hardware Drivers | 8 C driver files (2,760 LOC) to Rust — 65% reduction, GPIO chardev upgrade, async I2C/UART |
| 03 | Haystack Type System | 66 C++ files (8,000 LOC) eliminated by `libhaystack` crate — Dict mutex removed, Filter hierarchy flattened |
| 04 | REST API / Axum Migration | POCO (500K vendored) to Axum — fine-grained locking, proper CORS, graceful shutdown, background task isolation |

### Infrastructure & Safety (Docs 05-09)

| Doc | Title | Focus |
|-----|-------|-------|
| 05 | Zinc I/O & Encoding | Dual C/C++ Zinc parsers (3,067 LOC) eliminated by `libhaystack` — JSON support added for free |
| 06 | Sedona VM FFI Strategy | 29 `extern "C"` functions, Cell union ABI, panic safety, SCRAM auth in pure Rust, buffer overflow fix |
| 07 | IPC Bridge | POSIX message queues to tokio channels — 3-phase roadmap from IPC to shared memory, ARM ABI verification |
| 08 | Memory Safety Analysis | 45+ vulnerability sites across 13 bug classes — all structurally eliminated by Rust's type system |
| 09 | Dependency Mapping | Complete C/C++ to Rust crate mapping, final Cargo.toml, binary size optimization profile |

### Build & VM Analysis (Docs 10-14)

| Doc | Title | Focus |
|-----|-------|-------|
| 10 | Build & Cross-Compilation | CMake+Docker to Cargo — 82% build script reduction, 3-5x cold build speedup, 20-60x incremental |
| 11 | Migration Roadmap | 7-phase plan for 32,314 LOC, phase dependencies, testing strategy, rollback plan |
| 12 | Sedona VM Architecture | Reverse-engineering of VM internals — 1,281 LOC core interpreter, 240 opcodes, 78 native methods |
| 13 | Sedona VM Rust Porting | Actionable porting strategy — Cell type, opcode dispatch, native methods, 14-21 week estimate |
| 14 | Sedona VM Scalability Limits | 10 quantified bottlenecks — component ID overflow, 16KB stack, O(n^2) allocation, configurable Rust solutions |

### Future Protocols & Enhancements (Docs 15-19)

| Doc | Title | Focus |
|-----|-------|-------|
| 15 | SOX/WebSocket → ROX Protocol | SOX's 7 fundamental problems, Trio-over-WebSocket protocol, SCRAM-SHA-256, northbound mTLS clustering |
| 16 | roxWarp Protocol | Binary Trio (MessagePack), delta encoding with version vectors, Scuttlebutt gossip, Fantom pod for SkySpark |
| 17 | Name Length Analysis | Sedona 7-char limit, Sandstar 31-char patch, name interning recommendation (2 bytes/component) |
| 18 | Driver Framework v2 | Haxall-inspired `Driver` trait, `on_learn()` discovery, `PollScheduler`, LocalIoDriver with pure Rust HAL |
| 19 | Dynamic Slots | Side-car `DynSlotStore` for runtime metadata, LoRaWAN/Modbus/BACnet use cases, SOX virtual slot extension |

---

## 5. Completed Phases

### Phase Timeline

| Phase | Description | Date | Status |
|-------|-------------|------|--------|
| 0 | Workspace, HAL traits, MockHal | 2026-03-02 | Complete |
| 2 | Engine core (channels, tables, conversions, polls, watches) | 2026-03-02 | Complete |
| 2.5 | Engine<H> orchestration layer | 2026-03-02 | Complete |
| 3A-3H | Server binary, IPC, CLI, config loading, hardening, FD caching, Zinc wire | 2026-03-02 to 2026-03-04 | Complete |
| 1 | Haystack REST API (14 endpoints + Zinc, CORS, watches, filter) | 2026-03-04 | Complete |
| 4 | Sedona SVM FFI bridge, ChannelSnapshot, 22 native methods | 2026-03-04 | Complete |
| 5/5.5/5.6 | Deployment scripts, P0 hardening, performance | 2026-03-04 | Complete |
| 5.7 | Security (bind, bearer auth, filter depth, watch caps, rate limit, socket perms) | 2026-03-05 | Complete |
| 5.8 | Mock soak tests (6 integration tests simulating 1000+ poll cycles) | 2026-03-05 | Complete |
| 5.9 | Production cutover — C removed, Rust deployed to BeagleBone | 2026-03-05 | Complete |
| 5.10 | Post-deployment fixes — I2C protocol detection, ADC fault, exponential backoff, health CLI | Post-deploy | Complete |
| 6.0 | Full SVM integration, all Kit 4 native methods in Rust | 2026-03-05 | Complete |
| 6.5 | TLS (rustls), SCRAM-SHA-256, CORS whitelist, path sanitization | 2026-03-05 | Complete |
| 7.0 | Engine polish — virtual write docs, granular reload, I2C coalescing (270ms→45ms) | 2026-03-05 | Complete |
| 8.0A | Haystack-over-WebSocket (661 lines ws.rs, 31 tests) | 2026-03-04 | Complete |
| 8.0A-SOX | Full SOX/DASP protocol (pure Rust, 20 commands, 185 manifest types) | Post-8.0A | Complete |
| 8.0B | FileWrite + FileRename SOX commands | Post-SOX | Complete |
| 10.0A-E | Config-driven control (PID, sequencer, 20 components, .sax converter) | 2026-03-04 to 2026-03-05 | Complete |

### Production Deployment

- **Device:** BeagleBone Black (ARM Cortex-A8, 512MB RAM)
- **Target:** Todd Air Flow unit (30-113), 192.168.1.3:1919
- **Binary:** ~2.1 MB (server, stripped), ~580 KB (CLI, stripped)
- **Systemd:** Type=simple, MemoryLimit (Debian 9 / systemd 232 compatible)
- **Cross-compile:** Zig CC wrappers for ARM7, glibc 2.24 target

---

## 6. Key Technical Decisions

### 6.1 Sedona VM: FFI Bridge, Not Rewrite

The ~100,000-line Sedona VM remains in C, integrated via `extern "C"` FFI with 29 functions. Research Docs 12-13 provide a complete Rust porting strategy (14-21 weeks, ~5,800 lines) if needed in the future, but Phase 10.0's config-driven control engine (~400 lines Rust + TOML) makes the VM optional for the EacIo application.

### 6.2 libhaystack Over Custom Implementation

The `libhaystack` crate (J2 Innovations / Siemens) eliminated ~8,000 lines of C++ type infrastructure and ~3,067 lines of Zinc parsers with zero custom code. It provides Haystack 4 types, streaming Zinc/JSON encoding, and filter evaluation.

### 6.3 Config-Driven Control Over VM

~400 lines of Rust + `control.toml` replaces the 100K-line Sedona VM for the actual EacIo DDC application. 35 executable component types (Add2, Sub2, Mul2, Div2, Tstat, ConstFloat, PID, LeadSequencer, etc.) with dataflow engine, cycle detection, and visual wiring through Sedona Editor.

### 6.4 Fine-Grained Async Locking

The C++ system used a single global `Poco::RWLock` for all state. Rust uses separate `tokio::sync::RwLock` instances for records, watches, history, and channel index — reads don't block watches, history doesn't block reads.

### 6.5 Custom Rate Limiter

Atomic sliding-window rate limiter with zero additional dependencies, replacing the need for `tower::limit`.

### 6.6 I2C Coalescing

Pre-read cache reduces redundant I2C reads of the same sensor from 270ms to 45ms per poll cycle (6x improvement), replacing the C pthread thread pool with tokio tasks + per-bus `Mutex<BusState>`.

### 6.7 SOX/DASP: Pure Rust with Null-Terminated Strings

Critical wire format discovery: Sedona uses null-terminated strings (NOT length-prefixed), and Str encoding is `u2(size_including_null) + chars + 0x00`. The missing null terminator was the root cause of all COV subscription failures.

---

## 7. Quantitative Results

### 7.1 Binary Size

| Component | C/C++ (with POCO) | Rust |
|-----------|-------------------|------|
| Server binary | ~8-12 MB | ~2.1 MB (stripped) |
| CLI binary | — | ~580 KB (stripped) |
| POCO shared libs | ~8 MB (.so files) | 0 |
| Total install | ~25 MB | ~3-5 MB |

### 7.2 Performance

| Metric | C/C++ | Rust | Improvement |
|--------|-------|------|-------------|
| I2C poll cycle | 270 ms | 45 ms | 6x faster |
| Startup time | ~2-3 seconds | <1 second | 2-3x faster |
| Cold build | ~15 minutes | ~3-5 minutes | 3-5x faster |
| Incremental build | ~3 minutes | ~10-30 seconds | 20-60x faster |
| Static analysis | ~2-5 minutes | ~5-10 seconds | 20-30x faster |

### 7.3 Test Coverage

| Crate | Test Count |
|-------|-----------|
| sandstar-engine | ~200 |
| sandstar-server | ~280 |
| sandstar-hal-linux | 82 |
| sandstar-svm | ~50 |
| sandstar-hal | 9 |
| sandstar-ipc | 4 |
| **Total** | **1,637** |

### 7.4 API Surface

| Interface | Count |
|-----------|-------|
| REST endpoints | 20 (14 Haystack + health + metrics + zinc + WebSocket + rate-limit + auth) |
| SOX commands | 20/20 (complete DASP transport) |
| IPC commands | 12 |
| CLI commands | 10 |
| Control components | 35 executable types |
| Manifest types | 185 (from 15 kits) |

---

## 8. Safety and Security

### 8.1 Memory Safety: 45+ Vulnerabilities Eliminated

Doc 08 cataloged 45+ vulnerability sites across 13 bug classes in the C/C++ codebase. **All are structurally prevented by Rust's type system** — not by runtime checks, but by making the entire class impossible to express in safe code:

| Bug Class | C/C++ Sites | Rust Prevention |
|-----------|-------------|-----------------|
| Buffer overflow | 15+ | Bounds-checked indexing, `String` (heap, growable) |
| Data races | 5+ | `Send`/`Sync` traits (compile-time enforcement) |
| Use-after-free | 3+ | Borrow checker (lifetime tracking) |
| File descriptor leaks | 5+ | `File` implements `Drop` |
| Unchecked return values | 5+ | `Result<T, E>` is `#[must_use]` |
| Integer overflow | 3+ | `usize` + overflow checks in debug |
| Null pointer dereference | 3+ | `Option<T>` at compile time |
| Double free | 2+ | Ownership (Drop runs exactly once) |
| Format string vulnerabilities | 2+ | `format!` macro (compile-time validation) |
| Uninitialized variables | 2+ | Definite initialization analysis |
| Signal handler unsafety | 1 | `tokio::signal` (async-safe) |
| Missing return | 1+ | Compiler error (all paths must return) |
| Raw pointer IPC casting | 1 | Typed enum variants with serde |

### 8.2 Security Audit (All Resolved)

| Severity | Found | Issues | Resolution |
|----------|-------|--------|-----------|
| Critical | 3 | No auth, no TLS, no rate limiting | Bearer + SCRAM, rustls, atomic limiter |
| High | 3 | Filter depth, socket perms, watch cap | depth=32, mode=0660, cap=64 |
| Medium | 4 | PID perms, path traversal, log perms, CORS | 0600, canonicalize, umask, whitelist |
| **Total** | **10** | **All resolved** | **0 open** |

### 8.3 Notable Security Fix

The C++ code at `shaystack.cpp:130-131` contained a **buffer overflow bug**: `message.copy(buf, buf_len)` with no length check, followed by `buf[message.length()] = '\0'` writing past `buf_len`. This was identified in Doc 06 and fixed in the Rust FFI implementation with explicit bounds checking.

---

## 9. Future Roadmap

Based on Docs 12-19, the following enhancements are designed and documented but **not yet implemented**:

### 9.1 Phase 9.0 — ROX Protocol & roxWarp Clustering (Docs 15-16)

**Goal:** Replace SOX/UDP/DASP with modern WebSocket-based protocols.

| Sub-Phase | Description | Estimated LOC |
|-----------|-------------|---------------|
| ROX dual-stack | SOX + ROX (Trio-over-WebSocket) coexistence | ~2,600 |
| TLS + roxWarp | mTLS cluster foundation | ~3,500 |
| Gossip | Scuttlebutt state replication with version vectors | ~3,000 |
| SOX deprecation | Optional removal of legacy protocol | ~500 |
| **Total** | | **~9,600** |

**Key design elements:**
- Trio encoding (~66% smaller than Haystack 4 JSON)
- Binary Trio via MessagePack for device-to-device gossip (~89% smaller than JSON)
- SCRAM-SHA-256 authentication (Haxall-compatible)
- Delta encoding with version vectors for efficient COV propagation
- mTLS with per-node RSA certificates signed by cluster CA

### 9.2 Phase 11.0 — Sedona VM Rust Port (Docs 12-14)

**Goal:** Optionally replace the C VM with a pure Rust implementation.

| Aspect | Current (C) | Rust Target |
|--------|-------------|-------------|
| Core interpreter | 1,281 LOC | ~1,100 LOC |
| Native methods | 4,880 LOC (24 files) | ~3,000 LOC |
| Total C to convert | ~8,926 LOC | ~5,800 LOC |
| Opcodes | 240 | 240 (all required) |
| Stack overflow detection | Debug-only | Always-on |
| Component ID limit | 32,767 (signed short) | 4 billion (u32) |
| VM stack | 16 KB fixed | Configurable (default 64 KB) |
| Tree walk | Recursive (stack overflow risk) | Iterative (unlimited depth) |
| Effort estimate | — | 14-21 weeks |

**Scalability improvements** (Doc 14): The current system degrades between 2,000-10,000 components. The Rust architecture supports 100,000+ components via iterative tree walk, free-list allocation (O(1) vs O(n^2)), HashMap lookups (O(1) vs O(n)), and configurable limits.

### 9.3 Phase 12.0 — Driver Framework v2 (Doc 18)

**Goal:** Replace the C engine + C++ REST stack with a pure Rust Haxall-inspired driver architecture.

- `Driver` trait with `on_open()`, `on_close()`, `on_ping()`, `on_learn()`, `on_sync_cur()`, `on_watch()`, `on_write()`
- `DriverManager` as Tokio actor with `PollScheduler` and `WatchManager`
- `LocalIoDriver` using pure Rust HAL crates (gpio-cdev, i2cdev, industrial-io, sysfs-pwm, serialport)
- Protocol drivers: Modbus (TCP+RTU), BACnet (IP/MSTP), MQTT
- Estimated timeline: 12 weeks

### 9.4 Phase 13.0 — Dynamic Slots (Doc 19)

**Goal:** Attach arbitrary key-value metadata to Sedona components at runtime without modifying the VM.

- Side-car `DynSlotStore` pattern — `HashMap<CompId, Dict>`
- Zero overhead for components without dynamic slots
- LoRaWAN, Modbus, BACnet, MQTT use cases
- SOX virtual slot extension (IDs 200-254)
- Zinc persistence with debounced writes
- Estimated: 6 weeks across 5 sub-phases

### 9.5 Name Interning (Doc 17)

**Goal:** Replace fixed 32-byte component names with interned 2-byte IDs.

- `NameInternTable` with `RwLock<Vec<String>>` + `DashMap<String, u16>`
- 62% memory reduction vs 32-byte inline names
- No length limit on component names
- Backward-compatible with both 8-byte and 32-byte `.sab` formats

---

## 10. Risk Assessment

### 10.1 Resolved Risks

| Risk | Resolution |
|------|-----------|
| POCO dependency (500K LOC) | Eliminated — replaced by 25 Cargo crates |
| Memory safety (45+ bug sites) | Structurally eliminated by Rust type system |
| No authentication | Bearer token + SCRAM-SHA-256 implemented |
| No encryption | TLS via rustls (optional feature flag) |
| I2C performance on ARM | Coalescing cache: 270ms → 45ms |
| SOX protocol complexity | Pure Rust implementation, 20/20 commands, all tests passing |
| ARM cross-compilation | Zig CC wrappers, GLIBC 2.24 target, GitHub Actions CI |

### 10.2 Current / Ongoing Risks

| Risk | Severity | Mitigation |
|------|----------|-----------|
| SDP810 I2C sensor hardware failure | Low | Physical sensor replacement needed; software handles gracefully with exponential backoff |
| libhaystack API breaking change | Low | Pinned to exact version |
| Single production device | Medium | Second device (Baha, 211-135) unreachable from Windows development host |
| SOX retained indefinitely | Low | Legacy compatibility; ROX protocol designed as eventual replacement |

### 10.3 Future Enhancement Risks

| Risk | Phase | Probability | Impact | Mitigation |
|------|-------|-------------|--------|-----------|
| VM Rust port performance regression | 11.0 | 30% | High | Proof-of-concept Phase 1 before full commitment |
| Scode compatibility bugs | 11.0 | 40% | High | Byte-for-byte A/B testing against C VM |
| roxWarp mTLS on ARM | 9.0 | Low | Medium | `rustls` verified for ARM; `rcgen` needs `aws-lc-rs` backend |
| Driver Framework `block_on` deadlock | 12.0 | Medium | High | Careful Tokio runtime handle management |
| Dynamic slot memory pressure | 13.0 | Low | High | Configurable limits (default 8 MB cap) |

---

## Appendix A: Cross-Compilation Setup

```bash
# Environment
export PATH="$HOME/.cargo/bin:$PATH"
export CC_armv7_unknown_linux_gnueabihf="C:\\czb\\zigcc-arm-cc.bat"
export AR_armv7_unknown_linux_gnueabihf="C:\\czb\\zigar-arm.bat"

# Build
cargo build --target armv7-unknown-linux-gnueabihf --release

# Package
cargo deb --target armv7-unknown-linux-gnueabihf --no-strip

# Deploy
tools/installSandstarRust.sh 30-113
```

## Appendix B: Key File Paths

| Path | Content |
|------|---------|
| `ssCompile/ssCompile/sandstar_rust/` | Rust workspace root |
| `sandstar_rust/crates/` | 7 crate sources |
| `sandstar_rust/docs/research/` | 20 research documents |
| `sandstar_rust/test_logs/` | Test result archives |
| `sandstar_rust/tools/` | Deployment and validation scripts |
| `shaystack/sandstar/sandstar/EacIo/` | Production config (points.csv, tables.csv, database.zinc) |
| `shaystack/sandstar/sandstar/usr/local/config/` | ADC lookup tables (*.txt) |

## Appendix C: Document Cross-References

```
Doc 00 (Executive Summary)
 ├── References all docs (master overview)
 └── Defines phase timeline

Docs 01-04 (Core Migration)
 ├── Doc 01 (Engine)    → feeds into Doc 07 (IPC), Doc 11 (Roadmap Phase 2)
 ├── Doc 02 (Drivers)   → feeds into Doc 18 (Driver Framework v2)
 ├── Doc 03 (Haystack)  → feeds into Doc 05 (Zinc), Doc 09 (Dependencies)
 └── Doc 04 (REST API)  → feeds into Doc 15 (ROX), Doc 18 (Driver Framework v2)

Docs 05-09 (Infrastructure)
 ├── Doc 05 (Zinc)      → feeds into Doc 06 (FFI uses libhaystack c-api)
 ├── Doc 06 (FFI)       → feeds into Doc 07 (IPC), Doc 13 (VM Porting)
 ├── Doc 07 (IPC)       → feeds into Doc 14 (Scalability: IPC buffer limits)
 ├── Doc 08 (Safety)    → justifies entire migration
 └── Doc 09 (Deps)      → defines final Cargo.toml

Docs 10-14 (Build & VM)
 ├── Doc 10 (Build)     → enables Doc 11 (Roadmap deployment)
 ├── Doc 11 (Roadmap)   → master plan referencing Docs 01-09
 ├── Doc 12 (VM Arch)   → feeds into Doc 13 (Porting Strategy)
 ├── Doc 13 (VM Port)   → feeds into Doc 14 (Scalability fixes)
 └── Doc 14 (Scalability) → motivates Doc 18 (Driver Framework v2)

Docs 15-19 (Future)
 ├── Doc 15 (ROX)       → feeds into Doc 16 (roxWarp binary encoding)
 ├── Doc 16 (roxWarp)   → feeds into Doc 19 (Dynamic Slots gossip propagation)
 ├── Doc 17 (Names)     → feeds into Doc 13 (VM component identity)
 ├── Doc 18 (Drivers v2) → feeds into Doc 19 (Dynamic Slots driver integration)
 └── Doc 19 (DynSlots)  → integrates with Docs 15, 16, 18
```

---

*Generated from analysis of 20 research documents in `sandstar_rust/docs/research/`*
*Report date: 2026-04-09 | Sandstar Rust v1.4.0*
