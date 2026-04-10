# Sandstar Rust Migration - Comprehensive Status Report

**Date:** 2026-04-10 (Updated)
**Version:** 1.6.0
**Status:** Production Deployed
**Repository:** [Project-SandStar/Sandstar_Rust](https://github.com/Project-SandStar/Sandstar_Rust)

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Migration Scope and Achievements](#2-migration-scope-and-achievements)
3. [Architecture Overview](#3-architecture-overview)
4. [Engine Internals](#4-engine-internals)
5. [Server and Protocol Stack](#5-server-and-protocol-stack)
6. [Sedona VM Implementation](#6-sedona-vm-implementation)
7. [CI/CD and Deployment Infrastructure](#7-cicd-and-deployment-infrastructure)
8. [Documentation Suite](#8-documentation-suite)
9. [Research Document Index](#9-research-document-index)
10. [Completed Phases](#10-completed-phases)
11. [Key Technical Decisions](#11-key-technical-decisions)
12. [Quantitative Results](#12-quantitative-results)
13. [Safety and Security](#13-safety-and-security)
14. [Current Work in Progress](#14-current-work-in-progress)
15. [Future Roadmap](#15-future-roadmap)
16. [Risk Assessment](#16-risk-assessment)

---

## 1. Executive Summary

The Sandstar engine has been **fully rewritten from C/C++ to Rust** and deployed to production on a BeagleBone ARM device (192.168.1.3). The migration replaced ~32,314 lines of custom C/C++ and eliminated ~550,000 lines of vendored dependencies (POCO framework + Boost), producing a Rust codebase of **~85,346 lines across 123 source files in 7 workspace crates** with **2,319+ tests and 0 failures**.

The rewrite delivered significantly more than a straight port. Beyond full feature parity, it added: TLS encryption, SCRAM-SHA-256 authentication, rate limiting, WebSocket push, I2C coalescing (270ms to 45ms), config-driven DDC control, a full SOX/DASP protocol implementation (20/20 commands), visual wiring through the Sedona Editor, a pure Rust VM interpreter (90% complete), roxWarp clustering infrastructure, protocol drivers (Modbus/BACnet/MQTT), an alert system, Prometheus-style metrics, a conversion filter pipeline, and a SAX-to-TOML converter.

Twenty research documents (Docs 00-19) were produced during the migration, documenting every architectural decision. This report synthesizes all twenty and cross-references them against the actual codebase state.

---

## 2. Migration Scope and Achievements

### 2.1 Code Replaced vs Produced

| Category | C/C++ Lines | Rust Lines | Notes |
|----------|------------|------------|-------|
| Engine core | ~5,500 | ~9,260 | Expanded: PID, sequencer, 21 component types, filter pipeline |
| Hardware drivers | ~2,760 | ~3,485 | Expanded: CRC, sysfs helpers, simulator HAL |
| Haystack types | ~8,000 | 0 | Eliminated by `libhaystack` crate |
| Server + REST + SOX + roxWarp | ~6,900 | ~49,381 | Massively expanded: SOX (8,704 LOC), roxWarp (5K), auth, alerts, drivers |
| Sedona VM | (stayed as C FFI) | ~20,947 | Pure Rust interpreter, 80 native methods, component store |
| IPC bridge | ~1,156 | ~647 | Simplified via typed enums |
| CLI tools | ~2,266 | ~965 | Consolidated into single binary |
| Zinc/CSV parsers | ~3,067 | ~100 | Eliminated by `libhaystack` + `csv` crate |
| **Custom C/C++ total** | **~32,314** | — | — |
| **Total Rust produced** | — | **~85,346** | 123 files across 7 crates |
| POCO (vendored) | ~500,000 | 0 | Eliminated |
| Boost (vendored) | ~50,000 | 0 | Eliminated |

### 2.2 Feature Parity and Enhancements

- **99% feature parity** with the original C/C++ system (80/80 features matched)
- **30+ additional capabilities** not present in the original:
  - TLS via rustls (optional feature flag)
  - SCRAM-SHA-256 + bearer token dual authentication
  - Atomic sliding-window rate limiter (100 req/sec default, configurable)
  - WebSocket push with SCRAM auth and COV subscriptions
  - Full SOX/DASP protocol (pure Rust, 20/20 commands, 8,704 LOC)
  - 185 manifest type definitions parsed from XML
  - Config-driven DDC control (PID, sequencer, 35 executable component types)
  - I2C read coalescing with exponential backoff (270ms to 45ms, 6x speedup)
  - Cycle detection in component link graphs (DFS-based)
  - Component persistence (sox_components.json, auto-save every 5s)
  - Channel-to-logic bridge (ConstFloat proxy for live sensor data)
  - FileWrite + FileRename SOX commands
  - SAX converter (1,275 LOC) — Sedona XML to TOML transpiler
  - CORS middleware with proper preflight handling
  - Graceful shutdown (in-flight request completion)
  - Hot-reload via SIGHUP and REST endpoint
  - Fine-grained async locking (reads don't block watches)
  - Prometheus-style metrics endpoint (requests, WS, errors)
  - Alert/event notification system
  - In-memory history ring buffer per channel
  - Structured logging via `tracing` with log rotation
  - Systemd integration (Type=simple, sd_notify, core dumps)
  - Hardware watchdog integration (/dev/watchdog)
  - Simulator HAL for deterministic testing
  - Dynamic tag dictionaries per component (dyn_slots)
  - Name interning for component names
  - Conversion filter pipeline (spike, smoothing, rate-limiting)
  - roxWarp clustering with delta sync and mTLS (infrastructure built)
  - Protocol driver modules (Modbus TCP/RTU, BACnet, MQTT, LocalIO)
  - Pure Rust VM interpreter with 150+ opcodes (90% complete)

---

## 3. Architecture Overview

### 3.1 Workspace Structure (7 Crates)

| Crate | Files | Lines | Purpose | Tests |
|-------|-------|-------|---------|-------|
| `sandstar-server` | 65 | 49,381 | REST API, WebSocket, SOX/DASP, roxWarp, auth, drivers, control | ~1,209 |
| `sandstar-svm` | 27 | 20,947 | Pure Rust VM interpreter, native methods, component store | ~752 |
| `sandstar-engine` | 17 | 9,260 | Channels, tables, conversions, PID, sequencer, 21 components | ~284 |
| `sandstar-hal-linux` | 8 | 3,485 | Linux sysfs drivers: GPIO, ADC, I2C, PWM, UART, CRC | ~114 |
| `sandstar-cli` | 2 | 965 | CLI with 10 subcommands (read, write, subscribe, validate, etc.) | ~28 |
| `sandstar-hal` | 1 | 661 | HAL traits (HalRead, HalWrite, HalControl, HalDiagnostics) + SimulatorHal | ~19 |
| `sandstar-ipc` | 3 | 647 | Shared IPC types + bincode wire protocol | ~17 |
| **Total** | **123** | **85,346** | | **2,319+** |

### 3.2 Top 10 Largest Source Files

| File | Lines | Purpose |
|------|-------|---------|
| `sox/sox_handlers.rs` | 8,704 | Full SOX/DASP command dispatch + component tree operations |
| `vm_interpreter.rs` | 2,545 | Pure Rust opcode interpreter (150+ opcodes) |
| `engine.rs` | 2,386 | 16-step read pipeline, 9-step write pipeline |
| `rest/rows.rs` | 1,941 | RoWS WebSocket (live component tree) |
| `control.rs` | 1,827 | Config-driven DDC: PID, sequencer, component execution |
| `sox/dasp.rs` | 1,657 | DASP UDP transport, reliability, session management |
| `sax_converter.rs` | 1,275 | Sedona XML to TOML converter |
| `components.rs` | 1,469 | 21 executable component types |
| `auth.rs` | 1,199 | SCRAM-SHA-256 (RFC 5802) + bearer tokens |
| `roxwarp/handler.rs` | 1,079 | WebSocket peer gossip handler |

### 3.3 Runtime Modes

| Mode | Command | Description |
|------|---------|-------------|
| Demo | `cargo run -p sandstar-server` | 5 demo channels + MockHal |
| Sedona | `cargo run -p sandstar-server -- --sedona --scode-path <path>` | Full Sedona VM integration |
| Production | `SANDSTAR_CONFIG_DIR=".../EacIo" cargo run -p sandstar-server` | 140 channels + 16 lookup tables |
| Simulation | `cargo run -p sandstar-server -- --features simulator-hal` | REST-injectable test values |
| Read-Only | `cargo run -p sandstar-server -- --read-only` | Validation mode (no writes) |

### 3.4 Compile-Time Feature Flags

| Feature | Purpose |
|---------|---------|
| `mock-hal` (default) | MockHal for development/testing |
| `linux-hal` | Real Linux sysfs hardware drivers |
| `simulator-hal` | Shared-state injection via REST |
| `svm` | Enable Sedona VM integration |
| `tls` | Enable HTTPS via rustls |

### 3.5 Dependency Strategy

28 direct Rust crate dependencies replace POCO + Boost + custom parsers:

| Domain | Key Crates |
|--------|-----------|
| HTTP/API | axum, axum-server, tower, tower-http (CORS, tracing), hyper |
| Async runtime | tokio (rt, net, time, sync, signal, macros, io-util) |
| WebSocket | tokio-tungstenite |
| Serialization | serde, bincode, serde_json, rmp-serde, quick-xml, toml |
| Security | rustls, rustls-pemfile, webpki-roots, hmac, sha1, sha2, pbkdf2, base64, rand |
| Observability | tracing, tracing-subscriber, tracing-appender |
| Hardware | nix, libc (via sandstar-hal-linux) |
| CLI | clap (derive) |
| Utilities | thiserror, async-trait, smallvec, bitflags, fs2, chrono |

---

## 4. Engine Internals

### 4.1 Channel System

**Channel Types:** Analog, Digital, PWM, Triac, I2C, UART, VirtualAnalog, VirtualDigital
**Channel Directions:** In, Out, High, Low, None
**Virtual Channels:** Store values locally; can read from source via `channel_in` linkage

### 4.2 Engine Read Pipeline (16 Steps)

1. Lookup channel by ID
2. Disabled check (skip if disabled)
3. Failed/retry cooldown (exponential backoff for I2C: base 30s, max 300s)
4. Snapshot immutable fields
5. HAL dispatch by channel type (Analog, Digital, PWM, I2C, UART, Virtual)
6. HAL error handling → Down status
7. Set raw value
8. SDP810 garbage detection (reject raw > 32767 or < 0)
9. Auto-detect sensor type (channels 1100-1723 → table lookup)
10. Convert raw→cur via table interpolation or range scaling
11. SDP810 spike detection (reject >5x change from baseline)
12. Tag-based spike filter
13. Smoothing (Mean, Median, EWMA — window up to 10 samples)
14. Rate limiting (separate `max_rise`/`max_fall` per second)
15. Trigger on zero
16. Store and return

### 4.3 Engine Write Pipeline (9 Steps)

1. Lookup channel
2. Direction check (output or virtual only)
3. Disabled check
4. Convert/revert based on flags (RAW↔CUR)
5. HAL dispatch with validation
6. HAL error handling
7. Update channel value
8. Return result

### 4.4 Value Conversion System

Three conversion paths:
- **Table Lookup:** Binary search + linear interpolation for non-linear sensors (thermistors, RTDs)
- **Range Scaling:** Linear interpolation within raw low/high → engineering min/max
- **Conversion Functions:** SDP610 flow sensor physics (raw→Pa→inH2O→CFM with K-factor)

Auto-detection by channel ID (XXYY format): channels 1100-1723 automatically select the correct sensor table.

### 4.5 Conversion Filter Pipeline

Applied in sequence after value conversion:
1. **Spike Filter:** Reject readings exceeding threshold (e.g., >5x previous baseline)
2. **Smoothing Filter:** Mean, Median, or EWMA (exponential weighted moving average) with configurable window (up to 10 samples)
3. **Rate Limiter:** Separate `max_rise` and `max_fall` limits per second — prevents sudden jumps

### 4.6 Component Types (21 in Engine)

| Category | Types |
|----------|-------|
| Arithmetic | Add2, Sub2, Mul2, Div2, Neg, Round, FloatOffset |
| Logic | And2, Or2, Not |
| Timing/Control | SRLatch, DelayOn, DelayOff, OneShot, Ramp |
| HVAC | Thermostat, Hysteresis |
| Scheduling | ScheduleEntry, DailyScheduleFloat, BoolScheduleEntry, DailyScheduleBool |

### 4.7 PID Controller

- Gains: Kp, Ki (resets/min), Kd
- Output limits: out_min/max, bias, max_delta (rate limiting)
- Action: Direct or Reverse
- Anti-windup: Integral clamped to output range
- Configurable execution interval (default 1000ms)

### 4.8 Lead Sequencer

- 1-16 stages (clamped)
- Hysteresis prevents rapid cycling (default 0.5 = 50% band width)
- Equal-band threshold division
- Progressive staging with dead-band

### 4.9 Priority Array

BACnet-style 17-level priority write system per channel:
- Lazy-allocated (no overhead until first write)
- Timed writes with auto-relinquish after duration
- Eager expiration check on read

### 4.10 I2C Reliability Features

- **Coalescing:** Cache key = (device, address, label) for bulk reads — 6 SDP810 sensors in one HAL call
- **Exponential Backoff:** Per-sensor recovery with doubling cooldown (30s → 60s → 120s → 300s max)
- **SDP810 Garbage Detection:** Reject raw values > 32767 or < 0
- **SDP810 Spike Detection:** Reject readings >5x change from baseline, with drop-to-zero protection
- **CRC-8 Validation:** Per-packet integrity check for I2C data

---

## 5. Server and Protocol Stack

### 5.1 REST API Endpoints (24+)

**Public Read-Only (no auth required):**

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/` | GET | Dashboard HTML embed |
| `/editor` | GET | DDC visual editor HTML |
| `/api/about` | GET | Server version + boot time |
| `/api/ops` | GET | Available operations metadata |
| `/api/formats` | GET | Supported data formats |
| `/api/read` | GET | Haystack filter query |
| `/api/status` | GET | Engine status summary |
| `/api/health` | GET | Health check (200 OK) |
| `/api/metrics` | GET | Prometheus-style metrics |
| `/api/diagnostics` | GET | Poll timing, I2C backoff, channel health |
| `/api/channels` | GET | All channels with current values |
| `/api/polls` | GET | Active poll schedule |
| `/api/tables` | GET | Lookup table definitions |
| `/api/pointWrite` | GET | Point priority level info |
| `/api/history/{channel}` | GET | Historical values from ring buffer |
| `/api/ws` | GET | WebSocket upgrade (real-time push) |

**Protected (auth required if configured):**

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/api/pointWrite` | POST | Write value with priority level + holdtime |
| `/api/pollNow` | POST | Trigger immediate poll cycle |
| `/api/reload` | POST | Hot-reload configuration |
| `/api/watchSub` | POST | Subscribe to COV events |
| `/api/watchUnsub` | POST | Unsubscribe from watch |
| `/api/watchPoll` | POST | Poll for changed values |
| `/api/hisRead` | POST | Historical data range query |
| `/api/nav` | POST | Navigate component tree |
| `/api/invokeAction` | POST | Execute component action |

### 5.2 WebSocket Protocol (`/api/ws`)

**Configuration:** Max 32 concurrent connections, 120s client timeout, 200ms-60s poll interval

**Client → Server Messages:**
`auth`, `hello` (SCRAM step 1), `authenticate` (SCRAM step 3), `subscribe`, `unsubscribe`, `refresh`, `ping`

**Server → Client Messages:**
`authOk`, `challenge` (SCRAM step 2), `subscribed` (initial values), `update` (COV delta), `snapshot`, `unsubscribed`, `pong`, `error`

### 5.3 SOX/DASP Protocol (20/20 Commands)

**Transport:** UDP port 1876, SCRAM-SHA-256 auth, 30s session timeout, 1400-byte max datagram

| Category | Commands |
|----------|----------|
| Queries | ReadSchema (`v`/`V`), ReadVersion (`y`/`Y`), ReadComp (`c`/`C`), ReadProp (`r`/`R`) |
| Subscriptions | Subscribe (`s`/`S`), Unsubscribe (`u`/`U`) |
| Mutations | Write (`w`/`W`), Invoke (`k`/`i`/`K`/`I`), Add (`a`/`A`), Delete (`d`/`D`), Rename (`n`/`N`), Reorder (`o`/`O`), Link (`l`/`L`) |
| File Transfer | FileOpen (`f`/`F`), FileRead (`g`/`G`), FileWrite (`h`/`H`), FileClose (`z`/`q`/`Z`/`Q`), FileRename (`b`/`B`) |
| Events | Event (`e`) — server-push COV notifications |

**Component Tree Virtual Hierarchy:**
```
App (0)
├── service (1) → sox (2), users (3), plat (4)
├── io (5) → ch_1113 (100), ch_1713 (101), ... (up to 2048)
└── control (6) → (PID/sequencer mapping)
```

### 5.4 roxWarp Clustering (Infrastructure Built)

**9 modules, ~5,000 lines** — enabled via `--cluster` flag

| Module | Lines | Purpose |
|--------|-------|---------|
| `handler.rs` | 1,079 | WebSocket peer gossip handler |
| `protocol.rs` | 830 | Binary cluster protocol |
| `cluster.rs` | 795 | Cluster coordination |
| `binary_trio.rs` | 618 | Binary Trio encoding (MessagePack) |
| `peer.rs` | 497 | Peer connection management |
| `delta.rs` | 487 | Version vector + delta sync |
| `string_table.rs` | 389 | String compression for updates |
| `mtls.rs` | 219 | Mutual TLS certificate handling |
| `mod.rs` | 89 | Cluster state management |

**Endpoints:** `/roxwarp` (WebSocket gossip), `/api/cluster/status`, `/api/cluster/query`

### 5.5 Protocol Driver Modules

| Driver | File | Protocol |
|--------|------|----------|
| `local_io.rs` | GPIO/ADC/PWM hardware abstraction | sysfs |
| `modbus.rs` | Modbus TCP/RTU | Industrial |
| `bacnet.rs` | BACnet/IP integration | Building automation |
| `mqtt.rs` | MQTT publish/subscribe | IoT messaging |
| `poll_scheduler.rs` | Poll timing and batching | Internal |

### 5.6 Additional Server Features

- **Alert System** (`alerts.rs`, 1,034 LOC): Event notification with configurable thresholds
- **In-Memory History** (`history.rs`): Ring buffer per channel, queryable via REST, configurable retention (default 10K points)
- **Prometheus Metrics** (`metrics.rs`): Atomic counters for REST requests, WebSocket connections, messages, errors
- **Dynamic Tags** (`sox/dyn_slots.rs`): Per-component key-value metadata persisted to `dyn_slots.json`, auto-cleaned on delete
- **Name Interning** (`sox/name_intern.rs`): String interning for component names
- **SAX Converter** (`sax_converter.rs`, 1,275 LOC): Sedona XML → TOML transpiler (parses PID, LSeq, math/logic blocks)
- **Hardware Watchdog** (`watchdog.rs`): Periodic ping to `/dev/watchdog`, disabled in read-only mode
- **Signal Handling** (`signal.rs`): SIGPIPE ignored, SIGHUP → reload, SIGTERM/SIGINT → graceful shutdown
- **Systemd Notify** (`sd_notify.rs`): Socket activation, readiness signaling, core dump enablement

---

## 6. Sedona VM Implementation

### 6.1 Current Status: 90% Complete (Pure Rust)

The Sedona VM has been largely ported from C to pure Rust (20,947 lines, 27 files). This was originally planned as a distant Phase 11.0 but has been substantially implemented.

| Component | File | Lines | Status |
|-----------|------|-------|--------|
| Opcode Interpreter | `vm_interpreter.rs` | 2,545 | Complete (150+ opcodes) |
| Interpreter Tests | `interpreter_tests.rs` | 2,020 | 752 tests |
| Component Store | `component_store.rs` | — | Complete |
| Image Loader | `image_loader.rs` | — | Complete |
| SAB Validator | `sab_validator.rs` | — | Complete |
| Bridge (HAL) | `bridge.rs` | — | Complete |
| Runner | `runner.rs` | — | Complete |
| FFI Glue | `ffi.rs` | — | Complete |
| VM Memory | `vm_memory.rs` | — | Complete |
| VM Stack | `vm_stack.rs` | — | Complete |
| Opcodes | `opcodes.rs` | — | Complete |

### 6.2 Native Method Kits (80 Methods)

| Kit | File | Methods |
|-----|------|---------|
| sys | `native_sys.rs`, `native_component.rs` | System calls, component access |
| file | `native_file.rs` | File I/O operations |
| inet | `native_inet.rs` | TCP/UDP network sockets |
| serial | `native_serial.rs` | Serial port communication |
| EacIo | `native_eacio.rs` | Engine/I2C bridge |
| datetime | `native_datetime.rs` | Date/time functions (via chrono) |
| table | `native_table.rs` | Lookup table queries |

### 6.3 Key Safety Improvements Over C VM

| Issue | C VM | Rust VM |
|-------|------|---------|
| Stack overflow | Debug-only check | Always bounds-checked |
| Null pointers | Runtime crash | `Option<T>` at compile time |
| Memory leaks | Manual tracking | RAII `Drop` |
| Buffer overruns | Possible | Bounds-checked slices |
| Thread safety | Manual mutex | `Send`/`Sync` traits |

---

## 7. CI/CD and Deployment Infrastructure

### 7.1 GitHub Actions CI Pipeline

**File:** `.github/workflows/ci.yml` (82 lines)

**Test Job** (on every push/PR):
1. `cargo fmt --check` — formatting
2. `cargo clippy -- -D warnings` — lint (strict, warnings = errors)
3. `cargo test --workspace` — all 2,319+ tests

**Build ARM Job** (on push to master, after tests pass):
1. Install `armv7-unknown-linux-gnueabihf` target
2. Install Zig + `cargo-zigbuild`
3. Build `sandstar-server` and `sandstar-cli` with `--features linux-hal`

### 7.2 Deployment Scripts (16 Scripts in `tools/`)

| Script | Purpose |
|--------|---------|
| `build-rust.sh` | ARM cross-compile (replaces 303-line C build script) |
| `build-sedona-sox.sh` | Build Sedona kit manifests for SOX protocol |
| `installSandstarRust.sh` | Deploy .deb to BeagleBone via scp |
| `deploy-todd.sh` | Todd Air Flow (30-113) specific deployment |
| `deploy-baha.sh` | Baha device (211-135) specific deployment |
| `validate-engines.sh` | Compare C vs Rust engine side-by-side (REST polling, discrepancy logging) |
| `soak-monitor.sh` | Long-running stability test (memory/CPU/uptime, 4-8 hours) |
| `cutover-to-rust.sh` | Switch production from C to Rust (health check, backup, swap, verify) |
| `rollback-to-c.sh` | Emergency revert to C engine |
| `start.sh` / `stop.sh` / `restart.sh` | systemctl wrappers |
| `health-monitor.sh` | Continuous health checks (API, memory, CPU, errors) |
| `sim.sh` | Simulation/demo mode startup |
| `basemulator-bridge.py` | BACnet emulator integration (Python bridge) |
| `basemulator-mapping.json` | BACnet point mapping configuration |

### 7.3 Systemd Configuration (`etc/`)

- `sandstar-engine.service` — Main service file
- `sandstar-rust-validate.service` — Validation service (read-only mode)
- `initialize.sh` — Hardware initialization
- `logrotate.d/sandstar` — Log rotation configuration

### 7.4 Server CLI Arguments (25 Options)

| Flag | Default | Purpose |
|------|---------|---------|
| `--config-dir (-c)` | required | Configuration directory |
| `--poll-interval-ms (-p)` | 1000 | Sensor poll frequency (ms) |
| `--log-level` | info | Tracing filter |
| `--http-port` | 8085 | REST API port |
| `--http-bind` | 127.0.0.1 | Bind address |
| `--auth-token` | none | Bearer token |
| `--auth-user` / `--auth-pass` | none | SCRAM credentials |
| `--rate-limit` | 100 | Requests/sec (0 = unlimited) |
| `--read-only` | false | Validation mode |
| `--no-rest` | false | Disable REST API |
| `--no-control` | false | Disable PID/sequencer |
| `--tls-cert` / `--tls-key` | none | HTTPS PEM files |
| `--sox` | false | Enable SOX protocol |
| `--sox-port` | 1876 | SOX UDP port |
| `--sox-user` / `--sox-pass` | admin | SOX credentials |
| `--cluster` | false | Enable peer clustering |
| `--cluster-config` | none | Peer addresses JSON |
| `--cluster-port` | 7443 | Cluster listener port |
| `--cluster-cert` / `--cluster-key` / `--cluster-ca` | none | mTLS certificates |
| `--sedona` | false | Enable Sedona VM |

---

## 8. Documentation Suite

### 8.1 Non-Research Documentation (4 Files in `docs/`)

| Document | Purpose |
|----------|---------|
| `ROADMAP_v2.md` (21,343 tokens) | Complete versioned roadmap with phase status |
| `DEPLOYMENT_CHECKLIST.md` (449 lines) | 4-phase deployment guide: Install → Soak → Cutover → Monitor |
| `HARDCODED_LIMITS.md` | Audit of all hardcoded constants with recommendations |
| `PURE_RUST_PLAN.md` (212 lines) | Pure Rust VM implementation plan (15 files, ~7,730 new lines) |

### 8.2 Validation Runbook (`tools/validation-runbook.md`, 358 Lines)

- Pre-deployment checklist (8 items)
- Step-by-step deployment (4 steps)
- Manual validation tests (6 tests: status, channels, polls, read/write, history, performance)
- Automated validation via `validate-engines.sh`
- Soak test protocol (4-8 hours: memory monitoring, alert detection, >99% match rate)
- Troubleshooting guide (common failures)
- Success criteria (24-hour post-cutover: 0 alerts, <30MB memory, 0 restarts)
- Emergency rollback procedure

### 8.3 HowToUse Guides (14 Files in `HowToUses/`)

| Guide | Topics |
|-------|--------|
| 01 Server Basics | Demo mode, SOX protocol, real config, production, ports |
| 02 REST API | All 14+ Haystack endpoints |
| 03 Web Editor | Visual DDC programming, component palette, wiring, live data |
| 04 SOX Protocol | DASP transport, 20 commands, manifest loading |
| 05 RoWS Protocol | SOX over WebSocket, browser integration |
| 06 roxWarp Cluster | Device clustering, gossip, state replication |
| 07 Deployment | ARM cross-compile, scp/systemd install, BeagleBone setup |
| 08 CLI Commands | sandstar-cli subcommands (status, channels, read, write, reload, convert-sax) |
| 09 Alerts | Alert configuration, thresholds, notifications |
| 10 Dashboard | Web dashboard features, real-time updates, historical trends |
| 11 Driver Framework | Driver trait, on_learn() discovery, PollScheduler |
| 12 Dynamic Tags | Runtime metadata for LoRaWAN/Modbus/BACnet |
| 13 Sedona VM | Interpreter, native kits, FFI bridge, .scode compilation |

### 8.4 Example Configurations (7 Files in `examples/`)

| File | Purpose |
|------|---------|
| `control.toml` | Full DDC control (PID loops, 20+ component types) |
| `control_sim.toml` | Simulator configuration |
| `control_virtual_test.toml` | Virtual test setup |
| `database_virtual_test.zinc` | Test database |
| `alerts.json` | Alert definitions |
| `scenario_cooling.json` | Cooling simulation scenario |
| `scenario_morning_warmup.json` | Morning warmup scenario |

---

## 9. Research Document Index

The 20 research documents form a complete technical record of the migration:

### Foundation & Core Migration (Docs 00-04)

| Doc | Title | Focus |
|-----|-------|-------|
| 00 | Executive Summary | Full project overview, phase timeline, crate structure, security audit |
| 01 | Engine Core Analysis | C engine (5,507 LOC) to Rust mapping — 73% reduction, data structure transformations |
| 02 | Hardware Drivers | 8 C driver files (2,760 LOC) to Rust — 65% reduction, GPIO chardev upgrade, async I2C/UART |
| 03 | Haystack Type System | 66 C++ files (8,000 LOC) eliminated by `libhaystack` — Dict mutex removed, Filter hierarchy flattened |
| 04 | REST API / Axum Migration | POCO (500K vendored) to Axum — fine-grained locking, proper CORS, graceful shutdown |

### Infrastructure & Safety (Docs 05-09)

| Doc | Title | Focus |
|-----|-------|-------|
| 05 | Zinc I/O & Encoding | Dual C/C++ Zinc parsers (3,067 LOC) eliminated by `libhaystack` — JSON added free |
| 06 | Sedona VM FFI Strategy | 29 `extern "C"` functions, Cell union ABI, panic safety, buffer overflow fix |
| 07 | IPC Bridge | POSIX message queues to tokio channels — 3-phase roadmap, ARM ABI verification |
| 08 | Memory Safety Analysis | 45+ vulnerability sites across 13 bug classes — all structurally eliminated |
| 09 | Dependency Mapping | Complete C/C++ to Rust crate mapping, final Cargo.toml, binary size optimization |

### Build & VM Analysis (Docs 10-14)

| Doc | Title | Focus |
|-----|-------|-------|
| 10 | Build & Cross-Compilation | CMake+Docker to Cargo — 82% build script reduction, 20-60x incremental speedup |
| 11 | Migration Roadmap | 7-phase plan for 32,314 LOC, phase dependencies, testing, rollback |
| 12 | Sedona VM Architecture | VM internals — 1,281 LOC core, 240 opcodes, 78 native methods |
| 13 | Sedona VM Rust Porting | Actionable porting strategy — Cell type, opcode dispatch, 14-21 week estimate |
| 14 | Sedona VM Scalability Limits | 10 quantified bottlenecks — component ID overflow, 16KB stack, O(n^2) allocation |

### Future Protocols & Enhancements (Docs 15-19)

| Doc | Title | Focus |
|-----|-------|-------|
| 15 | SOX/WebSocket → ROX Protocol | SOX's 7 problems, Trio-over-WebSocket, SCRAM-SHA-256, mTLS clustering |
| 16 | roxWarp Protocol | Binary Trio (MessagePack), delta encoding, Scuttlebutt gossip, Fantom pod |
| 17 | Name Length Analysis | Sedona 7-char limit, 31-char patch, name interning (2 bytes/component) |
| 18 | Driver Framework v2 | Haxall-inspired `Driver` trait, `on_learn()`, PollScheduler, LocalIoDriver |
| 19 | Dynamic Slots | Side-car `DynSlotStore`, LoRaWAN/Modbus/BACnet use cases, SOX extension |

---

## 10. Completed Phases

### Phase Timeline

| Phase | Description | Date | Status |
|-------|-------------|------|--------|
| 0 | Workspace, HAL traits, MockHal | 2026-03-02 | Complete |
| 2 | Engine core (channels, tables, conversions, polls, watches) | 2026-03-02 | Complete |
| 2.5 | Engine<H> orchestration layer | 2026-03-02 | Complete |
| 3A-3H | Server binary, IPC, CLI, config loading, hardening, FD caching, Zinc wire | 2026-03-02 – 2026-03-04 | Complete |
| 1 | Haystack REST API (14 endpoints + Zinc, CORS, watches, filter) | 2026-03-04 | Complete |
| 4 | Sedona SVM FFI bridge, ChannelSnapshot, 22 native methods | 2026-03-04 | Complete |
| 5/5.5/5.6 | Deployment scripts, P0 hardening, performance | 2026-03-04 | Complete |
| 5.7 | Security (bind, bearer auth, filter depth, watch caps, rate limit, socket perms) | 2026-03-05 | Complete |
| 5.8 | Mock soak tests (6 integration tests, 1000+ poll cycles) | 2026-03-05 | Complete |
| 5.9 | Production cutover — C removed, Rust deployed | 2026-03-05 | Complete |
| 5.10 | Post-deployment fixes — I2C detection, ADC fault, backoff, health CLI | Post-deploy | Complete |
| 6.0 | Full SVM integration, all Kit 4 native methods | 2026-03-05 | Complete |
| 6.5 | TLS (rustls), SCRAM-SHA-256, CORS whitelist, path sanitization | 2026-03-05 | Complete |
| 7.0 | Engine polish — virtual write, granular reload, I2C coalescing (270ms→45ms) | 2026-03-05 | Complete |
| 8.0A | Haystack-over-WebSocket (ws.rs 892 lines, 31 tests) | 2026-03-04 | Complete |
| 8.0A-SOX | Full SOX/DASP protocol (pure Rust, 20/20 commands, 185 manifest types) | Post-8.0A | Complete |
| 8.0B | FileWrite + FileRename SOX commands | Post-SOX | Complete |
| 10.0A-E | Config-driven control (PID, sequencer, 35 components, .sax converter) | 2026-03-04 – 2026-03-05 | Complete |
| 11.0 (partial) | Pure Rust VM interpreter (150+ opcodes, 80 native methods, 752 tests) | Post-10.0 | **~90% Complete** |

### Production Deployment

- **Device:** BeagleBone Black (ARM Cortex-A8, 512MB RAM)
- **Target:** Todd Air Flow unit (30-113), 192.168.1.3:1919
- **Binary:** ~2.1 MB (server, stripped), ~580 KB (CLI, stripped)
- **Systemd:** Type=simple, MemoryLimit (Debian 9 / systemd 232 compatible)
- **Cross-compile:** Zig CC wrappers for ARM7, glibc 2.24 target, cargo-zigbuild in CI

---

## 11. Key Technical Decisions

### 11.1 Sedona VM: FFI → Pure Rust (Evolved)

Originally planned as a permanent C FFI bridge (Doc 06), the VM has been largely rewritten in pure Rust (20,947 lines, 90% complete). The `PURE_RUST_PLAN.md` documents the remaining 10%. Config-driven control (Phase 10.0) still makes the VM optional for EacIo.

### 11.2 libhaystack Over Custom Implementation

The `libhaystack` crate (J2 Innovations / Siemens) eliminated ~8,000 lines of C++ type infrastructure and ~3,067 lines of Zinc parsers. Provides Haystack 4 types, streaming Zinc/JSON encoding, and filter evaluation.

### 11.3 Config-Driven Control Over VM

~1,827 lines of Rust (`control.rs`) + `control.toml` provides PID, sequencer, and 35 executable component types with dataflow engine, cycle detection, and visual wiring — without requiring the Sedona VM at all.

### 11.4 Fine-Grained Async Locking

Separate `tokio::sync::RwLock` instances for records, watches, history, and channel index. Reads don't block watches; history doesn't block reads.

### 11.5 Command-Channel Architecture

```
REST Handler → mpsc::send(EngineCmd) → Main event loop
                                        ↓
                                   Engine::read/write
                                        ↓
Main event loop → oneshot::send(result) → REST handler
```
All operations are async-safe; engine runs on blocking thread.

### 11.6 I2C Coalescing + Exponential Backoff

Coalescing cache key = (device, address, label) reduces 6 redundant SDP810 reads to 1. Per-sensor exponential backoff (30s → 300s) for failed sensors prevents bus contention.

### 11.7 SOX/DASP: Null-Terminated Strings

Critical wire format discovery: Sedona Str encoding is `u2(size_including_null) + chars + 0x00`. Missing null terminator was root cause of all COV failures.

---

## 12. Quantitative Results

### 12.1 Binary Size

| Component | C/C++ (with POCO) | Rust |
|-----------|-------------------|------|
| Server binary | ~8-12 MB | ~2.1 MB (stripped) |
| CLI binary | — | ~580 KB (stripped) |
| POCO shared libs | ~8 MB (.so) | 0 |
| Total install | ~25 MB | ~3-5 MB |

### 12.2 Performance

| Metric | C/C++ | Rust | Improvement |
|--------|-------|------|-------------|
| I2C poll cycle | 270 ms | 45 ms | 6x faster |
| Startup time | ~2-3 seconds | <1 second | 2-3x faster |
| Cold build | ~15 minutes | ~3-5 minutes | 3-5x faster |
| Incremental build | ~3 minutes | ~10-30 seconds | 20-60x faster |
| Static analysis | ~2-5 minutes | ~5-10 seconds | 20-30x faster |

### 12.3 Codebase Metrics

| Metric | Value |
|--------|-------|
| Rust source files | 123 |
| Total lines of Rust | 85,346 |
| Workspace crates | 7 |
| Test annotations | 2,319+ |
| Test failures | 0 |
| Feature flags | 5 (mock-hal, linux-hal, simulator-hal, svm, tls) |
| Cargo dependencies (direct) | 28 |

### 12.4 Test Coverage by Crate

| Crate | Tests | Notable Test Files |
|-------|-------|--------------------|
| sandstar-server | ~1,209 | rest_api.rs (1,687 LOC), stress_tests.rs (1,253), soak_test.rs (646), ws_api.rs (594) |
| sandstar-svm | ~752 | interpreter_tests.rs (2,020 LOC), test_utils.rs (324) |
| sandstar-engine | ~284 | Unit tests in all modules |
| sandstar-hal-linux | ~114 | Integration tests in HAL modules |
| sandstar-cli | ~28 | CLI unit tests |
| sandstar-hal | ~19 | Mock HAL tests |
| sandstar-ipc | ~17 | IPC protocol tests |

### 12.5 API Surface

| Interface | Count |
|-----------|-------|
| REST endpoints | 24+ |
| SOX commands | 20/20 (complete DASP transport) |
| WebSocket message types | 16 (8 client→server, 8 server→client) |
| CLI commands | 10 |
| CLI arguments | 25 |
| Control components | 35 executable types (21 in engine + 14 via control.toml) |
| VM opcodes | 150+ (Rust interpreter) |
| Native methods | 80 (across 7 kits) |
| Manifest types | 185 (from 15 kits) |

---

## 13. Safety and Security

### 13.1 Memory Safety: 45+ Vulnerabilities Eliminated

Doc 08 cataloged 45+ vulnerability sites across 13 bug classes. **All structurally prevented by Rust's type system:**

| Bug Class | C/C++ Sites | Rust Prevention |
|-----------|-------------|-----------------|
| Buffer overflow | 15+ | Bounds-checked indexing, `String` (heap, growable) |
| Data races | 5+ | `Send`/`Sync` traits (compile-time) |
| Use-after-free | 3+ | Borrow checker (lifetime tracking) |
| File descriptor leaks | 5+ | `File` implements `Drop` |
| Unchecked return values | 5+ | `Result<T, E>` is `#[must_use]` |
| Integer overflow | 3+ | `usize` + overflow checks in debug |
| Null pointer dereference | 3+ | `Option<T>` at compile time |
| Double free | 2+ | Ownership (Drop runs exactly once) |
| Format string vulnerabilities | 2+ | `format!` macro (compile-time validation) |
| Uninitialized variables | 2+ | Definite initialization analysis |
| Signal handler unsafety | 1 | `tokio::signal` (async-safe) |
| Missing return | 1+ | Compiler error |
| Raw pointer IPC casting | 1 | Typed enum variants with serde |

### 13.2 Security Audit (All Resolved)

| Severity | Found | Issues | Resolution |
|----------|-------|--------|-----------|
| Critical | 3 | No auth, no TLS, no rate limiting | Bearer + SCRAM, rustls, atomic limiter |
| High | 3 | Filter depth, socket perms, watch cap | depth=32, mode=0660, cap=64 |
| Medium | 4 | PID perms, path traversal, log perms, CORS | 0600, canonicalize, umask, whitelist |
| **Total** | **10** | **All resolved** | **0 open** |

### 13.3 Authentication Details

**SCRAM-SHA-256 (RFC 5802):**
- SHA-256 hash, PBKDF2 with 10,000 iterations (configurable)
- Cryptographically random 16-byte salt per user
- Session tokens: 24-hour lifetime, max 256 simultaneous
- 30-second handshake timeout (anti-slowloris)

**Bearer Token (Legacy):**
- Single global token via `--auth-token`
- Coexists with SCRAM users
- Header: `Authorization: Bearer <token>`

**Rate Limiting:**
- Lock-free sliding 1-second window (AtomicU64 + AtomicI64)
- Default: 100 req/sec, configurable, 0 = unlimited
- Returns 429 Too Many Requests

---

## 14. Current Work in Progress

### 14.1 Phase 14.0A — Web-Based Visual DDC Editor

**Status: IN PROGRESS**

REST API endpoints for the web editor are being built. The full Phase 14.0 scope:

| Sub-Phase | Description | Status |
|-----------|-------------|--------|
| 14.0A | REST API endpoints for editor | In Progress |
| 14.0B | Web editor HTML | Not Started |
| 14.0C | Canvas rendering | Not Started |
| 14.0D | User interactions | Not Started |
| 14.0E | Component palette | Not Started |
| 14.0F | Live data + WebSocket | Not Started |

### 14.2 Phase 11.0 — Pure Rust VM (Remaining ~10%)

Per `PURE_RUST_PLAN.md`, the remaining work includes final native method coverage and scode edge cases. The interpreter core (150+ opcodes) and 80 native methods across 7 kits are implemented.

---

## 15. Future Roadmap

### 15.1 Phase 9.0 — ROX Protocol & roxWarp Clustering (Docs 15-16)

**Goal:** Replace SOX/UDP with modern WebSocket-based protocols.
**Infrastructure Status:** roxWarp modules built (~5K lines), not yet production-activated.
**Estimated additional:** ~4,600 LOC for ROX dual-stack + gossip activation.

### 15.2 Phase 12.0 — Driver Framework v2 (Doc 18)

**Goal:** Haxall-inspired driver architecture.
**Infrastructure Status:** Driver modules exist (Modbus, BACnet, MQTT, LocalIO, PollScheduler).
**Priority:** Very Low — current engine architecture serves production needs.

### 15.3 Phase 13.0 — Dynamic Slots (Doc 19)

**Goal:** Runtime metadata for components.
**Infrastructure Status:** `dyn_slots.rs` and `name_intern.rs` exist in sox/ module.
**Priority:** Very Low — needed primarily for multi-protocol discovery scenarios.

---

## 16. Risk Assessment

### 16.1 Resolved Risks

| Risk | Resolution |
|------|-----------|
| POCO dependency (500K LOC) | Eliminated — replaced by 28 Cargo crates |
| Memory safety (45+ bug sites) | Structurally eliminated by Rust type system |
| No authentication | Bearer token + SCRAM-SHA-256 |
| No encryption | TLS via rustls (optional feature flag) |
| I2C performance on ARM | Coalescing + backoff: 270ms → 45ms |
| SOX protocol complexity | Pure Rust, 20/20 commands, all tests passing |
| ARM cross-compilation | Zig CC wrappers + cargo-zigbuild in GitHub Actions CI |
| Build speed | 20-60x incremental improvement |

### 16.2 Current / Ongoing Risks

| Risk | Severity | Mitigation |
|------|----------|-----------|
| SDP810 I2C sensor failure | Low | Exponential backoff + garbage detection in software |
| libhaystack breaking change | Low | Pinned to exact version |
| Single production device | Medium | Baha (211-135) unreachable from Windows dev host |
| README version outdated (says 1.0.0) | Low | Update README to match Cargo.toml v1.6.0 |

### 16.3 Future Enhancement Risks

| Risk | Phase | Probability | Impact | Mitigation |
|------|-------|-------------|--------|-----------|
| VM remaining 10% edge cases | 11.0 | 40% | Medium | Byte-for-byte A/B testing against C VM |
| roxWarp mTLS on ARM | 9.0 | Low | Medium | rustls verified for ARM |
| Web editor security (XSS) | 14.0 | Medium | Medium | Sanitize all user input, CSP headers |
| Driver `block_on` deadlock | 12.0 | Medium | High | Careful Tokio runtime handle management |

---

## Appendix A: Cross-Compilation Setup

```bash
# Environment
export PATH="$HOME/.cargo/bin:$PATH"
export CC_armv7_unknown_linux_gnueabihf="C:\\czb\\zigcc-arm-cc.bat"
export AR_armv7_unknown_linux_gnueabihf="C:\\czb\\zigar-arm.bat"

# Build
cargo build --target armv7-unknown-linux-gnueabihf --release

# Or via alias
cargo arm-build

# Package (.deb)
cargo deb --target armv7-unknown-linux-gnueabihf --no-strip

# Deploy
tools/installSandstarRust.sh 30-113

# Validate
tools/validate-engines.sh <device-ip>
```

## Appendix B: Key File Paths

| Path | Content |
|------|---------|
| `sandstar_rust/` | Workspace root |
| `sandstar_rust/crates/` | 7 crate sources (123 .rs files) |
| `sandstar_rust/docs/research/` | 20 research documents + this report |
| `sandstar_rust/docs/ROADMAP_v2.md` | Current roadmap |
| `sandstar_rust/docs/DEPLOYMENT_CHECKLIST.md` | 4-phase deployment guide |
| `sandstar_rust/docs/HARDCODED_LIMITS.md` | Constants audit |
| `sandstar_rust/docs/PURE_RUST_PLAN.md` | VM completion plan |
| `sandstar_rust/HowToUses/` | 14 feature guides |
| `sandstar_rust/tools/` | 16 deployment/validation scripts |
| `sandstar_rust/tools/validation-runbook.md` | Validation procedures |
| `sandstar_rust/examples/` | 7 example configurations |
| `sandstar_rust/etc/` | Systemd service files, logrotate |
| `sandstar_rust/.github/workflows/ci.yml` | CI pipeline |
| `sandstar_rust/sox_components.json` | Persisted component tree |
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

*Generated from analysis of 20 research documents and full codebase audit (123 source files)*
*Report date: 2026-04-10 (updated) | Sandstar Rust v1.6.0*
