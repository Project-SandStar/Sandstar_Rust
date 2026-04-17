# Sandstar Rust — Progress Overview

**Last updated:** 2026-04-17
**Current version:** v2.8.1 (BeagleBone 1-11); v2.0.0 still on Device 1-3 (Todd Air Flow)
**Workspace:** 7 crates, ~40,000 LOC, **2,643 tests passing, 0 clippy warnings, 0 failures**
**GitHub:** https://github.com/Project-SandStar/Sandstar_Rust

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
| Pure Rust VM migration | ✅ Complete | [PURE_RUST_PLAN.md](PURE_RUST_PLAN.md), [PURE_RUST_VM_COMPLETION_PLAN.md](PURE_RUST_VM_COMPLETION_PLAN.md) |
| SOX/DASP protocol (20/20 commands) | ✅ Complete | [ROADMAP_v2.md](ROADMAP_v2.md) §Phase 8.0A-SOX |
| Visual DDC programming + dataflow engine | ✅ Complete (full loop verified on hardware) | [ROADMAP_v2.md](ROADMAP_v2.md) |
| Haystack filter (`not` compound, `->` path) | ✅ Complete — Project A | [IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md](IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md) |
| BACnet/IP driver (read + write + COV + RPM + BBMD) | ✅ Complete — Project B, 11 phases, live-validated | [IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md](IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md), [BACNET_SETUP.md](BACNET_SETUP.md) |
| MQTT driver | ✅ Complete — Project C, 4 phases, live-validated | [IMPLEMENTATION_PLAN_MQTT.md](IMPLEMENTATION_PLAN_MQTT.md), [MQTT_SETUP.md](MQTT_SETUP.md) |
| Poll integration (driver → engine channels) | ✅ Complete — shared tick task in rest/mod.rs | [IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md](IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md) Stage 1/2 |
| REST API + WebSocket | ✅ Complete — 14 endpoints, auth, TLS, rate limit | [ROADMAP_v2.md](ROADMAP_v2.md) §Phase 1 |
| Security hardening | ✅ Complete | [ROADMAP_v2.md](ROADMAP_v2.md) §Phase 5.7, §Phase 6.5 |
| Visual DDC editor (web UI) | 🟡 14.0A in progress, 14.0B–F planned | [ROADMAP_v2.md](ROADMAP_v2.md) §Phase 14.0 |
| Driver Framework v2 (Phase 12) | 🟡 12.0A complete; 12.0B–D deferred | [IMPLEMENTATION_PLAN_DRIVER_FRAMEWORK.md](IMPLEMENTATION_PLAN_DRIVER_FRAMEWORK.md) |
| Clustering — roxWarp (Phase 9) | ⬜ Not started — low priority | [ROADMAP_v2.md](ROADMAP_v2.md) §Phase 9 |
| Dynamic Slots (Phase 13) | ⬜ Not started — low priority | [ROADMAP_v2.md](ROADMAP_v2.md) §Phase 13 |
| Hardware sensor validation on 1-11 | 🟡 Deferred — no sensors physically attached | — |
| Real BACnet vendor device validation | 🟡 Pending — no hardware available | — |

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

**Live validation:** mosquitto 2.1.2 on Windows dev box `192.168.1.9:1884` → `mosquitto_pub -t sandstar/test/humidity -m "55.5"` → cache → `sync_cur` → `write_channel` at priority 16 → `/api/read?id=103` returned `cur=55.5`. (ISP blocks outbound TCP 1883 to public brokers; local mosquitto was the workaround.)

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

Behavior preserved identically — 2,643 tests still pass, deployed v2.8.1 to 1-11 with BACnet driver still running cleanly.

Phases 12.0B (shared helpers module), 12.0C (richer trait abstractions), 12.0D (LocalIoDriver unification) are **deferred** — trigger is a concrete need (a 4th driver, or specific code smell).

---

## §3 REST API surface

Defined in `crates/sandstar-server/src/rest/handlers.rs`. 14 endpoints:

| Endpoint | Purpose |
|---|---|
| `GET /api/status` | Engine health + poll counts |
| `GET /api/channels` | List channels |
| `GET /api/polls` | Poll buckets |
| `GET /api/tables` | Lookup tables |
| `GET /api/read` | Read by id or Haystack filter (Zinc + JSON) |
| `POST /api/write`, `GET /api/pointWrite` | Writes (priority array) |
| `GET /api/about`, `GET /api/ops`, `GET /api/formats` | Metadata |
| `GET /api/drivers`, `/api/drivers/{id}/status\|learn\|write` | Driver framework |
| `POST /api/hisRead` | Historical data from ring buffer |
| `GET /health`, `GET /api/metrics`, `GET /api/diagnostics` | Observability |
| `GET /api/ws` | Haystack-over-WebSocket (31 tests) |
| `GET /api/sox/*`, `GET /api/rows` | SOX editor backend (14.0A) |
| `GET /roxwarp` | roxWarp cluster WebSocket (when `--cluster` flag is set) |

**Security:**
- `--http-bind` defaults to 127.0.0.1; explicit `0.0.0.0` required for LAN exposure
- Bearer token auth (`SANDSTAR_AUTH_TOKEN`) + SCRAM-SHA-256 (`--auth-user` / `--auth-pass`)
- TLS via rustls
- Filter-parse depth cap (32), watch cap (64), max channels per watch (256)
- Default body limit 1 MB, rate limiting available
- Socket perms 0660 (Unix)
- CORS permissive (embedded device accessed from varying IPs)

---

## §4 Deployment state

| Device | IP | Version | Notes |
|---|---|---|---|
| 1-3 (Todd Air Flow) | 192.168.1.3 | **v2.0.0** | Pure-Rust baseline. 8.8 MB RSS. Production HVAC. |
| 1-11 | 192.168.1.11 | **v2.8.0** | BACnet + MQTT drivers built in. Both env vars currently unset — drivers quiescent. No sensors physically attached to this dev board. |
| Baha (211-135) | NAT via 172.28.109.221 | — | Unreachable from current Windows dev box. |

**Cross-compile chain** (from Windows):
- Zig CC wrappers at `C:\czb\` (zigcc-arm.bat linker, zigcc-arm-cc.bat C, zigar-arm.bat archiver)
- Zig binary via `pip install ziglang` (`C:\Python314\Lib\site-packages\ziglang\zig.exe`)
- GLIBC target: 2.24 (Debian 9 Stretch)
- `cargo arm-build-svm && cargo arm-deb --no-strip` produces the .deb in ~80s

**Deploy commands:**
- `scp -P 1919 ... .deb eacio@<ip>:/home/eacio/`
- `ssh ... "sudo systemctl stop sandstar-engine && sudo dpkg -i ... && sudo systemctl start sandstar-engine"`
- The `tools/installSandstarRust.sh` wrapper chokes on paths with spaces; manual scp+ssh is the current path.

**Operator documents:**
- [BACNET_SETUP.md](BACNET_SETUP.md) — enable, configure, firewall, BBMD, verify, troubleshoot
- [MQTT_SETUP.md](MQTT_SETUP.md) — same six sections for MQTT
- [DEPLOYMENT_CHECKLIST.md](DEPLOYMENT_CHECKLIST.md) — hardware install verification runbook
- [HARDCODED_LIMITS.md](HARDCODED_LIMITS.md) — audit of ~90 tuning constants

---

## §5 Quality gates

- **Tests:** 2,643 passing across 21 suites (0 failures, 1 ignored)
- **Clippy:** `cargo clippy -p sandstar-server -- -D warnings` clean
- **Format:** `cargo fmt --all` clean
- **Cross-compile:** ARM builds cleanly with zig toolchain
- **Security audit:** 10 issues identified, 3 critical / 2 high / 5 medium — all resolved (Phase 5.7 + 6.5)

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

Ranked by value, not effort. "Ripe" items have the highest leverage because the supporting work is now in place.

### 🟢 Highest value (ripe)
1. **Phase 12 — Driver Framework v2.** Three drivers now shipped; common patterns visible (shared `transact_inner`, poll bucket + tick + `write_channel`, value caching, value-path extraction). A trait-based refactor would make driver #4 significantly smaller. Research: [research/18_*](../research/18_SEDONA_DRIVER_FRAMEWORK_V2.md).
2. **Real-hardware validation on 1-11.** The device currently has no sensors attached — existing ADC readings fluctuate or report the BeagleBone's own temperature. Either attach physical sensors, migrate Sandstar to a real HVAC deployment, or accept 1-11 as software-only.
3. **Vendor BACnet device validation.** Our Python sim validates every byte layout. A real Siemens/Trane/Honeywell device may expose spec-interpretation edge cases (segmentation, large object arrays, unusual tag encodings).

### 🟡 Medium value
4. **Phase 14.0B–F — Visual DDC editor web UI.** 14.0A (REST endpoints) is in progress but paused while driver work shipped. HTML scaffold (14.0B), canvas engine (14.0C), interactions (14.0D), palette CRUD (14.0E), save/load (14.0F) remain.
5. **Long-running soak on 1-11.** Run BACnet sim + local mosquitto feeding values for 24–48 h. Verify subscription renewals, memory stability, no silent task panics. Zero code required.
6. **Finish the BACnet recv-loop refactor.** `read_property_generic` and `read_properties_multiple` still carry inline loops because they return `BacnetError` instead of `DriverError`. Marginal value.
7. **MQTT reconnect quirk.** mosquitto logged a brief reconnect during first connect; data flow unaffected but worth a longer-run look.

### 🔴 Deferred / low priority
8. **Phase 9 — roxWarp clustering.** Peer gossip + distributed queries. Large scope, no immediate user.
9. **Phase 13 — Dynamic slots.** Runtime component type modification. Niche. Research: [research/19_*](../research/19_DYNAMIC_SLOTS.md).
10. **Phase 5.8a–e — Hardware soak test.** Awaiting hardware access.

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

13 minor versions in 8 days. Current state: both drivers ready on 1-11, both quiescent until a real broker / device appears.

---

## §8 Document map

| Doc | Purpose |
|---|---|
| [ROADMAP_v2.md](ROADMAP_v2.md) | Master phase ledger, 860 lines — everything from Phase 0 to Phase 14. Covers SOX details, security audit, feature gaps, research summaries. |
| [IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md](IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md) | Project A + B per-phase detail, 838 lines. Design rationale, byte layouts, per-phase scope + completion notes. |
| [IMPLEMENTATION_PLAN_MQTT.md](IMPLEMENTATION_PLAN_MQTT.md) | Project C plan — scope, schema, risk table, completion criteria, progress log. |
| [IMPLEMENTATION_PLAN_DRIVER_FRAMEWORK.md](IMPLEMENTATION_PLAN_DRIVER_FRAMEWORK.md) | Phase 12 plan. 12.0A (loader refactor) complete; 12.0B-D deferred. |
| [PURE_RUST_PLAN.md](PURE_RUST_PLAN.md) | Historical: plan to rewrite the VM in pure Rust (complete). |
| [PURE_RUST_VM_COMPLETION_PLAN.md](PURE_RUST_VM_COMPLETION_PLAN.md) | Historical: 5-phase plan to make pure-Rust the default (complete). |
| [BACNET_SETUP.md](BACNET_SETUP.md) | **Operator guide** for BACnet. Enable, configure, firewall, BBMD, verify, troubleshoot. |
| [MQTT_SETUP.md](MQTT_SETUP.md) | **Operator guide** for MQTT. Same six-section structure. |
| [DEPLOYMENT_CHECKLIST.md](DEPLOYMENT_CHECKLIST.md) | Hardware install verification runbook. |
| [HARDCODED_LIMITS.md](HARDCODED_LIMITS.md) | Audit of ~90 tuning constants across REST/WS/engine/sensors/HAL/IPC/SVM. |
| [PROGRESS_REPORT.md](PROGRESS_REPORT.md) | Previous (shorter) snapshot. Superseded by this OVERVIEW.md. |
| `../research/` | Pre-migration analysis (22 numbered docs, 00–19). |

---

## Maintenance

This document is the **derived snapshot**. The plan documents listed above are the **source of truth** — when a phase ships, the plan doc's phase row gets the ✅ and commit hash, and this overview gets a matching one-line update in the relevant §2 table and §7 timeline.

**Don't use this file to track work in progress** — use the plan docs' progress logs for that. This is the "are we done?" view, not the "what's next?" view.

---

*Generated 2026-04-17 from the plan documents in this directory. For the live authoritative status, follow the per-section links — this overview is a snapshot.*
