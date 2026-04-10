# Sandstar Rust Migration -- Roadmap v2

**Date:** 2026-03-31 (updated)
**Status:** PRODUCTION DEPLOYED — Rust v2.0.0 on BeagleBone. **Pure Rust VM complete** — Sedona VM rewritten in pure Rust (240 opcodes, 131 native methods, no C code, no FFI, no cc crate). **Visual DDC programming working** — 35 executable component types with dataflow engine, components wired in Sedona Application Editor with wire lines on canvas, values propagate through links in real-time. **20/20 SOX commands complete** — all SOX ops implemented including fileWrite/fileRename. **Component persistence** via sox_components.json with auto-save every 5s. **Channel-to-logic bridge** using "chXXXX" naming convention for sensor proxy. **Full DDC loop verified with real hardware** — real sensor (121F) through Tstat to heating output. **Cycle detection** in link graph (DFS). **Faster COV** — 200 burst on subscribe, 50/tick normal. **185 component types** loaded from manifest XML parser across all 15 kits. **Phase 14.0A IN PROGRESS** — Web-based Visual DDC Editor (REST API endpoints for component tree CRUD).
**Version:** 2.0.0 (7 crates, 1,637 tests, ~40,000+ lines of pure Rust)
**Feature Parity:** ~99% (80/80 features vs C system + extras)
**Research Documents:** 20 analysis docs (00-19) — deep gap analysis completed 2026-03-20
**Research Coverage:** ~82% weighted (100% on core docs 00-11, 90-95% on docs 12-14, deferred on future docs 16-19)
**GitHub:** https://github.com/TurkerMertkan/Sandstar_Rust (private)
**Codebase Audit:** 2026-03-20 — 3-agent deep research gap analysis (20 docs vs codebase)

---

## 1. Phase Completion Status

### Fully Complete

| Phase | Description | Tests | Date |
|-------|-------------|-------|------|
| Phase 0 | Workspace setup, HAL traits, MockHal | 9 | 2026-03-02 |
| Phase 2 | Engine core: channels, tables, conversions, filters, polls, watches | 129 | 2026-03-02 |
| Phase 2.5 | Orchestration: Engine<H> with channel_read/write/convert/poll_update | -- | 2026-03-02 |
| Phase 3A | Server binary + IPC + CLI tools | 4 | 2026-03-02 |
| Phase 3B | Real config loading (database.zinc, points.csv, tables.csv) | 18 | 2026-03-03 |
| Phase 3C | Operational hardening (PID, systemd, config reload, logging, tracing-appender) | -- | 2026-03-03 |
| Phase 3D | LinuxHal integration, feature flags (mock-hal/linux-hal), HAL validation | 82 | 2026-03-03 |
| Phase 3E | ARM cross-compilation (cargo-zigbuild), .cargo/config.toml, cargo-deb | -- | 2026-03-03 |
| Phase 3F | Non-blocking poll, HAL SubsystemProbe validation, best-effort init, cargo-deb assets | -- | 2026-03-04 |
| Phase 3G | --json CLI, dual watchdog (dev/watchdog + GPIO60), SIGHUP hardening, shutdown timeout | -- | 2026-03-04 |
| Phase 3H | Sysfs FD caching, Zinc wire format (content negotiation), history/trending ring buffer | -- | 2026-03-04 |
| Phase 1 | Haystack REST API: 14 endpoints + Zinc, CORS, watch subscriptions, filter parser | 21 | 2026-03-04 |
| Phase 4 (partial) | SVM bridge crate: FFI bindings, ChannelSnapshot, SvmRunner, native method stubs | -- | 2026-03-04 |
| Phase 5 (partial) | Side-by-side validation: read-only mode, validate-engines.sh, soak-monitor.sh, cutover/rollback scripts | -- | 2026-03-04 |
| Phase 5.5 | P0 hardening: panic hook, /health, sd_notify, 1MB body limit, watch lease expiry, metrics, logrotate, build.rs version embedding | -- | 2026-03-04 |
| Phase 5.6 | Performance: conv.clone() elimination, format!("{:?}") to as_str(), HistoryPoint String to enum, atomic metrics counters | -- | 2026-03-04 |
| Phase 5.7 | Security hardening: --http-bind default 127.0.0.1, bearer auth middleware, filter depth limit (32), watch caps (64), socket perms 0660 | 3 | 2026-03-04 |

| Phase 8.0A-SOX | SOX/DASP protocol: Sedona Application Editor connectivity | -- | 2026-03-22 |

**Phase 8.0A-SOX Details (2026-03-22):**
- Full DASP transport (UDP reliable messaging with ACK piggybacking, session management)
- SOX readSchema ('v'), readVersion ('y'), readComp ('c') with tree/config/runtime/links
- SOX file transfer (fileOpen/fileRead/fileClose) with `m:` manifest URI + `/kits/` binary URI
- Null-terminated string wire format (Sedona spec compliance)
- 15 kit definitions with correct checksums + versions from production manifests
- Component tree: App, Folder, SoxService, UserService, PlatformService, 150 AnalogInput channels
- Kit/type IDs verified against actual kit manifest XML (sys, sox, EacIo)
- Path traversal protection (canonicalize + bounds check) on file transfer
- Manifest extraction from .kit ZIP files deployed to `/home/eacio/sandstar/etc/manifests/`
- Cross-compile fix: `--sysroot` with 8.3 short path for zig linker on Windows with spaces in username
- Live sensor data visible in editor: COV event format (lowercase 'e', runtime slot filtering), EacIo::AnalogInput slot schema aligned with kit manifest, SOX 1.1 batch subscribe, readComp config/runtime filtering
- SOX Write/Invoke complete: ConstFloat, ConstInt, ConstBool, WriteFloat, WriteInt, WriteBool, Add2/Sub2/Mul2/Div2 all working
- Manifest XML parser: 185 component types loaded from all 15 kit manifests at startup
- ReadProp ('r') handler implemented
- Command byte fixes: Invoke='i' (CC editor), ReadProp='r', Rename='n'
- Sedona Str encoding: u2(size_including_null) + chars + 0x00
- DASP rate-limited COV queue (10/tick) prevents window overflow
- CONFIG COV events pushed after invoke/write for added components
- Delete tree events use saved parent_id (before component removal)
- **Visual wiring (2026-03-27):** Link add/delete ('l' command), bidirectional link storage, wire lines render on editor canvas
- **Dataflow engine (2026-03-27):** execute_links() propagates values through wires, execute_components() computes Add2/Sub2/Mul2/Div2 math
- **Runtime COV events (2026-03-27):** Both config AND runtime COV pushed for non-channel components after dataflow execution
- **Reorder command (2026-03-27):** handle_reorder('o') reorders parent's children list
- **readComp what='l' (2026-03-27):** Returns component links with 0xFFFF terminator
- **Link COV on subscribe (2026-03-27):** Push link events for all components with links after batchSubscribe
- **20/20 SOX commands (2026-03-30):** All SOX commands complete including fileWrite('h') and fileRename('x')
- **35 executable component types (2026-03-30):** Full dataflow engine with Tstat, Add2, Sub2, Mul2, Div2, ConstFloat, and more
- **Component persistence (2026-03-30):** sox_components.json auto-saved every 5s, restored on startup
- **Cycle detection (2026-03-30):** DFS-based cycle detection in link graph prevents infinite loops
- **Channel-to-logic bridge (2026-03-30):** "chXXXX" naming convention — rename ConstFloat to "ch1713" to proxy live sensor data into logic components
- **Full DDC loop verified (2026-03-30):** Real sensor (121F) flows through Tstat component to heating output on actual hardware
- **Faster COV (2026-03-30):** 200 burst events on subscribe, 50/tick normal operation
- **FileWrite + FileRename (2026-03-30):** Both implemented, completing Phase 8.0B

**Summary:** All core phases (0 through 10.0E + SOX + Dataflow + DDC loop) are complete. 1,637 tests passing, 0 failures.

### Partially Complete

| Phase | Description | Done | Remaining |
|-------|-------------|------|-----------|
| Phase 4 (SVM/Sedona) | Sedona FFI bridge | Bridge crate exists with ChannelSnapshot, SvmRunner, native method stubs (kit 4 + kit 100). **SOX protocol fully implemented in pure Rust** — all 20/20 SOX commands complete including fileWrite/fileRename. No SVM or C FFI needed. | VM binary compilation optional (config-driven control replaces VM for EacIo). |
| Phase 5 (Integration) | Production deployment | **v1.4.0 DEPLOYED**. C sandstar removed, Rust live on BeagleBone (192.168.1.3:1919). 1,637 tests, CI/CD, diagnostics, health monitoring. | No longer blocked. Production validated. |

### Not Started

| Phase | Description | Complexity | Research Doc | Priority |
|-------|-------------|-----------|--------------|----------|
| ~~Phase 8.0B~~ | ~~Full ROX Protocol~~ — **COMPLETE.** All 20/20 SOX commands implemented: readSchema(v), readVersion(y), readComp(c), subscribe(s), unsubscribe(u), write(w), fileOpen(f), fileRead(g), fileClose(q), event(e), readSchemaDetail(n), readProp(r), invoke(i), add(a), delete(d), rename(n), link(l), reorder(o), fileWrite(h), fileRename(x). | S | 15 | **COMPLETE** |
| **Phase 14.0A** | **Web-Based Visual DDC Editor: REST API Endpoints** | **M** | -- | **IN PROGRESS** |
| Phase 14.0B | Web-Based Visual DDC Editor: HTML Scaffold | M | -- | PLANNED |
| Phase 14.0C | Web-Based Visual DDC Editor: Canvas Rendering Engine | L | -- | PLANNED |
| Phase 14.0D | Web-Based Visual DDC Editor: Interactions | L | -- | PLANNED |
| Phase 14.0E | Web-Based Visual DDC Editor: Component Palette & CRUD | M | -- | PLANNED |
| Phase 14.0F | Web-Based Visual DDC Editor: Live Data & WebSocket | M | -- | PLANNED |
| Phase 9.0 | Northbound clustering (roxWarp) | XL | 16 | Low |
| Phase 11.0 | Sedona VM Rust port (bytecode interpreter, name interning) | XL | 12, 13, 14, 17 | **COMPLETE** (2026-04-10) |
| Phase 12.0 | Driver Framework v2 (Haxall-inspired, pure Rust) | XL | 18 | Very Low |
| Phase 13.0 | Dynamic Slots (hybrid static+dynamic slot model) | L | 19 | Very Low |

---

## SOX Protocol Implementation Status

**100% pure Rust implementation -- NO Sedona VM or C FFI required.**

### Wire Format
- Null-terminated strings (Sedona spec compliance)
- Big-endian integers for all multi-byte fields

### Transport
- DASP (Datagram Authenticated Session Protocol) over UDP port 1876
- ACK piggybacking for efficient reliable delivery
- Session management with hello/challenge/authenticate handshake

### Commands Implemented

| Command | Code | Description |
|---------|------|-------------|
| readSchema | `v` | Read kit schema (kit IDs, checksums, versions) |
| readVersion | `y` | Read platform version string |
| readSchemaDetail | `n` | Read detailed schema for specific kit |
| readComp | `c` | Read component tree (config, runtime, links, children) |
| subscribe | `s` | Subscribe to component change-of-value events (SOX 1.1 batch format implemented) |
| unsubscribe | `u` | Unsubscribe from COV events |
| write | `w` | Write slot values to components |
| fileOpen | `f` | Open file for reading (`m:` manifest URI, `/kits/` binary URI) |
| fileRead | `g` | Read file chunk (with path traversal protection) |
| fileClose | `q` | Close file handle |
| event | `e` | Server-push COV event to subscribed clients |
| readProp | `r` | Read single property value |
| invoke | `i` | Invoke component action (add/delete/rename via CC editor) |
| add | `a` | Add component to tree at runtime |
| delete | `d` | Delete component (with tree event using saved parent_id) |
| rename | `n` | Rename component |

| link | `l` | Add/delete component links (wiring), readComp what='l' for link data |
| reorder | `o` | Reorder component children |

### Commands Completed (formerly "Not Yet Implemented")

| Command | Code | Description | Phase | Status |
|---------|------|-------------|-------|--------|
| fileWrite | `h` | Write file to device | 8.0B | **DONE** |
| fileRename | `x` | Rename file on device | 8.0B | **DONE** |

**All 20/20 SOX commands are now implemented.**

### Dataflow Engine (2026-03-27)

Pure Rust link execution engine — values propagate through wired connections:
- `execute_links()`: Every tick, copies values from source slots to destination slots through links
- `execute_components()`: Computes math operations (Add2, Sub2, Mul2, Div2) after link propagation
- Bidirectional link storage: Links stored on both source and target components for editor wire rendering
- Runtime + Config COV events pushed for non-channel components that change
- Verified working: ConstFloat(5.0) → Add2.In1 = 5.0 → Add2.Out = 5.0

### Key Implementation Details
- 15 kit definitions with correct checksums + versions from production manifests
- Manifest XML parser: 185 component types loaded from all 15 kit manifests at startup
- Component tree: App, Folder, SoxService, UserService, PlatformService, 150 AnalogInput channels
- Kit/type IDs verified against actual kit manifest XML (sys, sox, EacIo)
- Manifest extraction from .kit ZIP files deployed to `/home/eacio/sandstar/etc/manifests/`
- Path traversal protection (canonicalize + bounds check) on file transfer
- Sedona Application Editor (Workbench) connects and operates against pure Rust engine
- SOX write/invoke: ConstFloat, ConstInt, ConstBool, WriteFloat, WriteInt, WriteBool, Add2/Sub2/Mul2/Div2
- Sedona Str encoding: u2(size_including_null) + chars + 0x00 (null-terminated)
- DASP rate-limited COV queue (10/tick) prevents send window overflow
- CONFIG COV events pushed after invoke/write for added components
- Delete tree events use saved parent_id (before component removal)

---

## 2. Security Audit Findings

The security audit identified issues across four severity levels. These MUST be addressed relative to deployment context (embedded device on private industrial network vs internet-exposed).

### Critical (blocks production on any network)

| # | Issue | Current State | Risk | Status |
|---|-------|---------------|------|--------|
| S1 | No authentication/authorization | Bearer token auth on POST endpoints; bind to 127.0.0.1 by default | MITIGATED | **RESOLVED (5.7a+5.7b)** |
| S2 | No TLS/HTTPS | Optional rustls TLS behind `tls` feature flag, --tls-cert/--tls-key | MITIGATED | **RESOLVED (6.5a)** |
| S3 | No REST rate limiting | Custom atomic rate limiter, --rate-limit (default 100/s) | MITIGATED | **RESOLVED (5.7e)** |

### High

| # | Issue | Current State | Risk | Status |
|---|-------|---------------|------|--------|
| S4 | Filter parser depth limit | MAX_PARSE_DEPTH=32, rejects deeper nesting | MITIGATED | **RESOLVED (5.7c)** |
| S5 | Unix socket file permissions | Mode 0660 after bind | MITIGATED | **RESOLVED (5.7f)** |
| S6 | Watch subscription cap | MAX_WATCHES=64, 60s active expiry timer | MITIGATED | **RESOLVED (5.7d)** |

### Medium

| # | Issue | Current State | Risk | Status |
|---|-------|---------------|------|--------|
| S7 | PID file permissions world-readable | PID file created with mode 0600 | MITIGATED | **RESOLVED (6.5d)** |
| S8 | Config path traversal | `std::fs::canonicalize()` validates config dir | MITIGATED | **RESOLVED (6.5c)** |
| S9 | Log file permissions | `UMask=0077` in systemd service files | MITIGATED | **RESOLVED** |
| S10 | No CSRF protection | Explicit CORS policy (methods, headers, max_age) replacing permissive() | MITIGATED | **RESOLVED (6.5e)** |

---

## 3. Feature Gap Analysis (~2% remaining)

### Engine Core Gaps (0% -- CLOSED)

| Feature | C System | Rust | Status |
|---------|----------|------|--------|
| Virtual channel write propagation | ~~Writes flow through virtual chain~~ **Actually disabled in C (commented out)** | Matches C behavior | **RESOLVED** -- documented with tests |
| Granular config update | Reload individual channel config | Diff-based reload with rollback | **RESOLVED** -- Phase 7.0b |

### HAL Driver Gaps (18% missing)

| Feature | C System | Rust | Impact | Effort |
|---------|----------|------|--------|--------|
| I2C worker thread pool | Dedicated I2C threads (3) | Single spawn_blocking | Low -- only matters with many I2C devices | [M] |
| UART async callback mode | Interrupt-driven UART reads | Polling-based | Low -- adequate for current sensors | [M] |
| PWM pinmux recovery | Auto-retry pinmux on failure | Best-effort, log warning | Low -- handled by initialize.sh | [S] |

### SVM/Sedona Gaps (20% missing -- SOX CLOSED)

| Feature | C System | Rust | Impact | Status |
|---------|----------|------|--------|--------|
| VM binary execution | Compiles and runs vm.c | Config-driven control replaces VM for EacIo; SvmRunner exists for other apps | Low -- EacIo runs without VM | Optional |
| Full native method table | All 3 kits + EacIo + shaystack | Kit 4 (22 Rust natives) + kit 100 (28 stubs) | Low -- only needed if VM used | Optional |
| ~~SOX protocol~~ | ~~SVM talks to engine via SOX~~ | ~~Pure Rust SOX: DASP + readSchema/readComp/subscribe/write/file transfer~~ | ~~High~~ | **RESOLVED (8.0A-SOX)** |
| ~~SOX remaining ops~~ | ~~reorder, readLink, fileWrite, fileRename~~ | All 20/20 SOX commands implemented | ~~Very Low~~ | **RESOLVED (8.0B)** |

### Operations Gaps (5% missing)

| Feature | C System | Rust | Impact | Effort |
|---------|----------|------|--------|--------|
| ~~Automatic core dump collection~~ | ~~Core dump + syslog on crash~~ | ~~RLIMIT_CORE + LimitCORE=infinity~~ | ~~Low~~ | **RESOLVED (7.0d)** |
| Remote firmware update | OTA via SSH scripts | Manual SCP + systemctl | Low -- scripts exist | [S] |

### Simulation Gaps (0% -- CLOSED)

| Feature | C System | Rust | Status |
|---------|----------|------|--------|
| Hardware-free testing | Not available | SimulatorHal + BASemulator bridge + data logging | **RESOLVED (5.8g)** -- Rust surpasses C |

---

## 4. Prioritized Roadmap (Phase 5.7+)

### Priority Definitions

- **Critical** -- Blocks production deployment; must be done before soak test
- **High** -- Should be done before production soak test for safety
- **Medium** -- Can be done post-production when stable
- **Low** -- Future enhancement, no production impact

---

### Phase 5.7: Security Hardening (Critical) [M] -- COMPLETE

**Goal:** Close the three Critical security gaps (S1-S3) and the two High gaps (S4, S6) that could cause denial of service or unauthorized control.

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 5.7a | **Bind address flag**: `--http-bind` CLI arg (default `127.0.0.1`). Env: `SANDSTAR_HTTP_BIND`. | [S] | DONE |
| 5.7b | **API token auth**: `--auth-token` / `SANDSTAR_AUTH_TOKEN`. Middleware checks `Authorization: Bearer <token>` on all POST endpoints. GET endpoints stay open. | [S] | DONE |
| 5.7c | **Filter parser depth limit**: `MAX_PARSE_DEPTH=32` in filter.rs, depth counter threaded through recursive descent parser. | [S] | DONE |
| 5.7d | **Watch subscription cap**: `MAX_WATCHES=64`. Reject new `WatchSub` when limit reached. 60s periodic expiry timer in main select! loop. | [S] | DONE |
| 5.7e | **Rate limiting middleware**: Custom atomic sliding-window rate limiter. `--rate-limit` flag (default 100 req/s, 0=unlimited). Returns 429. | [S] | DONE |
| 5.7f | **Unix socket permissions**: Mode 0660 after bind in ipc.rs. | [S] | DONE |

**Completed:** 2026-03-05 (6 of 6 tasks)
**Tests added:** 6+ (bind address, auth token, filter depth, rate limiting)

**Note on TLS:** The C system also runs without TLS on a private industrial network. TLS is not needed for Phase 5.7 parity. If internet exposure is planned, add `axum-server` with `rustls` in Phase 6.5 (see below).

---

### Phase 5.8: Production Soak Test (Critical) [M] -- MOSTLY COMPLETE

**Goal:** Validate identical behavior on BeagleBone hardware. Originally planned as 48h soak, reduced to 4-8h side-by-side validation based on comprehensive software verification (800 tests, 0 clippy, SimulatorHal E2E, soak+stress tests).

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 5.8a | **Deploy from Linux VM**: SSH to BeagleBone via jump host (172.28.109.221), install .deb package. | [S] | Awaiting hardware access |
| 5.8b | **Side-by-side validation**: Run validation service on port 8086 (--read-only), C on 8085. Use `soak-monitor.sh` for 4-8h continuous comparison. | [M] | Awaiting hardware access |
| 5.8c | **Metric collection**: Log poll durations, memory RSS, HAL errors every 60s to file. Compare with C system. | [S] | Awaiting hardware access |
| 5.8d | **Edge case validation**: Verify I2C sensor reconnect, ADC accuracy across temperature range, GPIO debounce behavior, UART framing. | [M] | Awaiting hardware access |
| 5.8e | **Decision gate**: Pass/fail criteria documented in `validation-runbook.md`. All 140 channels within 0.5% tolerance for 4-8 hours. 800 tests + virtual soak provide the confidence that the original 48h target was designed to build; 4-8h validates only hardware-specific behavior (I2C timing, ADC accuracy, ARM constraints). | [S] | Awaiting hardware access |
| 5.8f | **Mock soak tests**: 6 integration tests simulating long-running operation (1000+ polls, watch lifecycle, concurrent REST, HAL error recovery, rapid reload, WS storm). | [M] | DONE |
| 5.8g | **SimulatorHal + BASemulator bridge**: Full simulation without hardware. `SimulatorHal` (Arc<RwLock<SimulatorState>>), REST inject/outputs/state/scenario endpoints, basemulator-bridge.py with DataLogger, sim.sh one-command launcher. Structured CSV data logging (inputs.csv, outputs.csv, channels.csv, bas_raw.csv, session.json per session). | [L] | DONE |
| 5.8h | **Code quality cleanup**: 70 clippy warnings fixed, dead code removed, 55 hardcoded limits documented. | [S] | DONE |
| 5.8i | **Test coverage expansion**: 65 new tests (cmd_handler, CLI, IPC, filter, handlers, integration). 800 total. | [M] | DONE |
| 5.8j | **Hardware readiness assessment**: 800 tests passing, 0 clippy warnings, SimulatorHal E2E validation, soak+stress integration tests. Software layer fully proven. Hardware soak reduced from 48h to 4-8h side-by-side. | [S] | DONE (2026-03-10) |

**Completed:** 5.8f, 5.8g, 5.8h, 5.8i, 5.8j done (5/10 tasks). Only 5.8a-e remain (hardware access).
**Tests added:** 6 soak integration tests + 8 SimulatorHal unit tests + 65 coverage tests
**New files:** `crates/sandstar-hal/src/simulator.rs`, `crates/sandstar-server/src/rest/sim.rs`, `tools/sim.sh`, `tools/basemulator-bridge.py`, `tools/basemulator-mapping.json`
**Note:** Hardware tasks (5.8a-e) were completed during Phase 5.9 deployment (2026-03-18).
**Blocks:** Nothing (Phase 5.9 cutover complete)

---

### Phase 5.8h: Code Quality Cleanup (High) [S] -- COMPLETE

**Goal:** Address 70 clippy warnings and dead code identified by codebase audit (2026-03-10).

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 5.8h-1 | **Clippy style fixes**: 70 warnings fixed (needless borrows, redundant clones, unused imports, manual range contains, `map_or`→`is_some_and`, missing Default impls). Zero correctness issues. | [S] | DONE |
| 5.8h-2 | **Dead code removal**: Removed `SaxComponent.name` field + redundant `in_links` variable in sax_converter.rs; fixed unused `control_runner` assignment in main.rs shutdown. | [S] | DONE |
| 5.8h-3 | **Hardcoded limits audit**: 55 constants documented across 10 categories in `docs/HARDCODED_LIMITS.md`. 4 candidates identified for future CLI flags (MAX_WATCHES, MAX_WS_CONNECTIONS, MAX_SESSIONS, SESSION_LIFETIME_SECS). | [S] | DONE |

**Completed:** 2026-03-10 (3 of 3 tasks)
**Results:** 0 clippy warnings, 735 tests passing, `docs/HARDCODED_LIMITS.md` created

---

### Phase 5.8i: Test Coverage Expansion (High) [M] -- COMPLETE

**Goal:** Raise test coverage from ~60% to ~80% of public APIs. Audit identified critical untested modules.

| Task | Description | Tests | Effort | Status |
|------|-------------|-------|--------|--------|
| 5.8i-1 | **cmd_handler.rs**: 10 new tests — ListChannels, ListPolls, ListTables, WriteChannel (success + nonexistent), GetWriteLevels, PollNow, ReloadConfig, AboutInfo, GetHistory. | 10 | [M] | DONE |
| 5.8i-2 | **CLI crate**: 19 new tests — argument parsing for all subcommands (status, channels, polls, tables, read, write, shutdown, reload, history, convert-sax), --json flag, default/custom socket, level defaults. | 19 | [S] | DONE |
| 5.8i-3 | **main.rs orchestration**: Covered indirectly via integration tests (server startup, config loading, rate limiting, watch lifecycle). Direct unit tests deferred — main.rs is mostly orchestration glue. | -- | [M] | DEFERRED |
| 5.8i-4 | **ipc.rs**: 5 new tests — create_listener, accept+read_timeout, cleanup idempotent, round-trip frame exchange, concurrent connections. | 5 | [S] | DONE |
| 5.8i-5 | **REST edge cases**: 31 new tests — filter parser (8: Unicode, escapes, numeric edge cases, malformed, depth limit), handlers (11: channel parsing, history range, date boundaries, duration), integration (12: rate limiting 429, concurrent watch+REST, write-then-read, filter AND/OR/range, about/health/nav). | 31 | [S] | DONE |

**Completed:** 2026-03-10 (4 of 5 tasks; main.rs deferred)
**New tests:** 65 added (735 → 800 total)
**Results:** 800 tests passing, 0 failures, 0 clippy warnings

---

### Phase 5.9: Production Cutover (Critical) [S]

**Goal:** Swap C system for Rust system on primary port 8085.

| Task | Description | Effort |
|------|-------------|--------|
| 5.9a | **Cutover execution**: Run `cutover-to-rust.sh` -- stops C, moves Rust to port 8085, enables systemd. | [S] |
| 5.9b | **Post-cutover monitoring**: 24h watchdog -- check /health every 60s, auto-rollback if 3 consecutive failures. | [S] |
| 5.9c | **Rollback verification**: Dry-run `rollback-to-c.sh` before cutover to confirm it works. | [S] |

**Total effort:** 1-2 hours
**Blocked by:** Phase 5.8 pass
**Blocks:** Nothing (system is live)

---

### Phase 6.0: Sedona VM Integration (High) [L] -- MOSTLY COMPLETE

**Goal:** Bring Sedona VM from 50% to functional FFI bridge so DDC logic runs.

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 6.0a | **Include VM C sources**: `build.rs` compiles vm.c + 25 native C files. Kit 0/2/9 on Unix, stubs on Windows. | [M] | DONE |
| 6.0b | **Complete native method table**: Kit 0/2/9 have C impls (Unix), Kit 4 (EacIo) has 22 Rust native methods, Kit 100 (shaystack) has 28 stubs (remote ops only). | [M] | DONE (local DDC) |
| 6.0c | **SOX HTTP client**: NOT NEEDED — bridge.rs ChannelSnapshot + SvmWrite queues replace SOX entirely. | [M] | N/A |
| 6.0d | **Integration tests**: 12 bridge tests (snapshot CRUD, write queues, tag writes, runner lifecycle, channel resolution). Needs hardware for VM execution test. | [M] | DONE (bridge layer) |
| 6.0e | **Tag write bridge**: SvmTagWrite queue implemented in bridge.rs, drained in main.rs poll loop. | [S] | DONE |

**Completed:** 2026-03-05 (4/5 tasks; VM execution test needs hardware + real scode)
**Note:** With Phase 10.0A-D (config-driven control), the SVM is no longer needed for the EacIo application. The FFI bridge exists for backward compatibility with other Sedona applications.

---

### Phase 6.5: TLS + Advanced Security (Medium) [M] -- COMPLETE

**Goal:** Optional HTTPS support and hardened auth for deployments on less-trusted networks.

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 6.5a | **TLS via axum-server + rustls**: Optional `tls` feature flag, `--tls-cert`/`--tls-key` CLI flags, PEM cert/key loading. 7 unit tests. | [M] | DONE |
| 6.5b | **SCRAM-SHA-256 auth**: Full RFC 5802 implementation. `--auth-user`/`--auth-pass` CLI flags. HTTP `/api/auth` endpoint (hello→challenge→authenticate). WS SCRAM flow. Session tokens (24h expiry). Backward compat with bearer tokens. ~640 lines auth.rs + 14 unit tests + 5 integration tests. | [M] | DONE |
| 6.5c | **Config path sanitization**: `std::fs::canonicalize()` validates config dir, prevents path traversal. | [S] | DONE |
| 6.5d | **Restrictive file permissions**: PID file 0600, socket 0660. | [S] | DONE |
| 6.5e | **CORS restriction**: Replaced `CorsLayer::permissive()` with explicit method/header whitelist (GET/POST/OPTIONS, content-type/authorization/accept, max_age 3600s). | [S] | DONE |

**Completed:** 2026-03-05 (5 of 5 tasks)
**Blocks:** Nothing (full security suite ready)

---

### Phase 7.0: Engine Core Polish (Medium) [M] -- MOSTLY COMPLETE

**Goal:** Close the remaining engine feature gap.

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 7.0a | **Virtual channel write propagation**: Investigated — C system also doesn't propagate (ENGINE_MESSAGE_WRITE_VIRTUAL commented out). Documented with tests confirming current behavior matches C. | [S] | DONE (matches C) |
| 7.0b | **Granular config reload**: Diff-based SIGHUP reload. `ReloadSummary` tracks added/removed/modified/unchanged. `PollStore::snapshot()/restore()` rollback on failure. 6 tests. | [M] | DONE |
| 7.0c | **I2C read coalescing + thread-safety**: I2C pre-read cache in poll_update() eliminates redundant reads for channels sharing same sensor. LinuxI2c/LinuxUart raw-pointer UB replaced with Mutex. 6x improvement for SDP810 (270ms→45ms). 4 tests. | [M] | DONE |
| 7.0d | **Core dump collection**: `RLIMIT_CORE=unlimited` at startup (unix), logs `/proc/sys/kernel/core_pattern`. `LimitCORE=infinity` in systemd services. | [S] | DONE |

**Completed:** 2026-03-05 (4 of 4 tasks)
**Blocked by:** Nothing
**Blocks:** Nothing

---

### Phase 8.0A: Haystack-over-WebSocket (Medium) [M] -- COMPLETE

**Goal:** Real-time push for watch subscriptions via WebSocket. Eliminates polling latency for Haystack clients. This is the high-value, ready-now subset of ROX.

| Task | Description | Effort |
|------|-------------|--------|
| 8.0Aa | **WebSocket upgrade endpoint**: `GET /api/ws` using `axum::extract::ws::WebSocket`. Bearer token auth on upgrade. | [S] |
| 8.0Ab | **Watch-over-WS protocol**: Client sends JSON `{"op":"watchSub","channels":[1113,2001]}`, server pushes `{"op":"watchPoll","rows":[...]}` on change. Reuses existing `EngineCmd::WatchSub/WatchPoll`. | [M] |
| 8.0Ac | **Heartbeat + reconnect**: Server sends ping every 30s, client pong. Auto-unsub on disconnect. | [S] |
| 8.0Ad | **Zinc/JSON content negotiation**: Support both `text/zinc` and `application/json` over WS frames. | [S] |
| 8.0Ae | **Integration tests**: WS connect, subscribe, receive push, unsubscribe, disconnect. | [S] |

**Completed:** 2026-03-04 (661 lines ws.rs + 15 integration tests + 16 unit tests)
**Decision:** Split from full ROX because it requires NO SVM/component-tree access, uses existing watch infrastructure, and provides immediate value.
**Note:** First feature where Rust surpasses the C system (C has no WebSocket support).

---

### Phase 8.0B: Full ROX Protocol -- Remaining SOX Operations (Low) [M] -- COMPLETE

**Goal:** Complete all SOX commands for full component lifecycle management via Sedona Application Editor.

**All 20/20 SOX commands implemented.** Pure Rust, no SVM needed.

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 8.0Ba | **invoke(i)**: Invoke component actions (add/delete/rename via CC editor). CONFIG COV events pushed after invoke/write. | [M] | DONE |
| 8.0Bb | **add(a) / delete(d)**: Add/remove components from tree at runtime. Delete uses saved parent_id before removal for tree events. | [M] | DONE |
| 8.0Bc | **rename(n)**: Rename components. Command byte='n' (CC editor convention). | [S] | DONE |
| 8.0Bd-1 | **readProp(r)**: Single-property reads. | [S] | DONE |
| 8.0Bd-2 | **link(l)**: Add/delete links, readComp what='l' for link data. | [S] | DONE |
| 8.0Be | **fileWrite(h) / fileRename(x)**: Write files to device, rename files. | [S] | DONE |
| 8.0Bf | **Trio encoder/decoder**: Binary Haystack encoding per Project Haystack spec (more efficient than Zinc). | [M] | Not started (optional) |
| 8.0Bg | **reorder(o)**: Reorder component children. | [S] | DONE |

**Completed:** 2026-03-30 (all SOX commands done, only Trio encoding remains as optional enhancement)
**Blocks:** Phase 9.0 (clustering)

---

### Phase 14.0: Web-Based Visual DDC Editor (Medium) [L]

**Goal:** Browser-based visual programming editor served directly from the Rust server, enabling drag-and-drop DDC component wiring without requiring the Sedona Application Editor (Java desktop app). Provides a modern, accessible alternative for creating and editing control logic.

---

#### Phase 14.0A: REST API Endpoints (Rust Backend) -- IN PROGRESS

**Goal:** Expose the ComponentTree via `Arc<RwLock>` and add 11 new `/api/sox/*` REST endpoints for tree CRUD, palette listing, and position updates.

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 14.0Aa | **Share ComponentTree via Arc<RwLock>**: Make the SOX component tree accessible from REST handlers alongside the existing DASP/SOX path. | [M] | IN PROGRESS |
| 14.0Ab | **GET /api/sox/tree**: Return full component tree as JSON (id, name, type, children, slots, links, position). | [S] | |
| 14.0Ac | **POST /api/sox/add**: Add component to tree (parent_id, kit_id, type_id, name). | [S] | |
| 14.0Ad | **POST /api/sox/delete**: Delete component by id (with subtree). | [S] | |
| 14.0Ae | **POST /api/sox/rename**: Rename component (id, new_name). | [S] | |
| 14.0Af | **POST /api/sox/write**: Write slot value (comp_id, slot_name, value). | [S] | |
| 14.0Ag | **POST /api/sox/link**: Add/remove link (from_comp, from_slot, to_comp, to_slot). | [S] | |
| 14.0Ah | **GET /api/sox/palette**: Return available component types from manifest (kit/type/slots). | [S] | |
| 14.0Ai | **POST /api/sox/position**: Update component x,y position for editor layout persistence. | [S] | |
| 14.0Aj | **POST /api/sox/reorder**: Reorder children of a parent component. | [S] | |
| 14.0Ak | **Integration tests**: REST round-trip tests for all 11 endpoints. | [M] | |

**Total effort:** 3-5 days
**Blocks:** Phase 14.0B

---

#### Phase 14.0B: Editor HTML Scaffold -- PLANNED

**Goal:** Serve a single-file visual editor at `GET /editor` with a dark glassmorphism theme, toolbar, sidebar, canvas workspace, and CSS design system.

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 14.0Ba | **GET /editor route**: Serve single HTML file with embedded CSS/JS from Rust server (no build tooling). | [S] | PLANNED |
| 14.0Bb | **Dark glassmorphism theme**: CSS design system with blur/transparency effects, category colors, CSS custom properties. | [M] | PLANNED |
| 14.0Bc | **Toolbar**: Top bar with zoom controls, undo/redo buttons, save, grid toggle, editor title. | [S] | PLANNED |
| 14.0Bd | **Sidebar**: Collapsible left panel showing component tree hierarchy, properties panel for selected node. | [M] | PLANNED |
| 14.0Be | **Canvas workspace**: Central area for node placement and wire drawing. Responsive layout. | [S] | PLANNED |

**Total effort:** 2-3 days
**Blocks:** Phase 14.0C

---

#### Phase 14.0C: Canvas Rendering Engine -- PLANNED

**Goal:** Hybrid DOM nodes + Canvas wires rendering. Grid background, bezier curve wires, viewport pan/zoom, node DOM elements with category-colored headers.

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 14.0Ca | **Grid background**: Canvas-rendered dot/line grid with zoom-responsive density. | [S] | PLANNED |
| 14.0Cb | **Node DOM elements**: HTML div nodes with category-colored headers (Math=blue, Logic=green, HVAC=orange, etc.), input/output port circles, slot value displays. | [M] | PLANNED |
| 14.0Cc | **Bezier curve wires**: Canvas-drawn cubic bezier connections between ports, colored by data type. | [M] | PLANNED |
| 14.0Cd | **Viewport pan/zoom**: Mouse wheel zoom, middle-click pan, zoom-to-fit, minimap (optional). | [M] | PLANNED |
| 14.0Ce | **Coordinate system**: World-space to screen-space transform, consistent hit testing across zoom levels. | [S] | PLANNED |

**Total effort:** 3-5 days
**Blocks:** Phase 14.0D

---

#### Phase 14.0D: Interactions -- PLANNED

**Goal:** Drag nodes, create/delete wires by port dragging, select/multi-select, pan/zoom, context menus, undo/redo stack.

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 14.0Da | **Drag nodes**: Click-drag to move nodes, snap-to-grid option, update position via REST. | [S] | PLANNED |
| 14.0Db | **Wire creation**: Drag from output port to input port to create link, visual preview during drag. | [M] | PLANNED |
| 14.0Dc | **Wire deletion**: Click wire to select, Delete key or right-click to remove. | [S] | PLANNED |
| 14.0Dd | **Select/multi-select**: Click to select node, Shift+click or rubber-band for multi-select, selection highlight. | [M] | PLANNED |
| 14.0De | **Context menus**: Right-click on canvas (add component), node (rename/delete/properties), wire (delete), port (disconnect all). | [M] | PLANNED |
| 14.0Df | **Undo/redo stack**: Command pattern for add/delete/move/wire/rename operations, Ctrl+Z/Ctrl+Y. | [M] | PLANNED |

**Total effort:** 3-5 days
**Blocks:** Phase 14.0E

---

#### Phase 14.0E: Component Palette & CRUD -- PLANNED

**Goal:** Searchable command palette, add/delete/rename components, inline property editing, right-click context menu.

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 14.0Ea | **Command palette**: Press `/` to open searchable overlay listing all available component types from manifest. Filter by name/kit/category. | [M] | PLANNED |
| 14.0Eb | **Add component**: Select from palette, click canvas to place. Calls POST /api/sox/add. | [S] | PLANNED |
| 14.0Ec | **Delete component**: Select + Delete key or context menu. Removes links. Calls POST /api/sox/delete. | [S] | PLANNED |
| 14.0Ed | **Rename component**: Double-click node header for inline rename. Calls POST /api/sox/rename. | [S] | PLANNED |
| 14.0Ee | **Inline property editing**: Click slot value on node to edit in-place. Calls POST /api/sox/write. | [M] | PLANNED |

**Total effort:** 2-3 days
**Blocks:** Phase 14.0F

---

#### Phase 14.0F: Live Data & WebSocket -- PLANNED

**Goal:** Real-time value updates via WebSocket, flow animation on wires, cross-editor sync (browser and Sedona Editor), value flash animations.

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 14.0Fa | **WebSocket subscription**: Connect to existing `/api/ws` endpoint, subscribe to component COV events. | [S] | PLANNED |
| 14.0Fb | **Live value updates**: Update slot values on node DOM elements in real-time as COV events arrive. | [M] | PLANNED |
| 14.0Fc | **Flow animation**: Animated dashes/particles on wires to indicate data flow direction and activity. | [S] | PLANNED |
| 14.0Fd | **Value flash animations**: Brief highlight/pulse when a slot value changes (CSS transition). | [S] | PLANNED |
| 14.0Fe | **Cross-editor sync**: Changes made in Sedona Application Editor (via SOX/DASP) reflect in browser editor and vice versa. Tree change events trigger UI refresh. | [M] | PLANNED |
| 14.0Ff | **Connection status**: Visual indicator for WebSocket connection state (connected/reconnecting/disconnected). | [S] | PLANNED |

**Total effort:** 3-5 days
**Blocks:** Nothing

---

### Phase 9.0: Northbound Clustering -- roxWarp (Low) [XL]

**Goal:** Multi-device clustering with delta-encoded state replication.

| Task | Description | Effort |
|------|-------------|--------|
| 9.0a | **Binary Trio diff protocol**: Delta encoding of grid changes between cluster peers. | [L] |
| 9.0b | **Gossip discovery**: mDNS or UDP broadcast for peer discovery on same subnet. | [M] |
| 9.0c | **State reconciliation**: Conflict resolution for concurrent writes from different peers. | [L] |
| 9.0d | **Fantom pod for SkySpark**: Package as SkySpark extension for centralized monitoring. | [L] |

**Total effort:** 1-2 weeks
**Blocked by:** Phase 8.0
**Blocks:** Nothing

---

### Phase 10.0A-D: Config-Driven Control Engine (Medium) [M] -- COMPLETE

**Goal:** Replace Sedona VM with native Rust PID + sequencer for EacIo HVAC control.

| Task | Description | Status |
|------|-------------|--------|
| 10.0A | **PID Controller** (`pid.rs`): PI(D) with anti-windup, max_delta rate limiting, direct/reverse action, configurable interval. 15 unit tests. | DONE |
| 10.0B | **Lead Sequencer** (`sequencer.rs`): N-stage with hysteresis dead-band, configurable range. 11 unit tests. | DONE |
| 10.0C | **Control Runner** (`control.rs`): TOML config loader, wires PID+sequencer to engine channels, validates write levels. 12 unit tests. | DONE |
| 10.0D | **Server Integration**: Runs after `poll_update()` in `spawn_blocking`, SIGHUP reload, `--no-control` + `--control-config` CLI flags. | DONE |

**Completed:** 2026-03-04 (~400 lines of control logic replacing 100K-line Sedona VM)
**Decision:** Config-driven approach (Approach A) over full component framework — 100% of value at 25% of cost for this application.

### Phase 10.0E: Additional Components Library (Low) [M] -- COMPLETE

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 10.0Ea | **Arithmetic**: Add2, Sub2, Mul2, Div2, Neg, Round, FloatOffset (7 components) | [S] | DONE |
| 10.0Eb | **Logic**: And2, Or2, Not, SRLatch (4 components) | [S] | DONE |
| 10.0Ec | **Timing**: DelayOn, DelayOff, OneShot, Ramp (4 components, Instant-based) | [S] | DONE |
| 10.0Ed | **HVAC**: Thermostat (with deadband), Hysteresis (threshold-based) | [M] | DONE |
| 10.0Ee | **Scheduling**: DailyScheduleFloat, DailyScheduleBool | [M] | DONE |
| 10.0Ef | **Migration tooling**: .sax XML → control.toml converter (~700 lines). CLI `convert-sax` subcommand. Link-chain following, WriteFloat elimination, priority extraction. 8 tests. | [M] | DONE |

**Completed:** 2026-03-05 (all 6 tasks, 20 components + converter)
**Blocks:** Nothing

---

### Phase 11.0: Sedona VM Rust Port (Very Low) [XL] -- COMPLETE

**Goal:** Replace the 100K-line C VM with a safe Rust implementation.
**Completed:** 2026-04-10 -- All C code eliminated. Pure Rust VM, no FFI, no cc crate.

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 11.0a | **Bytecode interpreter**: Cell-based stack machine, **240 opcodes** (all 10 categories), bounds-checked VmStack (4096 cells), VmMemory with code+data segments, match-based dispatch with NaN equality special cases. | [XL] | **DONE** |
| 11.0b | **Native method framework**: `NativeTable` with `(kit_id, method_id)` dispatch, `NativeContext` struct replacing C `(SedonaVM*, Cell*)` pointers. Safe Rust function signatures. | [L] | **DONE** |
| 11.0c | **Native method kits**: Kit 0/sys (60 methods: malloc/free, copy, intStr/floatStr, ticks, Str ops, Component reflection, FileStore, Test). Kit 2/inet (17 methods: TCP/UDP sockets, SHA-1). Kit 9/datetimeStd (3 methods). Kit 4/EacIo (23 methods via bridge). Kit 100/shaystack (28 stubs). | [L] | **DONE** |
| 11.0d | **Image loader + VmConfig**: Scode header parsing/validation (magic, version, block size), configurable limits (stack 64KB, components 4096, scode 4MB, data 1MB, call depth 64), Block16/Byte32 address widths. | [M] | **DONE** |
| 11.0e | **ComponentStore**: Free-list allocation (O(1) alloc/free), u32 IDs (4B max), iterative tree walk (no recursion), SmallVec<[u32; 8]> children. | [M] | **DONE** |
| 11.0f | **RustSvmRunner**: Lifecycle management (start/resume/stop), loads scode from file, runs main+resume methods, Drop impl for cleanup. | [M] | **DONE** |
| 11.0g | **Bridge**: ChannelSnapshot (Arc<RwLock>), SvmWrite/SvmTagWrite queues (Arc<Mutex<Vec>>), FFI catch_unwind safety wrappers. | [M] | **DONE** |
| 11.0h | **Test utilities**: ScodeBuilder for in-memory scode assembly, op/op_u8/op_u16/op_u32 emitters, build_memory convenience. | [S] | **DONE** |
| 11.0i | **Name interning**: Replace 7-char name limit with interned strings (doc 17). 31-char name validation enforced at all entry points (REST, RoWS, SOX, editor). | [M] | **DONE** |
| 11.0j | **Compatibility testing**: Verified via C code elimination and full test suite. | [L] | **DONE** |

**Completion:** 100% (2026-04-10). All C code removed, pure Rust VM with 650+ tests.
**Test count:** 650+ tests in sandstar-svm crate (509 unit + 141 integration)
**Total effort:** 2-4 weeks (completed incrementally alongside other phases)
**Blocked by:** Nothing
**Blocks:** Phase 12.0, 13.0
**Research docs:** 12 (VM Architecture), 13 (Porting Strategy), 14 (Scalability Limits), 17 (Name Interning)

---

### Phase 12.0: Driver Framework v2 (Very Low) [XL]

**Goal:** Pure Rust driver architecture inspired by Haxall's connector framework. Replaces C/C++ driver layer with structured lifecycle, auto-discovery, and polling buckets.

**Research doc:** [18_SEDONA_DRIVER_FRAMEWORK_V2.md](research/18_SEDONA_DRIVER_FRAMEWORK_V2.md)

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 12.0a | **Driver trait**: `open()`, `close()`, `ping()`, `learn()`, `sync_cur()`, `write()` lifecycle callbacks. Status cascading (driver→point). `DriverManager` with register/remove/open_all/close_all/sync_all. REST endpoints (`/api/drivers`, status, learn). | [L] | **COMPLETE** |
| 12.0b | **PollScheduler**: Polling buckets with automatic staggering. Replaces engine auto-poll-all pattern. | [M] | Not started |
| 12.0c | **WatchManager**: Change-of-value subscriptions at the driver level. COV deadband support. | [M] | Not started |
| 12.0d | **LocalIoDriver**: Channel-aware driver with direction/type/enable metadata, HAL bridge for GPIO/ADC/I2C/PWM. Learn returns all channels with tags. Write validates output direction. | [L] | **COMPLETE** |
| 12.0e | **Protocol drivers**: ModbusDriver (TCP/RTU), BacnetDriver (IP/MSTP), MqttDriver (pub/sub). Stubs created with trait implementations. | [XL] | Stubs complete |
| 12.0f | **DriverManager**: Tokio actor orchestrating lifecycle, health monitoring, auto-reconnect. | [L] | Sync version complete; async/actor pending |

**Total effort:** 2-4 weeks
**Blocked by:** Phase 11.0 (COMPLETE)
**Blocks:** Phase 13.0

---

### Phase 13.0: Dynamic Slots (Very Low) [L]

**Goal:** Hybrid static+dynamic slot model for runtime point discovery. Overcome Sedona's compile-time-frozen slot limitation.

**Research doc:** [19_DYNAMIC_SLOTS.md](research/19_DYNAMIC_SLOTS.md)

| Task | Description | Effort | Status |
|------|-------------|--------|--------|
| 13.0a | **DynSlotStore**: Side-car `HashMap<u16, HashMap<String, DynValue>>` with typed values, memory limits (64/comp, 10K total). | [M] | COMPLETE |
| 13.0b | **REST API**: GET/PUT/DELETE `/api/tags/{comp_id}` with merge/replace modes, plain JSON auto-conversion. | [M] | COMPLETE |
| 13.0c | **Persistence**: Atomic JSON save (write .tmp then rename), load on startup, corrupt file recovery, version tag, auto-save every 5s. | [M] | COMPLETE |
| 13.0d | **Component cleanup**: Tags auto-deleted when SOX component is deleted. | [S] | COMPLETE |
| 13.0e | **Protocol metadata**: Store Modbus register address, BACnet object ID, MQTT topic, LoRaWAN devEUI as dynamic slots. | [M] | COMPLETE (via REST API) |
| 13.0f | **SOX/ROX protocol extension**: Serialize/deserialize dynamic slots over network (readTags, setTags, deleteTag). | [M] | Not started |
| 13.0g | **Haystack tag integration**: Dynamic slots participate in Haystack filter queries. | [M] | Not started |

**Total effort:** 1-2 weeks
**Blocked by:** Phase 12.0 (discovery creates the need for dynamic slots)
**Blocks:** Nothing

---

## 5. Critical Path to Production

```
PRODUCTION DEPLOYED — Rust v2.0.0 live on BeagleBone (192.168.1.3)
  1,637 tests, 0 warnings, 0 security issues, CI/CD active

COMPLETED production tracks:
  ✓ Phase 5.7:      Security hardening (6/6 tasks)
  ✓ Phase 5.8:      Hardware validation + mock soak + SimulatorHal + code quality + test coverage
  ✓ Phase 5.9:      Production cutover (C removed, Rust v1.0.0 → v1.1.0 deployed)
  ✓ Phase 5.10:     Post-deployment fixes (I2C protocol, ADC fault, backoff, health CLI)
  ✓ Phase 6.0:      SVM FFI Integration (4/5 tasks, VM exec optional)
  ✓ Phase 6.5:      TLS + Security (5/5 tasks incl. SCRAM-SHA-256)
  ✓ Phase 7.0:      Engine core polish (4/4 tasks)
  ✓ Phase 8.0A:     Haystack-over-WebSocket (all tasks)
  ✓ Phase 8.0A-SOX: SOX/DASP protocol (pure Rust, Sedona Editor connected)
  ✓ Phase 8.0B:     Full ROX Protocol (20/20 SOX commands, fileWrite+fileRename done)
  ✓ Phase 10.0:     Config-driven control (A-E complete + .sax converter)
  ✓ DDC Loop:       Visual DDC programming, 35 component types, persistence, channel bridge

Active development:
  ├── Phase 14.0A: Web DDC Editor REST API [M, IN PROGRESS]
  ├── Phase 14.0B: Editor HTML Scaffold    [M, PLANNED, needs 14.0A]
  ├── Phase 14.0C: Canvas Rendering Engine [L, PLANNED, needs 14.0B]
  ├── Phase 14.0D: Interactions            [L, PLANNED, needs 14.0C]
  ├── Phase 14.0E: Palette & CRUD          [M, PLANNED, needs 14.0D]
  └── Phase 14.0F: Live Data & WebSocket   [M, PLANNED, needs 14.0E]

Remaining future tracks (post-production):
  ├── Phase 9.0:  roxWarp clustering       [XL, no blockers]
  ├── Phase 11.0: Sedona VM Rust port      [XL, COMPLETE (2026-04-10), 650+ tests]
  ├── Phase 12.0: Driver Framework v2      [XL, needs 11.0]
  └── Phase 13.0: Dynamic Slots            [L, needs 12.0, 13.0a-e COMPLETE]
```

**Status:** PRODUCTION LIVE since 2026-03-18, v2.0.0 (pure Rust VM, 20/20 SOX, 35 component types, visual DDC, full DDC loop verified, 1,637 tests). **Phase 14.0A (Web DDC Editor REST API) IN PROGRESS.**
**BeagleBone:** 192.168.1.3 (Todd Air Flow), port 1919
**Deployment guide:** `docs/DEPLOYMENT_CHECKLIST.md` (copy-paste ready)

---

## 6. Summary Table

| Phase | Name | Priority | Effort | Status | Research Docs |
|-------|------|----------|--------|--------|---------------|
| 0-3H | Core migration | -- | -- | COMPLETE | 00-05, 07-10 |
| Phase 1 | REST API | -- | -- | COMPLETE | 04, 05 |
| Phase 5.5-5.6 | Hardening + Performance | -- | -- | COMPLETE | 08 |
| 5.7 | Security hardening | Critical | [M] | COMPLETE (6/6 tasks) | -- |
| 5.8 | Production validation | Critical | [M] | COMPLETE (hardware validated 2026-03-19) | -- |
| 5.8h | Code quality cleanup | High | [S] | COMPLETE (70 clippy fixed, 3 dead code removed, 55 limits documented) | -- |
| 5.8i | Test coverage expansion | High | [M] | COMPLETE (65 new tests: cmd_handler, CLI, IPC, filter, handlers, integration) | -- |
| 5.8j | Hardware readiness assessment | High | [S] | COMPLETE (project declared hardware-ready, soak reduced 48h→4-8h) | -- |
| **5.9** | **Production cutover** | **Critical** | **[S]** | **COMPLETE** (2026-03-18, C removed, Rust v1.0.0 live) | -- |
| **5.10** | **Post-deployment fixes** | **High** | **[S]** | **COMPLETE** (I2C protocol fix, ADC fault detection, backoff, health CLI) | 02 |
| 6.0 | Sedona VM integration | High | [L] | MOSTLY COMPLETE (4/5) | 06 |
| 6.5 | TLS + advanced security | Medium | [M] | COMPLETE (5/5 tasks) | -- |
| 7.0 | Engine core polish | Medium | [M] | COMPLETE (4/4 tasks) | 01, 02 |
| 8.0A | Haystack-over-WebSocket | Medium | [M] | COMPLETE (ws.rs + 31 tests) | 15 |
| 8.0A-SOX | SOX/DASP protocol (pure Rust) | Medium | [L] | COMPLETE (DASP + 20/20 commands, 185 manifest types, dataflow engine) | 15 |
| 8.0B | Full ROX Protocol (all SOX ops) | Low | [S] | **COMPLETE** (20/20 SOX commands, fileWrite+fileRename done) | 15 |
| **14.0A** | **Web DDC Editor: REST API** | **Medium** | **[M]** | **IN PROGRESS** | -- |
| 14.0B | Web DDC Editor: HTML Scaffold | Medium | [M] | PLANNED | -- |
| 14.0C | Web DDC Editor: Canvas Rendering | Medium | [L] | PLANNED | -- |
| 14.0D | Web DDC Editor: Interactions | Medium | [L] | PLANNED | -- |
| 14.0E | Web DDC Editor: Palette & CRUD | Medium | [M] | PLANNED | -- |
| 14.0F | Web DDC Editor: Live Data & WS | Medium | [M] | PLANNED | -- |
| 9.0 | roxWarp clustering | Low | [XL] | Not started | 16 |
| 10.0A-D | Config-driven control engine | Medium | [M] | COMPLETE | -- |
| 10.0E | Additional components library | Low | [M] | COMPLETE (20 + converter) | -- |
| 11.0 | Sedona VM Rust port | Very Low | [XL] | **COMPLETE** (2026-04-10, 650+ tests, pure Rust, no C/FFI) | 12, 13, 14, 17 |
| **12.0** | **Driver Framework v2** | **Very Low** | **[XL]** | **In progress (12.0a,d complete; stubs for e)** | **18** |
| **13.0** | **Dynamic Slots** | **Very Low** | **[L]** | **In progress (13.0a-e complete; SOX/ROX + Haystack pending)** | **19** |

---

## 7. Metrics Snapshot

| Metric | Value |
|--------|-------|
| Rust source lines | ~39,000 (~7,700 new VM + native methods) |
| C/C++ replaced | ~27,000 lines engine + 6,839 lines SVM (all eliminated, pure Rust) |
| POCO eliminated | ~500,000 lines |
| Test count | 1,637 passing, 0 failures |
| Pure Rust VM | 240 opcodes, 131 native methods (5 kits), 650+ tests, VmConfig, ComponentStore, RustSvmRunner |
| Crates | 7 (engine, hal, hal-linux, ipc, server, cli, svm) |
| REST endpoints | 25 (14 Haystack + health + metrics + zinc + WebSocket + rate-limit + auth + 4 simulator) |
| Control components | 35 executable types (PID, sequencer, Tstat, Add2/Sub2/Mul2/Div2, ConstFloat, and more) |
| IPC commands | 13 |
| CLI commands | 12 (status, channels, polls, tables, read, write, shutdown, reload, history, convert-sax, health, diagnostics) |
| Feature parity | ~99% (80/80 features + extras) |
| Production deployment | BeagleBone (Todd Air Flow, 192.168.1.3), 3.4MB RAM, 0.28% CPU |
| Live sensors | 1 — Solidyne 00-WTS-A (10K NTC, channel 1713, 78°F validated) |
| CI/CD | GitHub Actions: fmt + clippy + test on push/PR, ARM cross-compile on master |
| Monitoring | health-monitor.sh cron (5min), /api/diagnostics endpoint, CLI health + diagnostics |
| ARM binary size | Server ~2.1MB + CLI 580KB (stripped, with crypto deps) |
| Security issues open | 0 Critical, 0 High, 0 Medium — all resolved |
| Research documents | 20 (00-19) — deep gap analysis 2026-03-20 |
| Research coverage | ~72% weighted (100% core, deferred on future phases) |
| Simulation tools | SimulatorHal + BASemulator bridge + DataLogger + sim.sh |
| SOX commands | 20/20 complete (pure Rust, no SVM needed) |
| Component persistence | sox_components.json, auto-save every 5s |
| DDC loop | Verified: real sensor (121F) through Tstat to heating output |
| Features surpassing C | 12 (WebSocket, SimulatorHal, data logging, ADC fault detect, I2C backoff, CLI health/diagnostics, poll overrun detect, CI/CD, pure Rust SOX, visual DDC programming, component persistence, channel-to-logic bridge) |
| Clippy warnings | 0 |
| Test coverage (est.) | ~80% public API |
| Project health score | 9.5/10 (production validated, full DDC loop verified, 2026-03-30) |
| GitHub | https://github.com/TurkerMertkan/Sandstar_Rust (private) |

---

## 8. Research Document Coverage

Deep gap analysis completed 2026-03-20 (3-agent, 20 documents vs full codebase).

| Doc | Title | Phase | Coverage | Key Finding |
|-----|-------|-------|----------|-------------|
| 00 | Executive Summary | All | 98% | Complete; test count outdated (820 vs 627) |
| 01 | Engine Core Analysis | 0, 2, 7.0 | 100% | Surpassed: PID, sequencer, components, priority arrays |
| 02 | Hardware Drivers | 3D, 7.0c | 95% | GPIO uses sysfs (not chardev); async I2C/UART unnecessary |
| 03 | Haystack Type System | 2 | 90% | Custom impl instead of libhaystack (deliberate tradeoff) |
| 04 | REST API (Axum) | 1 | 92% | No Xeto/Commit/Root ops (low priority); surpassed with WS+auth |
| 05 | Zinc I/O Encoding | 3H | 80% | Custom Zinc parser works; not libhaystack |
| 06 | Sedona FFI Strategy | 4, 6.0 | 75% | Architecture changed: in-process bridge replaces 29 FFI functions |
| 07 | IPC Bridge | 3A | 85% | Skipped POSIX IPC (correct); typed bincode over TCP/Unix |
| 08 | Memory Safety | All | 100% | All 13 C/C++ bug classes eliminated structurally |
| 09 | Dependency Mapping | All | 92% | libhaystack not used; zig CC instead of Docker cross |
| 10 | Build & Cross-compile | 3E | 95% | No CI/CD; zig CC surpasses Docker approach |
| 11 | Migration Roadmap | 0-5 | 98% | All 7 phases complete + WebSocket, control engine, SCRAM |
| 12 | SVM Architecture | 11.0 | 100% | Pure Rust VM complete: 240 opcodes, VmStack, VmMemory, NativeTable, ImageLoader. 650+ tests. All C code eliminated (2026-04-10) |
| 13 | SVM Porting Strategy | 11.0 | 100% | All recommended phases implemented: Cell type (i32 stack), opcode dispatch (match), stack (bounds-checked Vec), memory segments, native method system, scode loader. Kit 0/2/4/9 natives ported. C code removed, pure Rust VM complete |
| 14 | Scalability Limits | 11.0, 5.6, 7.0c | 95% | ComponentStore (free-list, u32 IDs, iterative tree walk), VmConfig (configurable limits: 64KB stack, 4096 components, 4MB scode, Byte32 addressing). All scalability fixes from doc 14 implemented |
| 15 | SOX/WebSocket | 8.0A, 8.0A-SOX, 8.0B | 100% | WS + SCRAM + full SOX/DASP done (20/20 commands, pure Rust, 185 manifest types, dataflow engine, component persistence) |
| 16 | roxWarp Protocol | 9.0 | 0% | Entirely unimplemented (future Phase 9.0) |
| 17 | Name Length Analysis | 11.0c | 60% | Unlimited names via String; interning unnecessary at scale. **31-char Sedona-compat name validation enforced across all entry points** (REST, RoWS, SOX add/rename, editor JS) as of 2026-04-04. |
| 18 | Driver Framework v2 | 12.0 | 55% | Driver trait + DriverManager + LocalIoDriver + REST endpoints done. Modbus/BACnet/MQTT stubs created. PollScheduler/WatchManager/async actor pending |
| 19 | Dynamic Slots | 13.0 | 60% | DynSlotStore with REST API, persistence (atomic save/load on startup), auto-save 5s, component cleanup, memory limits. SOX/ROX protocol extension and Haystack filter integration pending |

**Key architectural divergence:** Custom Zinc/filter implementation instead of libhaystack dependency (docs 03/05); in-process bridge instead of 29-function C FFI (doc 06); channel-centric model instead of component-centric (docs 18/19). All are defensible engineering decisions that reduced complexity and external dependencies.

---

## 9. Decision Log

| Decision | Rationale | Date |
|----------|-----------|------|
| Default bind to 127.0.0.1 (planned) | Matches defense-in-depth; reverse proxy or --bind=0.0.0.0 for explicit exposure | 2026-03-04 |
| Bearer token before SCRAM | Simple, sufficient for private network; SCRAM deferred to Phase 6.5 with TLS | 2026-03-04 |
| TLS deferred to 6.5 | C system has no TLS either; private industrial network; parity first | 2026-03-04 |
| ~~Sedona VM stays C for now~~ | ~~100K lines, stable, FFI bridge works~~ -- **SUPERSEDED:** Phase 11.0 complete (2026-04-10), VM is now pure Rust | 2026-03-04 |
| No internet exposure planned | BeagleBone sits on private 172.28.x.x subnet; security measures are defense-in-depth | 2026-03-04 |
| ROX split into 8.0A + 8.0A-SOX + 8.0B | Haystack-over-WS (8.0A) ready; SOX/DASP (8.0A-SOX) implemented in pure Rust with 11 commands; remaining ops (8.0B) no longer blocked by SVM FFI | 2026-03-04 |
| Custom rate limiter over tower::limit | Atomic sliding-window implementation — zero additional deps, simpler than enabling tower features | 2026-03-05 |
| Virtual write non-propagation matches C | C system has ENGINE_MESSAGE_WRITE_VIRTUAL commented out; documented with tests rather than diverging | 2026-03-05 |
| TLS implemented as optional feature flag | `cargo build --features tls` to include rustls; zero overhead when not compiled in | 2026-03-05 |
| CORS restriction over permissive | Explicit method/header whitelist prevents CSRF; only GET/POST/OPTIONS allowed | 2026-03-05 |
| SCRAM-SHA-256 with bearer backward compat | Both auth modes coexist — SCRAM for Haystack/Haxall clients, bearer for simple scripts | 2026-03-05 |
| Mock soak tests for 5.8 | Can't access BeagleBone; built 6 integration tests simulating 1000+ poll cycles with stress patterns | 2026-03-05 |
| Core dump via RLIMIT_CORE | Unix-only, `LimitCORE=infinity` in systemd; logs core_pattern path at startup | 2026-03-05 |
| I2C thread pool deferred | Requires RefCell→Arc refactor in LinuxHal; low priority with current sensor count | 2026-03-05 |
| 20 component library over full framework | Concrete structs, no trait abstraction; TOML `[[component]]` config; matches existing PID/sequencer pattern | 2026-03-05 |
| I2C coalescing over thread pool | All 6 I2C channels share same sensor; pre-read cache eliminates 5 of 6 reads (270ms→45ms). Also fixed raw-pointer UB in LinuxI2c/LinuxUart with Mutex | 2026-03-05 |
| SVM Phase 6.0c (SOX client) not needed | bridge.rs ChannelSnapshot + SvmWrite queues replace SOX entirely; no HTTP client needed | 2026-03-05 |
| .sax converter as CLI subcommand | `sandstar-cli convert-sax` for one-time migration; link-chain following eliminates WriteFloat passthroughs | 2026-03-05 |
| SimulatorHal as feature flag | `simulator-hal` feature creates SharedSimState (Arc<RwLock>) with REST inject/outputs/state/scenario endpoints. Enables full-stack testing without BeagleBone | 2026-03-10 |
| BASemulator bridge in Python | Python script bridges BASemulator XML-RPC to Sandstar REST. DataLogger writes structured CSV per session. Simpler than Rust for integration glue | 2026-03-10 |
| sim.sh one-command launcher | Single script starts server + bridge with pre-flight checks, health wait, graceful cleanup. Modes: --test, --config, --scenario, --once | 2026-03-10 |
| Driver Framework v2 added as Phase 12.0 | From research doc 18 — Haxall-inspired pure Rust driver architecture. Deferred: current HAL + engine pattern sufficient for EacIo | 2026-03-10 |
| Dynamic Slots added as Phase 13.0 | From research doc 19 — hybrid static+dynamic slot model for runtime discovery. Only needed if Driver Framework v2 adds protocol drivers (Modbus, BACnet) | 2026-03-10 |
| 3-agent codebase audit | Full project analysis: code quality (67 clippy, 0 bugs), research docs (99% coverage), test coverage (~60%, 4 untested modules). Health score: 9/10 | 2026-03-10 |
| Phase 5.8h added (code quality) | Clippy warnings are all style-level but should be cleaned before production. No correctness issues found | 2026-03-10 |
| Phase 5.8i added (test coverage) | dispatch.rs, CLI, main.rs, ipc.rs have 0 tests. Targeting ~55-75 new tests to reach ~80% coverage | 2026-03-10 |
| Hardware soak reduced 48h→4-8h | 800 tests + SimulatorHal E2E + soak/stress tests provide confidence that 48h was designed to build. 4-8h validates only hardware-specific behavior (I2C timing, ADC accuracy, ARM constraints) | 2026-03-10 |
| Web-based DDC editor (Phase 14.0) | Browser-based visual editor served from Rust server replaces need for Java-based Sedona Application Editor. Single HTML file with embedded CSS/JS, no build tooling. Hybrid DOM nodes + Canvas wires for rendering. 6 sub-phases (A-F) from REST API through live WebSocket sync | 2026-03-31 |
| Project declared hardware-ready | All software phases complete. 800 tests, 0 clippy, 0 security issues. Only blocker is BeagleBone network access. Deployment checklist created at docs/DEPLOYMENT_CHECKLIST.md | 2026-03-10 |
| Phase 5.9 production cutover | C sandstar v0.1.1 removed, Rust v1.0.0 deployed to BeagleBone (Todd Air Flow). 150 channels, 15 polls, 3.4MB memory, 0.28% CPU | 2026-03-18 |
| I2C protocol detection by address | SDP810 channels with generic labels ("CFM Flow") were using wrong protocol. Added address-based fallback (0x25 → SDP810) | 2026-03-19 |
| ADC out-of-range fault detection | Disconnected thermistors showed Ok/-40°F. Now raw at table boundaries → Fault status. 2% margin distinguishes near-limit from at-limit | 2026-03-19 |
| I2C exponential backoff | Reinit spam ~232 WARN/hr → ~12 DEBUG/hr. Backoff 30s→60s→120s→240s→300s cap. Recovery resets immediately | 2026-03-20 |
| CLI health command | At-a-glance device status: channel fault/down summary, per-type breakdown, uptime. Supports --json | 2026-03-20 |
| Custom Zinc/filter over libhaystack | Research docs 03/05 proposed libhaystack crate. Deliberate choice to use custom impl (~1500 lines) — no external dependency on third-party crate for critical data format | 2026-03-20 |
| Channel-centric over component-centric | Rust uses flat channel tables instead of Sedona component tree. Simpler, works for EacIo. Future Driver Framework (Phase 12.0) may need component model | 2026-03-20 |
| GitHub repo created | Private repo at TurkerMertkan/Sandstar_Rust. Milestone releases for significant phases | 2026-03-19 |
| Solidyne 00-WTS-A requires halved table | EacIo voltage divider produces ADC at half expected range for this sensor. Created thermistor10K2_wts.txt (values/2). Swapped on device | 2026-03-20 |
| auto_detect skip when pre-configured | auto_detect_sensor() was overwriting zinc-loaded config every poll. Now skips if table_index+low+high already set | 2026-03-20 |
| BeagleBone needs static IP | DHCP reassigns IP on power loss (was .104, then .105, now .102). Static IP or DHCP reservation needed for reliable access | 2026-03-20 |
| Haystack API opened to 0.0.0.0 | Changed --http-bind from 127.0.0.1 to 0.0.0.0 in systemd service for future SkySpark connectivity | 2026-03-20 |
