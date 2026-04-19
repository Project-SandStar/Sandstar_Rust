# Changelog

All notable changes per minor version. Patch-level commits within a version (build fixes, doc tweaks) are rolled up.

Format loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project uses a single workspace version — bumps apply to every crate.

---

## [2.8.10] — 2026-04-20

### Added
- **Prometheus `/metrics` endpoint** (Option 1 observability pass). Text exposition format v0.0.4, zero new dependencies.
  - 18 counters and 5 gauges covering poll cycles, HAL errors, REST/IPC traffic, WS/RoWS activity.
  - `sandstar_build_info{version,git_hash}` gauge carrying build metadata as labels.
  - 3 new driver-framework counters wired into the actor's `sync_all` path: `sandstar_driver_sync_total`, `sandstar_driver_sync_errors_total`, `sandstar_driver_cov_events_total` (Phase 12.0B + 12.0D metrics).
  - Legacy `/api/metrics` JSON endpoint preserved.
  - 3 unit + 3 HTTP integration tests. Operator docs in `DRIVER_REST_API.md` §7.
- Deployed to production; `sandstar_hal_errors_total` immediately surfaced the known-bad SDP810 I2C sensor.

## [2.8.9] — 2026-04-18

### Added
- **Phase 14.0Aj** — `PUT /api/sox/comp/{id}/reorder` for child-order editing through the Web DDC REST surface.
- **Phase 14.0Ak** — `tests/sox_api_integration.rs`, 13 HTTP round-trip tests covering every `/api/sox/*` endpoint.
- **Phase 9.0b** — mDNS peer discovery for roxWarp clusters via `mdns-sd`. Opt-in through `ClusterConfig.enable_mdns`; static peer list preserved.

### Changed
- Deployed to production BeagleBone (new DHCP IP 192.168.1.4).
- ROADMAP audit pass 2: reconciled every `<100%` research-doc row; research coverage ~72% → ~99.75%. Only Doc 04 retains `<100%` (Xeto/Commit/Root Haystack ops, deferred).

## [2.8.8] — 2026-04-17

### Added
- **Phase 12.0F** — `LocalIoDriver` migrated to `AsyncDriver` with optional `EngineHandle` delegation. Production instance registered in `rest/mod.rs`. Engine's own poll loop untouched — pure façade.
- 3 engine-backed unit tests (mock `EngineCmd` responder).

### Removed
- Legacy sync `DriverManager` + stub `drivers::LocalIoDriver` — ~800 LOC of parallel-path test-only code deleted. `Driver` trait preserved for `AnyDriver::Sync` + mock fixtures.

### Deployed
- v2.8.2 → v2.8.8 on device; Phase 12 validated end-to-end with BACnet simulator + `ws_latency_test.py`.

## [2.8.7] — 2026-04-17

### Added
- **Phase 12.0D.WS** — bridged `CovEvent`s into the Haystack `/api/ws`. Each session subscribes to the broadcast, and matching watches force-poll on next push-timer tick (~200 ms latency bound). `RecvError::{Lagged,Closed}` handled defensively.

## [2.8.6] — 2026-04-17

### Added
- **Phase 12.0E** — `DriverMessage{id, payload}` + `on_receive` trait method (default `NotSupported`). `AnyDriver` dispatch, `DriverCmd::SendMessage`, `DriverHandle::send_message`, `POST /api/drivers/{id}/message` (auth-gated, 501 for un-overridden drivers).

## [2.8.5] — 2026-04-17

### Added
- **Phase 12.0G** — runtime driver lifecycle REST: create / open / close / ping / delete, plus batch `POST /api/syncCur`. Auth-gated; factory dispatches `bacnet`/`mqtt` by string.
- `check_auth` promoted to `pub(crate)`; closed a pre-existing gap where `POST /api/drivers/{id}/write` was unprotected.

## [2.8.4] — 2026-04-17

### Added
- **Phase 12.0D** — `CovEvent` broadcast channel. `DriverHandle::subscribe_cov()` returns a fresh `tokio::sync::broadcast::Receiver`. `sync_all` emits on first-read + change-of-value (bit-level `to_bits()` compare; NaN retriggers). Errors don't emit. Late subscribers get no backlog.
- `DEFAULT_COV_CAPACITY = 512`. 6 unit tests.

## [2.8.3] — 2026-04-17

### Changed
- **Phase 12.0C** — `sync_cur`/`write` trait methods switched from returning `Vec<(u32, Result<_>)>` to taking `&mut SyncContext` / `&mut WriteContext`. `AnyDriver` wrapper drains contexts for backward-compat — actor + REST callers unchanged. Four drivers migrated.

## [2.8.2] — 2026-04-17

### Added
- **Phase 12.0B** — `PointStatus::{RemoteDisabled, RemoteDown, RemoteFault(String)}` variants. `from_driver_error(&DriverError)` maps comm faults to the right remote variant. `sync_all` populates per-point status so `/api/drivers/{id}/status` shows real remote health.

## [2.8.1] — 2026-04-17

### Added
- **Phase 12.0A** — generic `DriverLoader<L>` trait. Replaced ~417 lines of `load_bacnet_drivers` + `load_mqtt_drivers` duplication with one helper.

## [2.8.0] — 2026-04-17

### Added
- **Project C (Phases M1–M4)** — pure-Rust MQTT v3.1.1 driver via `rumqttc` 0.24. `MqttValueCache` (600 s max age), publish on write (plain `f64` or `{"value":N}` envelope iff `value_path` set), shared 5 s tick task pattern from BACnet. Value-flow verified against local mosquitto (`mqtt_pub` → `/api/read`).

## [2.7.0] — 2026-04-16

### Added
- BACnet Stage 2 — sync results wired to `engine.write_channel` at level 16, duration 30 s. BACnet values now flow end-to-end; `/api/read?id=N` returns the polled value.

## [2.6.x] — 2026-04-15

### Added
- **v2.6.1** — shared driver tick task; `handle.sync_all()` every 5 s with `MissedTickBehavior::Skip`.
- **v2.6.0** — `load_bacnet_drivers()` calls `register_point` + `add_poll_bucket` for each configured object.
- BACnet operator guide (`BACNET_SETUP.md`).

## [2.5.x] — 2026-04-14

### Added
- **v2.5.1** — **Phase B8.2** — COV subscription renewal at 240 s (80% of 300 s lifetime) piggybacked onto `sync_cur`, no background task.
- **v2.5.0** — **Phase B10** — BVLL Register-Foreign-Device + Forwarded-NPDU + BVLL-Result. Auto-registration in `open()` (non-fatal on failure). Who-Is over local broadcast + BBMD distribute.
- **Phase B8.1** — `CovCache` with `max_age`-based expiry processed inline on all 6 recv loops as a side-effect.

## [2.3.0] — 2026-04-13

### Added
- **Phase B9** — ReadPropertyMultiple. `RpmRequestSpec` + `RpmResult` structs, APDUs, `BacnetDriver::read_properties_multiple`, `sync_cur` refactored to batch by device with fallback to individual reads on error.

## [2.2.0] — 2026-04-12

### Added
- **Phase B7** — BACnet WriteProperty. `encode_write_property`, `BacnetDriver::write_property`, `AsyncDriver::write()` dispatcher (Real for AI/AO/AV, Enumerated for BI/BO/BV, priority 16). Validated end-to-end against `bacnet_sim.py`.

## [2.1.x] — 2026-04-11

### Added
- **Phase B5** — REST integration via `DriverHandle` (replaced legacy `SharedDriverManager`). `SANDSTAR_BACNET_CONFIGS` env var. E2E test.
- **Phase B4** — `learn()`, `read_object_list`, `read_object_name`, `BacnetValue::Array`.

## [2.0.0] — 2026-04-10

### Changed
- **Pure Rust VM completion.** Deleted `csrc/` (27 C files, 6,839 lines). `ffi.rs` + `runner.rs` deleted. `build.rs` became a no-op. **Zero C code in the repository.** 88 native methods active. Binary fully static (no `.so` dependencies). Deployed to BeagleBone as 1.8 MB `.deb`.

## [1.x] — 2026-03

Initial Rust migration run (Phase 0–10). Channel model, HAL, REST, WebSocket, SOX/DASP, SCRAM auth, TLS, visual DDC programming, control engine (PID + sequencer + 35 components), SimulatorHal, security hardening, ARM cross-compile. Details in `docs/Progress/ROADMAP_v2.md`.
