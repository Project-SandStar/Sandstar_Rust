# Sandstar Rust — Progress Overview

**Last updated:** 2026-11-18 (audited against `master` `b48484b`; earlier text last updated 2026-04-20)
**Current version:** workspace `2.9.0` (`Cargo.toml`); tags `v2.9.0` (2026-04-25) and `v2.9.1` (2026-04-29). The v2.9.1 tag has no matching `Cargo.toml` bump, and `CHANGELOG.md` stops at 2.8.10.
**Deployed:** one field BeagleBone runs `2.9.0+3b8e97c` (version + git hash, readable from `/api/about` `buildInfo`). Since that build, `master` changed only tooling and documentation.
**Workspace:** 7 crates. Tests: 2748 passed on Windows, 2764 on Linux CI, 0 failed (26 suites). `test_sys_ticks` is flaky.
**Quality gate:** `bash tools/gates.sh` (fmt, clippy `-D warnings`, `test --no-fail-fast`, `linux-hal,svm` check). CI is green on `master`.
**GitHub:** https://github.com/Project-SandStar/Sandstar_Rust
**Research docs:** each `../research/NN_*.md` now starts with a "Status (2026-11-18)" block; the doc-by-doc summary is [`../research/PROGRESS_REPORT_2026_11_18.md`](../research/PROGRESS_REPORT_2026_11_18.md).

This is the single-read status tracker for the Sandstar Rust project. It consolidates everything in `docs/Progress/` into one scan. For per-phase rationale, design decisions, or operator instructions, follow the links into the detailed documents — they remain the **source of truth**.

---

## Table of contents

- [§1 TL;DR status matrix](#1-tldr-status-matrix)
- [§2 Completed work](#2-completed-work)
  - [§2.1 Pure Rust VM migration](#21-pure-rust-vm-migration)
  - [§2.2 SOX/DASP protocol + DDC engine](#22-soxdasp-protocol--ddc-engine)
  - [§2.3 Haystack filter improvements (Project A)](#23-haystack-filter-improvements-project-a)
  - [§2.4 BACnet/IP driver (Project B)](#24-bacnetip-driver-project-b)
  - [§2.5 MQTT driver (Project C)](#25-mqtt-driver-project-c)
  - [§2.6 Poll integration + shared tick task](#26-poll-integration--shared-tick-task)
  - [§2.7 Driver Framework v2 — Phase 12.0A](#27-driver-framework-v2--phase-120a)
  - [§2.8 Native editor backend — Phase 14.0G](#28-native-editor-backend--phase-140g)
  - [§2.9 Maintenance steps S-1 to S-9 (2026-11)](#29-maintenance-steps-s-1-to-s-9-2026-11)
- [§3 REST API surface](#3-rest-api-surface)
- [§4 Deployment state](#4-deployment-state)
- [§5 Quality gates](#5-quality-gates)
- [§6 What's pending](#6-whats-pending)
- [§7 Version timeline](#7-version-timeline)
- [§8 Document map](#8-document-map)

---

## §1 TL;DR status matrix

| Area | Status | Primary doc |
|------|--------|-------------|
| Pure Rust VM migration | ✅ Port complete, no C left. ⚠ The VM cannot yet run real scode (natives get no VM memory) and the field service does not start it; control logic runs on the SOX dataflow | [PURE_RUST_PLAN.md](PURE_RUST_PLAN.md), [PURE_RUST_VM_COMPLETION_PLAN.md](PURE_RUST_VM_COMPLETION_PLAN.md), [research/13](../research/13_SEDONA_VM_RUST_PORTING_STRATEGY.md) |
| SOX/DASP protocol (20/20 commands) | ✅ Complete (off by default; the field service runs with `--sox`) | [ROADMAP_v2.md](ROADMAP_v2.md) §Phase 8.0A-SOX |
| Visual DDC programming + dataflow engine | ✅ Complete (full loop verified on hardware) | [ROADMAP_v2.md](ROADMAP_v2.md) |
| Haystack filter (`not` compound, `->` path) | ✅ Complete — Project A | [IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md](IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md) |
| BACnet/IP driver (read + write + COV + RPM + BBMD) | ✅ Complete — Project B, 11 phases, live-validated | [IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md](IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md), [BACNET_SETUP.md](BACNET_SETUP.md) |
| MQTT driver | ✅ Complete — Project C, 4 phases, live-validated | [IMPLEMENTATION_PLAN_MQTT.md](IMPLEMENTATION_PLAN_MQTT.md), [MQTT_SETUP.md](MQTT_SETUP.md) |
| Poll integration (driver → engine channels) | ✅ Complete — shared tick task in rest/mod.rs | [IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md](IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md) Stage 1/2 |
| REST API + WebSocket | ✅ 30+ routes (see §3), bearer + SCRAM auth, rate limit, WS push. ⚠ Several write routes sit outside the auth layer | [ROADMAP_v2.md](ROADMAP_v2.md) §Phase 1, [DRIVER_REST_API.md](DRIVER_REST_API.md) |
| Security hardening | 🟡 Partial. Open: the field service binds `0.0.0.0` with no auth configured; write routes outside `check_auth` (§3); SIGTERM not handled (no graceful shutdown) | [ROADMAP_v2.md](ROADMAP_v2.md) §Phase 5.7, §Phase 6.5 |
| Visual DDC editor | 🟡 14.0A REST complete. 14.0B–F were superseded by 14.0G (native backend for the external Vue editor): about 75%, G-6 open, G-7/G-8 partial | [IMPLEMENTATION_PLAN_NATIVE_EDITOR_BACKEND.md](IMPLEMENTATION_PLAN_NATIVE_EDITOR_BACKEND.md) |
| Driver Framework v2 (Phase 12) | ✅ All sub-phases complete (12.0A–G + 12.0D.WS). Caveats: MQTT never reconnects; BACnet COV subscriptions never start in production; REST-created drivers get no poll tick; Modbus is not loadable | [IMPLEMENTATION_PLAN_DRIVER_FRAMEWORK.md](IMPLEMENTATION_PLAN_DRIVER_FRAMEWORK.md) |
| Clustering — roxWarp (Phase 9) | 🟡 9.0a + 9.0b + 9.0c shipped; 9.0d SkySpark Fantom pod open. The typed binary-Trio format exists in code but is not what goes over the wire | [ROADMAP_v2.md](ROADMAP_v2.md) §Phase 9, [research/16](../research/16_ROXWARP_PROTOCOL.md) |
| Dynamic Slots (Phase 13) | ✅ All 7 sub-phases complete — `DynSlotStore`, REST `/api/tags/{id}`, ROX `readTags`/`setTags`/`deleteTag`, SOX `readComp` extension, filter engine integration, atomic persistence | [ROADMAP_v2.md](ROADMAP_v2.md) §Phase 13 |
| Field sensor validation | 🟡 The field device has live HVAC channels; one table-converted reading looks implausible and is under investigation | — |
| Real BACnet vendor device validation | 🟡 Pending — no hardware available | — |
| Maintenance steps S-1 to S-9 (Nov 2026) | ✅ CI gate green again, toolchain pin, log retention, cluster interval validation, watchdog visibility/retry, gate script, secrets hook (§2.9) | — |

Legend: ✅ complete · 🟡 partial / deferred / paused · ⬜ not started

---

## §2 Completed work

### §2.1 Pure Rust VM migration

**Source of truth:** [PURE_RUST_PLAN.md](PURE_RUST_PLAN.md), [PURE_RUST_VM_COMPLETION_PLAN.md](PURE_RUST_VM_COMPLETION_PLAN.md)

Shipped in v2.0.0 (2026-04-10). All 240 Sedona VM opcodes and 131 native methods rewritten in Rust; the C source tree (`csrc/`, `ffi.rs`, `runner.rs`) was deleted. `pure-rust-vm` is the default; the `cc` crate no longer appears in the build.

Phase transitions (all ✅):

1. Enable 37 previously-disabled native methods
2. Wire server to use `RustSvmRunner`
3. Integration test against real `.scode` files
4. Make pure-rust-vm the default
5. Remove the C FFI path

**Resulting footprint:**
- `sandstar-svm` crate: 577 tests, `build.rs` is a no-op
- ARM .deb: 1.8 MB
- Target binary: 5.4 MB (server), 779 KB (CLI), stripped

### §2.2 SOX/DASP protocol + DDC engine

**Source of truth:** [ROADMAP_v2.md](ROADMAP_v2.md) §Phase 8.0A-SOX and §SOX Protocol Implementation Status

All 20/20 SOX editor commands implemented. Full DASP transport with UDP reliable messaging and ACK piggybacking. 185 component types from 15 kits. Dataflow engine with cycle detection (DFS). 35 executable component types (Add2, Sub2, Mul2, Div2, Tstat, ConstFloat, PID, …). Full DDC loop verified on real hardware: sensor (121 °F) through Tstat to heating output.

Key implementation facts (from [ROADMAP_v2.md](ROADMAP_v2.md)):
- Null-terminated strings (Sedona wire format)
- Sedona Str encoding: `u2(size_including_null) + chars + 0x00`
- DASP ACK piggybacking on response datagrams
- `sox_components.json` auto-save every 5s
- Channel-to-logic bridge via "chXXXX" naming (e.g. rename a `ConstFloat` to `ch1713` to proxy live sensor data)
- 200-message COV burst on subscribe, 50/tick normal rate limit

### §2.3 Haystack filter improvements (Project A)

**Source of truth:** [IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md](IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md) Project A

Completed 2026-04-13.

| Phase | Scope | Status |
|---|---|---|
| A1 | Proper `not` operator — `Expr::Not(Box<Expr>)` AST variant; fixed parse_term bug where `Expr::Missing(format!("{:?}", other))` produced nonsense | ✅ |
| A2 | `->` path dereference — new `Path` struct, `Token::Arrow`, `Pather` callback, rewrote `parse_cmp`, unified `matches_full` evaluator | ✅ |
| A3 | Unified test suite expanded to cover all new paths | ✅ |
| A4 | Deployed to 1-11 with v2.0.0 | ✅ |

### §2.4 BACnet/IP driver (Project B)

**Source of truth:** [IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md](IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md) Project B
**Operator guide:** [BACNET_SETUP.md](BACNET_SETUP.md)

The plan originally said "read-only" but B7 (WriteProperty) was added mid-sprint. Final driver supports discovery, read (+ batching), write (priority array), COV subscribe/renew/notification cache, and BBMD foreign-device registration.

| Phase | Scope | Ver | Status |
|---|---|---|---|
| B1 | BVLL / NPDU / APDU frame codec, ObjectType, BacnetValue, TransactionTable | 2.1.0 | ✅ |
| B2 | Who-Is / I-Am discovery wired into `open()` | 2.1.0 | ✅ |
| B3 | `sync_cur()` + `read_present_value()` + retry/dispatch via TransactionTable | 2.1.0 | ✅ |
| B4 | `learn()` — reads Device.object-list + ObjectName per device; `BacnetValue::Array` | 2.1.0 | ✅ |
| B5 | REST wiring; `SANDSTAR_BACNET_CONFIGS` env; DriverHandle replaces SharedDriverManager | 2.1.0 | ✅ |
| B6 | Production deploy to 1-11 + live validation via `tools/bacnet_sim.py` | 2.1.2 | ✅ |
| B7 | WriteProperty — `Apdu::{WritePropertyRequest, SimpleAck}`, `encode_write_property`, dispatcher (Real for AI/AO/AV, Enumerated for BI/BO/BV, priority 16) | 2.2.0 | ✅ |
| B9 | ReadPropertyMultiple — `RpmRequestSpec` + `RpmResult`, sync_cur groups by device and batches; fallback to individual RP on Error PDU | 2.3.0 | ✅ |
| B8 | SubscribeCOV wire-level — `Apdu::{SubscribeCovRequest, UnconfirmedCovNotification, ConfirmedCovNotification}`, on_watch/on_unwatch tracking | 2.4.0 | ✅ |
| B8.1 | COV notification reception — `CovCache` with max_age expiry; 6 inline recv loops process notifications as side-effect | 2.5.0 | ✅ |
| B8.2 | COV subscription renewal — `CovSubscription.subscribed_at`, `renewal_interval` (default 240s = 80% of lifetime), piggyback on `sync_cur` | 2.5.1 | ✅ |
| B10 | BBMD / Router — Register-Foreign-Device, Distribute-Broadcast-To-Network, Forwarded-NPDU, BVLL-Result encoders/decoders; auto-registration in `open()` | 2.5.0 | ✅ |

**Two critical operational notes** (documented in [BACNET_SETUP.md](BACNET_SETUP.md)):
1. Linux hosts typically need an explicit firewalld rule: `firewall-cmd --permanent --zone=public --add-port=47808/udp && firewall-cmd --reload`. Without it, conntrack drops the I-Am reply because its source doesn't match the outbound broadcast tuple. This was how the v2.1.2 debug logging was used to diagnose the issue on 1-11.
2. `point_id` in the config MUST correspond to an existing VirtualAnalog channel in `database.zinc`. The driver does NOT auto-create channels. Writes to non-existent channels fail cleanly with `channel N not found` (logged as WARN).

### §2.5 MQTT driver (Project C)

**Source of truth:** [IMPLEMENTATION_PLAN_MQTT.md](IMPLEMENTATION_PLAN_MQTT.md)
**Operator guide:** [MQTT_SETUP.md](MQTT_SETUP.md)

rumqttc 0.24 (pure-Rust, cross-compiles cleanly under Zig CC). All 4 phases shipped 2026-04-17.

| Phase | Scope | Commit | Ver | Status |
|---|---|---|---|---|
| M1 | Client lifecycle — `MqttConfig` / `MqttObjectConfig`, `MqttDriver` with AsyncClient + event-loop task, open/close/ping/learn | `3abd14c` | 2.8.0-dev | ✅ |
| M2 | Value cache + `sync_cur` — shared `Arc<Mutex<MqttValueCache>>`, JSON pointer extraction, plain-f64 parse | `a6b7049` | 2.8.0-dev | ✅ |
| M3 | `AsyncDriver::write()` — publish to `publish_topic`, payload format (`"N"` or `{"value":N}` JSON) | `4191fc2` | 2.8.0-dev | ✅ |
| M4 | Server wiring (`load_mqtt_drivers` in `rest/mod.rs`), 5 E2E tests, `MQTT_SETUP.md` | `763bdea` | 2.8.0 | ✅ |

**Live validation:** mosquitto 2.1.2 on Windows dev box `<dev-host>:1884` → `mosquitto_pub -t sandstar/test/humidity -m "55.5"` → cache → `sync_cur` → `write_channel` at priority 16 → `/api/read?id=103` returned `cur=55.5`. (ISP blocks outbound TCP 1883 to public brokers; local mosquitto was the workaround.)

### §2.6 Poll integration + shared tick task

**Source of truth:** [IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md](IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md) (Stage 1 and Stage 2 sections)

The gap that no one had noticed until v2.6.0: the driver actor's loop was purely command-driven. Nothing invoked `handle.sync_all()` periodically, so every `sync_cur` implementation — BACnet + MQTT — was effectively dead code in production.

**Fix** (now shared by both drivers): `load_bacnet_drivers` and `load_mqtt_drivers` in `rest/mod.rs` each spawn a `tokio::spawn` tick task (5s interval, `MissedTickBehavior::Skip`) that:
1. Calls `handle.sync_all(tick_points.clone()).await`
2. For each `Ok(value)` result: calls `engine.write_channel(point_id, Some(value), level=16, who="bacnet:{id}" | "mqtt:{id}", duration=30s)`

Values now flow end-to-end: `peer → driver.sync_cur → engine channel → /api/read`.

Also in this work:
- Version bump choreography: v2.6.0 added `register_point` + `add_poll_bucket`; v2.6.1 added the tick task; v2.7.0 wired results into engine.write_channel. Deployed incrementally with verification at each step.
- Discovery for both drivers documented its value: the live broker validation for MQTT used this path end-to-end.

### §2.7 Driver Framework v2 — Phase 12.0A

**Source of truth:** [IMPLEMENTATION_PLAN_DRIVER_FRAMEWORK.md](IMPLEMENTATION_PLAN_DRIVER_FRAMEWORK.md)
**Research:** [research/18_*.md](../research/18_SEDONA_DRIVER_FRAMEWORK_V2.md)

Phase 12.0A completed 2026-04-17 (v2.8.1). Scoped refactor that collapses the ~200-line-each `load_bacnet_drivers` + `load_mqtt_drivers` functions in `rest/mod.rs` into a single generic `load_drivers<L: DriverLoader>(handle, engine_handle)` helper.

| Piece | Location | Description |
|---|---|---|
| `DriverLoader` trait | `crates/sandstar-server/src/drivers/loader.rs` | 5 associated items (env var, driver type, label, config type, factory) capture everything that differs between drivers |
| `load_drivers<L>` | same file | Parse env var → register drivers → register points → add poll bucket → `open_all` → spawn tick task. Single source of truth for the whole glue. |
| `BacnetLoader` | `drivers/bacnet/mod.rs` | Impl for BACnet: `ENV_VAR="SANDSTAR_BACNET_CONFIGS"`, factory `BacnetDriver::from_config`, who prefix `"bacnet"` |
| `MqttLoader` | `drivers/mqtt.rs` | Impl for MQTT: `ENV_VAR="SANDSTAR_MQTT_CONFIGS"`, factory `MqttDriver::from_config`, who prefix `"mqtt"` |

**Net LOC impact:** `rest/mod.rs` dropped from ~1,400 to ~998 lines (-417 lines of duplication removed); `loader.rs` added 289 lines (generic + reusable); two loader impls added ~30 lines each. Net: -~130 LOC, but more importantly adding a 4th driver is now ~30 lines of glue instead of ~200.

Behavior preserved identically — 2,643 tests still passed at the time, deployed v2.8.1 with the BACnet driver still running cleanly.

> **Update (2026-11-18):** 12.0B-G and 12.0D.WS were also completed the same week (v2.8.2-v2.8.8, see §7 and [IMPLEMENTATION_PLAN_DRIVER_FRAMEWORK.md](IMPLEMENTATION_PLAN_DRIVER_FRAMEWORK.md)). The "deferred" note that stood here is obsolete.

### §2.8 Native editor backend — Phase 14.0G

**Source of truth:** [IMPLEMENTATION_PLAN_NATIVE_EDITOR_BACKEND.md](IMPLEMENTATION_PLAN_NATIVE_EDITOR_BACKEND.md)

Serves the external Vue editor directly from the Rust binary, replacing the SkySpark/Fantom bridge: shared SOX op dispatcher (`sox/sox_ops.rs`), the `/live` WebSocket (Fantom dialect), `/live/sedonaState`, `/file/*`, `/library/` and editor assets. G-0 to G-5 are done; v2.9.0 (`03b4979`, 2026-04-25) and the fix-pack tagged v2.9.1 (`44de90b`, 2026-04-29) were production bring-up fixes. Open: G-6 (`.sab` → `.sax` endpoint), G-7 (`.deb` bundle, SCRAM on `/live` and `/file`), G-8 (protocol tests, setup guide).

### §2.9 Maintenance steps S-1 to S-9 (2026-11)

Tracked step by step in the internal progress notes; summary of what reached `master`:

| Step | Result |
|---|---|
| S-5 | CI quality gate green again (clippy, IPC tests on the Unix transport, PWM test). Toolchain pinned to 1.99.1 (`rust-toolchain.toml`, CI); MSRV 1.82. Project rules tracked under `.claude/rules/`. |
| S-6 | Credential-bearing `tools/` scripts moved out of git (`.gitignore` credentials block). |
| S-7 | Log retention: daily files `sandstar-engine.log.YYYY-MM-DD`, newest 14 kept (`--log-max-files`, `SANDSTAR_LOG_MAX_FILES`), pruned by the date in the file name. roxWarp timer intervals validated on load (1..=86400 s). Table files fall back to the `tables.csv` path. Watchdog logs why a device cannot be opened. |
| S-8 | GPIO60 (external watchdog) export is logged and retried in `initialize.sh`; the engine retries opening GPIO60 for about 5 minutes after a failed start. Deployed as `2.9.0+3b8e97c`. |
| S-9 | `tools/gates.sh` quality-gate runner; a hook that blocks file-tool access to credential files; process rules (branch per step, status ladder, plan approval). |

---

## §3 REST API surface

Router: `router_with_auth` in `crates/sandstar-server/src/rest/mod.rs`, plus routers merged in `main.rs`. All routes are on the REST port (default 8085). The "Auth" column says whether the route is behind the `check_auth` middleware. With no token and no SCRAM user configured, `check_auth` lets every request through.

| Routes | Auth | Purpose |
|---|---|---|
| `GET /`, `/editor` + assets | no | Dashboard and bundled editor page |
| `GET /api/about`, `/api/ops`, `/api/formats`, `/api/read`, `/api/status`, `/health`, `/api/metrics`, `/metrics`, `/api/diagnostics`, `/api/channels`, `/api/polls`, `/api/tables`, `/api/history/{channel}`, `GET /api/pointWrite` | no | Read-only metadata, data and observability (`/metrics` is Prometheus text) |
| `POST /api/auth` | — | SCRAM handshake |
| `POST /api/pointWrite`, `/api/pollNow`, `/api/reload`, `/api/watchSub`, `/api/watchUnsub`, `/api/watchPoll`, `/api/hisRead`, `/api/nav`, `/api/invokeAction` | **yes** | Haystack ops and engine writes |
| `GET /api/ws` | handshake | Haystack-over-WebSocket (JSON) with SCRAM inside the socket |
| `GET /api/drivers`, `/api/drivers/{id}/status`, `/api/drivers/{id}/learn` | no | Driver listing |
| `POST /api/drivers`, `DELETE /api/drivers/{id}`, `POST /api/drivers/{id}/open\|close\|ping\|message\|write`, `POST /api/syncCur` | **yes** | Driver lifecycle ([DRIVER_REST_API.md](DRIVER_REST_API.md)) |
| `GET /api/sox/tree`, `/api/sox/comp/{id}`, `/api/sox/palette`, `/api/sox/names` | no | SOX component tree (with `--sox`) |
| `POST/DELETE/PUT /api/sox/comp*`, `/api/sox/link` | **no** ⚠ | SOX component edits (with `--sox`) |
| `GET /api/rows`, `GET /live`, `GET /live/sedonaState`, `GET/PUT /file/*`, `/library/` | **no** ⚠ | Editor WebSockets and file transfer (with `--sox`) |
| `GET/PUT/DELETE /api/tags*` | **no** ⚠ | Dynamic tags |
| `/api/alerts/*` (config, recipients, test e-mail) | **no** ⚠ | Alert management |
| `GET /roxwarp`, `GET /api/cluster/status`, `POST /api/cluster/query` | **no** ⚠ | roxWarp (with `--cluster`; mTLS applies only on the separate cluster port) |
| `/api/sim/*` | no | Only in builds with the `simulator-hal` feature |

**Security posture:**
- `--http-bind` defaults to `127.0.0.1`. ⚠ The packaged systemd unit passes `--http-bind 0.0.0.0` and configures no auth, so on the field device every route above is reachable on the LAN without credentials.
- Bearer token (`--auth-token` / `SANDSTAR_AUTH_TOKEN`) and SCRAM-SHA-256 (`--auth-user` / `--auth-pass`, `SANDSTAR_AUTH_USER` / `SANDSTAR_AUTH_PASS`).
- REST TLS is behind the `tls` cargo feature; the release alias `arm-build-svm` does not enable it.
- Filter depth cap 32, watch cap 64, max channels per watch 256, body limit 1 MB, rate limit on by default (100 req/s).
- CORS: any origin, with an explicit method and header list.
- Engine calls from REST, the `/api/ws` WebSocket, SOX, roxWarp and drivers wait at most 10 s (`ENGINE_REPLY_TIMEOUT`). REST then answers `503 {"err":"engine timeout"}`; for writes and other state-changing commands the text adds "may still be applied" (R-28, S-20).
- On an engine timeout (R-28, S-21):
  - `/health` waits at most 2 s and answers `503 {"healthy":false,"uptimeSecs":0}`; a healthy engine still gives 200.
  - `/api/ws` replies with error code `ENGINE_TIMEOUT` and ends the current push round; the timed-out watch asks for all values on its next poll, because the queued poll may still advance the change baseline.
  - Since S-22, `/api/ws` push polls and the SOX tree refresh run beside their event loops, so pings, client messages and DASP packets are still handled while the engine is slow. The next round or refresh waits 10 s after a timeout. Client requests (`subscribe`, `unsubscribe`, `refresh`) still wait inline for their own reply.
  - Watches of closed `/api/ws` connections are closed by one shared task that retries after a timeout (pauses growing from 10 s to 10 min) and gives up after the 1 h lease. A push result taken before a client `refresh` or re-subscribe is dropped, so an older `update` never follows a newer `snapshot`.
  - `localIo` marks the points `Down` (`RemoteDown`) instead of `Fault` and stops the round.
- Only Ctrl+C (SIGINT) and SIGHUP are handled; `systemctl stop` (SIGTERM) ends the process without graceful shutdown.

---

## §4 Deployment state

Host addresses, SSH details and site names are deliberately not written in this repository. They live in the machine-local connection files used by the deploy tooling.

| Item | State |
|---|---|
| Field devices | One BeagleBone (Debian 9, kernel 4.9, armv7). Older device labels in this repo's history refer to the same unit. |
| Installed build | `2.9.0+3b8e97c` |
| Service | `sandstar-engine.service`: `--http-bind 0.0.0.0 --sox --cluster`, no auth configured. The C service no longer exists. |
| Drivers | A site-local systemd drop-in enables a BACnet driver; it logs a steady stream of `comm fault` warnings that are not yet explained. No MQTT driver is confirmed. |
| Logs | `/var/log/sandstar/sandstar-engine.log.YYYY-MM-DD`, newest 14 kept. The systemd journal is volatile and holds well under an hour. |
| Watchdogs | The GPIO60 external watchdog sometimes cannot be opened at start; since S-8 the export is logged and retried. Root cause open. |

**Build** (from Windows):
- Load the machine-local cross environment first (`arm-env` script, provides `CC`/`AR`), then `cargo arm-build-svm && cargo arm-deb --no-strip`.
- The linker is a Zig wrapper targeting glibc 2.24.
- The `.deb` bundles lookup tables from a sibling `shaystack` checkout.

**Deploy:** follow [DEPLOYMENT_CHECKLIST.md](DEPLOYMENT_CHECKLIST.md). Always take a full backup of the installed version first.

**Operator documents:**
- [DEPLOYMENT_CHECKLIST.md](DEPLOYMENT_CHECKLIST.md) — deploy and rollback runbook
- [BACNET_SETUP.md](BACNET_SETUP.md) — enable, configure, firewall, BBMD, verify, troubleshoot
- [MQTT_SETUP.md](MQTT_SETUP.md) — same six sections for MQTT
- [DRIVER_REST_API.md](DRIVER_REST_API.md) — REST surface for driver lifecycle (list/create/open/close/ping/write/delete/syncCur/message) + WS push behavior, with copy-pasteable curl recipes
- [HARDCODED_LIMITS.md](HARDCODED_LIMITS.md) — audit of tuning constants

---

## §5 Quality gates

- **One command:** `bash tools/gates.sh`, which runs:
  1. `cargo fmt --all -- --check`
  2. `cargo clippy --workspace -- -D warnings`
  3. `cargo test --workspace --no-fail-fast`
  4. `cargo check -p sandstar-server --no-default-features --features linux-hal,svm`
  5. `docs` reminder (not a gate): lists documents from `tools/docs-map.tsv` whose code paths changed on the branch while the document did not (rule: `.claude/rules/workflow.md` "Doküman disiplini").
- **Tests:** 2748 passed on Windows, 2764 on Linux CI, 0 failed. `test_sys_ticks` is flaky.
- **Toolchain:** pinned to 1.99.1 (`rust-toolchain.toml`, CI); MSRV 1.82.
- **Not clean yet:** `cargo clippy --all-targets` (lints in test code).
- **CI:** fmt, clippy, tests, then an ARM build on push. The ARM build uses `linux-hal` without `svm` and produces no `.deb`.
- **Security audit (Phase 5.7 + 6.5):** the 10 issues found then were resolved. A later full review found new open items: no auth in the field configuration, write routes outside auth, panic paths on network input in release builds (`panic = "abort"`), and SIGTERM handling.

**Coverage highlights (hand-inspection, not formal tool):**
- Frame codecs (BACnet, MQTT) have round-trip + wire-snapshot tests pinning exact byte sequences
- Mock-socket tests for every read / write / subscribe path
- 10+ E2E tests spawn UDP mock BACnet devices for full-stack validation
- MQTT has 5 integration tests (config parsing, JSON shape, env-var absence)
- Subscription renewal tested with accelerated intervals

**Observability:**
- `tracing` spans across driver `open` / discovery / `sync_cur` / `write` paths
- Tick task emits structured `ok=N err=N write_err=N` log lines every 5s
- Pre-existing lints in unrelated modules (`sox_handlers.rs`, `sax_converter.rs`, …) are documented but don't block the driver gates

---

## §6 What's pending

Rewritten 2026-11-18. Ranked by risk first, then value.

### 🔴 Security and robustness (field device affected)
1. **Auth in the field configuration.** The unit binds `0.0.0.0` with no auth; and even with auth on, SOX edits, `/live`, `/file`, tags, alerts and cluster routes bypass `check_auth` (§3).
2. **Panic paths on network input.** Release builds abort on panic; known cases include a short BACnet BVLL frame and some lock-poison `unwrap`s on REST paths.
3. **Graceful shutdown.** Handle SIGTERM and wire `with_graceful_shutdown`.
4. **Numeric output guards.** Not every value leaving the process is checked with `is_finite` (engine outputs, roxWarp feed).

### 🟡 Correctness gaps in shipped features
5. **MQTT reconnect.** The event loop exits on the first error and the driver status stays `Ok`.
6. **BACnet COV in production.** `add_watch` has no production caller, so COV subscriptions never start; values arrive by the 5 s poll.
7. **REST-created drivers** get no poll tick and are lost on restart.
8. **Zinc request bodies.** `/api/formats` advertises `text/zinc` receive, but every POST handler takes JSON.
9. **Sedona VM.** Decide its future: fix native memory access, mark experimental, or remove. It is not used in production.

### 🟢 Feature work
10. **14.0G remainder:** G-6 `.sab` → `.sax` endpoint, G-7 packaging and auth, G-8 tests and guide.
11. **roxWarp 9.0d** (Fantom pod) and putting the typed binary-Trio format on the wire, if a non-Rust peer is planned.
12. **Haystack ops not implemented:** `watchList`, `hisWrite`, `commit`, `root`, `xeto`.
13. **Field validation:** 24-48 h soak test; vendor BACnet device; resolve the BACnet `comm fault` stream and the implausible table-converted reading.
14. **Housekeeping:** align `Cargo.toml`, `CHANGELOG.md` and the `v2.9.1` tag; README version; `clippy --all-targets`.

---

## §7 Version timeline

One-week sprint 2026-04-10 → 2026-04-17:

| Date | Version | Scope |
|---|---|---|
| 2026-04-10 | 2.0.0 | Pure Rust VM complete — production baseline. Zero C. |
| 2026-04-14 | 2.1.0 | BACnet B1 frame codec deployed to 1-11 |
| 2026-04-14 | 2.1.1 | Fix: call `open_all()` on registered drivers |
| 2026-04-14 | 2.1.2 | Discovery debug logging (used to find firewalld issue) |
| 2026-04-15 | 2.2.0 | BACnet B7 WriteProperty |
| 2026-04-15 | 2.3.0 | BACnet B9 ReadPropertyMultiple |
| 2026-04-16 | 2.4.0 | BACnet B8 SubscribeCOV wire-level |
| 2026-04-16 | 2.5.0 | BACnet B8.1 COV reception + B10 BBMD |
| 2026-04-16 | 2.5.1 | BACnet B8.2 COV renewal |
| 2026-04-16 | 2.6.0 | Poll integration: `register_point` + `add_poll_bucket` |
| 2026-04-16 | 2.6.1 | Tick task — `sync_cur` actually fires |
| 2026-04-16 | 2.7.0 | Stage 2 — sync_cur results → engine channels visible in `/read` |
| 2026-04-17 | 2.8.0 | MQTT M1-M4 complete + live broker validated |
| 2026-04-17 | 2.8.1 | Phase 12.0A — generic `DriverLoader` trait + `load_drivers<L>`; rest/mod.rs shrunk by 417 lines |
| 2026-04-17 | 2.8.2 | Phase 12.0B — `PointStatus::Remote{Disabled,Down,Fault}` + `from_driver_error`; sync_cur errors now set per-point status in the actor |
| 2026-04-17 | 2.8.3 | Phase 12.0C — `SyncContext`/`WriteContext` callback-style API; `Driver` + `AsyncDriver` trait `sync_cur`/`write` now take `&mut ctx` instead of returning `Vec`. `AnyDriver` wrapper preserves the old Vec signature for actor/REST callers. Four driver impls migrated. |
| 2026-04-17 | 2.8.4 | Phase 12.0D — `CovEvent` broadcast channel. `DriverHandle::subscribe_cov()` returns a `tokio::sync::broadcast::Receiver<CovEvent>`; actor `sync_all` emits on change-of-value per point. WS bridge deferred to 12.0D.WS (trigger-based). |
| 2026-04-17 | 2.8.5 | Phase 12.0G — runtime driver-lifecycle REST: `POST /api/drivers` (create), `POST /api/drivers/{id}/{open,close,ping}`, `DELETE /api/drivers/{id}`, `POST /api/syncCur`. All behind bearer/SCRAM auth. Factory dispatches bacnet/mqtt by `driver_type` string. |
| 2026-04-17 | 2.8.6 | Phase 12.0E — custom driver messages: `DriverMessage{id, payload}` + `on_receive` trait method + `DriverHandle::send_message` + `POST /api/drivers/{id}/message`. Default impl returns NotSupported; drivers opt in per-id. |
| 2026-04-17 | 2.8.7 | Phase 12.0D.WS — WebSocket COV push. Each WS session subscribes to the CovEvent broadcast; matching watches are force-polled on next push-timer tick (~200 ms latency bound). `RecvError::Lagged`/`Closed` handled defensively. |
| 2026-04-17 | 2.8.8 | Phase 12.0F — `LocalIoDriver` migrated to `AsyncDriver` with optional engine-backed delegation. One instance registered in production via `rest/mod.rs`. Engine's own poll loop untouched; driver is a façade that exposes local channels through `/api/drivers`. |

| 2026-04-18 | 2.8.9 | 14.0Aj reorder endpoint, 14.0Ak SOX API integration tests, 9.0b mDNS peer discovery (opt-in) |
| 2026-04-20 | 2.8.10 | Prometheus `GET /metrics` (18 counters + 5 gauges, `sandstar_build_info`) |
| 2026-04-25 | 2.9.0 | Phase 14.0G native editor backend, production bring-up fixes (`03b4979`) |
| 2026-04-29 | tag v2.9.1 | 14.0G fix-pack (`44de90b`); `Cargo.toml` still says 2.9.0 |
| 2026-11 | 2.9.0+hash | Maintenance steps S-5 to S-9 (§2.9); field device on `2.9.0+3b8e97c` |

---

## §8 Document map

| Doc | Purpose |
|---|---|
| [ROADMAP_v2.md](ROADMAP_v2.md) | Master phase ledger, 860 lines — everything from Phase 0 to Phase 14. Covers SOX details, security audit, feature gaps, research summaries. |
| [IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md](IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md) | Project A + B per-phase detail, 838 lines. Design rationale, byte layouts, per-phase scope + completion notes. |
| [IMPLEMENTATION_PLAN_MQTT.md](IMPLEMENTATION_PLAN_MQTT.md) | Project C plan — scope, schema, risk table, completion criteria, progress log. |
| [IMPLEMENTATION_PLAN_DRIVER_FRAMEWORK.md](IMPLEMENTATION_PLAN_DRIVER_FRAMEWORK.md) | Phase 12 plan. Completed (12.0A-G). |
| [IMPLEMENTATION_PLAN_NATIVE_EDITOR_BACKEND.md](IMPLEMENTATION_PLAN_NATIVE_EDITOR_BACKEND.md) | Phase 14.0G plan (native backend for the external Vue editor). About 75% done. |
| [NOTES_EDITOR_ROWS_SPIKE.md](NOTES_EDITOR_ROWS_SPIKE.md), [NOTES_FANTOM_MSGTYPE_SEMANTICS.md](NOTES_FANTOM_MSGTYPE_SEMANTICS.md) | 14.0G spike and reference notes (implemented). |
| [PURE_RUST_PLAN.md](PURE_RUST_PLAN.md) | Historical: plan to rewrite the VM in pure Rust (complete). |
| [PURE_RUST_VM_COMPLETION_PLAN.md](PURE_RUST_VM_COMPLETION_PLAN.md) | Historical: 5-phase plan to make pure-Rust the default (complete). |
| [BACNET_SETUP.md](BACNET_SETUP.md) | **Operator guide** for BACnet. Enable, configure, firewall, BBMD, verify, troubleshoot. |
| [MQTT_SETUP.md](MQTT_SETUP.md) | **Operator guide** for MQTT. Same six-section structure. |
| [DRIVER_REST_API.md](DRIVER_REST_API.md) | **Operator/integrator guide** for the driver REST surface (Phase 12 endpoints). Auth model, per-endpoint curl recipes, WS push behavior, error codes, troubleshooting. |
| [DEPLOYMENT_CHECKLIST.md](DEPLOYMENT_CHECKLIST.md) | Deploy, verify and rollback runbook (rewritten 2026-11-18). |
| [HARDCODED_LIMITS.md](HARDCODED_LIMITS.md) | Audit of tuning constants across REST/WS/engine/sensors/HAL/IPC/SVM. |
| `../research/` | Pre-migration analysis (20 numbered docs, 00–19), each with a 2026-11-18 status block. Summary: [PROGRESS_REPORT_2026_11_18.md](../research/PROGRESS_REPORT_2026_11_18.md); older snapshots `STATUS_REPORT_2026_04_09.md` and `PROGRESS_REPORT_2026_04_10.md` are superseded. |

---

## Maintenance

This document is the **derived snapshot**. The plan documents listed above are the **source of truth** — when a phase ships, the plan doc's phase row gets the ✅ and commit hash, and this overview gets a matching one-line update in the relevant §2 table and §7 timeline.

**Don't use this file to track work in progress** — use the plan docs' progress logs for that. This is the "are we done?" view, not the "what's next?" view.

---

*Generated 2026-04-17 from the plan documents in this directory; header, §1, §2.7-§2.9, §3-§8 revised 2026-11-18 against the code at `b48484b`. For the live authoritative status, follow the per-section links — this overview is a snapshot.*
