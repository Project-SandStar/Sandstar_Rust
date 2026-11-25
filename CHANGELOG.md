# Changelog

All notable changes per minor version. Patch-level commits within a version (build fixes, doc tweaks) are rolled up.

Format loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project uses a single workspace version — bumps apply to every crate.

---

## [2.10.0] — 2026-11-25

Hardening release: 73 commits since the `v2.9.1` tag (maintenance steps S-5 to S-52). All of them were deployed to the field device as `2.9.0+<git hash>` builds while they were made; this entry gives them a version number. IDs in parentheses refer to the risk register and step log kept with the project notes.

### Behavior changes (read before upgrading)
- **Deleting a component deletes everything below it.** Children used to stay in the tree with a parent that was gone. The links and tags of every removed component go too. Applies to `/live`, RoWS, `/api/sox` and UDP SOX (R-61, S-55).
- **A failed configuration reload is reported as failed and changes nothing.** `sandstar-cli reload`, `POST /api/reload` and SIGHUP used to report success when the config directory was missing or `tables.csv` / `database.zinc` could not be read, and a bad `tables.csv` left the engine without tables. Now the reply is an error and the tables, channels and poll list stay as they were (R-62, S-55).
- **Links to nothing are refused.** Both components must exist; for a type the manifest knows, the slot must be one of its slots. `/live` and RoWS reorder accepts only the component's current children in a new order (R-60, S-55).
- **HTTP writes need authentication when auth is configured.** Every HTTP write route is behind the auth layer (R-2, S-31). Editor WebSockets (`/live`, `/api/ws`, `/api/rows`) need a bearer token or the session cookie, and browsers must send an allowed `Origin` (R-52, S-32). The SOX tree and `/file/*` reads are behind the same editor gate (R-55, S-33); `PUT /file` accepts the session cookie only with an `Origin` (R-56, S-37). Auth stays off until it is configured per device (`DEPLOYMENT_CHECKLIST.md` §7).
- **The roxWarp cluster transport is fail-closed.** Without mTLS it does not listen or connect (R-54, S-36).
- **Non-finite values are refused.** NaN and ±inf are rejected on writes, bad channel limits are rejected at load, non-finite readings fault the channel, and driver reads and writes follow the same rule (R-9, R-10, R-49; S-25, S-27, S-28). roxWarp does not replicate non-finite values (R-45, S-14).
- **Control loops hold their outputs** when a setpoint or a component input is not `Ok` (R-35, S-29).
- **Dynamic tags are bounded:** Haystack tag names up to 63 bytes, values up to 4 KiB, 2 MiB in total; a batch is applied completely or not at all (R-48, S-46).
- **SOX channel writes go through one bounded queue** (capacity 64, one writer). When it is full a write is dropped and counted (`SOX: channel writes dropped`) (R-40, S-47).
- **BACnet:** a device missing at start is looked for again (30 s, doubling to 10 min); a device that stops answering backs off (30 s, doubling to 5 min) instead of being waited on at every poll; point failures are logged on change, with a reminder every 10 min, not on every tick (R-25; S-49, S-50).
- **MQTT:** the driver reconnects after an error (1 s, doubling to 60 s) and subscribes again; while the broker is not connected the driver is `Down`, reads fail and writes are refused; a config with `tls: true` is refused (TLS is not implemented) (R-25, S-51).
- **SIGTERM runs the graceful shutdown** (`systemctl stop` logs `engine stopped … reason="SIGTERM"`) (R-13, S-43).
- **Package contents:** the C-era scripts (`cutover-to-rust.sh`, `rollback-to-c.sh`, `validate-engines.sh`) and the `sandstar-rust-validate` unit are no longer installed; an upgrade removes the old unit file. `health-monitor.sh` is now shipped (R-22, R-23, S-52).

### Security
- Auth layering: HTTP write routes, editor WebSockets, SOX tree and file reads, late-merged routers inside the global layers (R-2, R-52, R-53, R-55, R-56; S-31 to S-37).
- SOX file transfer: path validation, session ownership of open transfers, chunk bounds (R-15, S-45).
- SMTP and MQTT passwords are masked in logs and API replies; `alerts.json` is written `0600` and atomically (R-14, S-48).
- Malformed network input is rejected in the BACnet, MQTT and Modbus drivers instead of panicking; dropped BACnet frames are logged, rate-limited (R-5 to R-8, S-24).
- roxWarp: peer timestamps are bounded and cluster timer intervals are validated at load (R-27, R-39; S-7, S-15).
- Credential-bearing tool scripts are ignored by git (S-6, S-9).

### Fixed
- `/live`, RoWS and the SOX REST API refuse numbers that do not fit their field instead of wrapping or cutting them: a component id above 65535 no longer reaches another component, an integer outside `i32` or a number too large for `f32` is not stored, a reorder list with an invalid id is rejected, and REST `invoke` answers 400 when the value was not written (R-33, S-54).
- Poisoned locks are recovered instead of aborting the process (REST, SOX, roxWarp, alerts) (R-4; S-16 to S-19).
- Engine calls from REST, WebSocket, driver and SOX paths have a 10 s reply timeout; WS push polls and the SOX refresh no longer block their loops; each WS connection uses one engine slot (R-28; S-20 to S-23).
- IPC connections are served off the main loop with total deadlines (R-28, S-42).
- SOX name intern tables no longer abort or fill up (R-47, S-26).
- Daily log files are pruned by the date in their name; the previous retention setting deleted nothing on the device's file system (R-41, S-7).
- Watchdog: the reason a watchdog device cannot be opened is logged; GPIO60 export is logged and retried in `initialize.sh`, and the engine retries opening it for about 5 minutes (R-42; S-7, S-8).
- Table files fall back to the path given in `tables.csv` (S-7).
- `soak-monitor.sh` and `health-monitor.sh` work on the device as installed (systemd 232, service user) and report real values (R-23, S-52).

### Changed
- MSRV 1.82; toolchain pinned to 1.99.1 in `rust-toolchain.toml` and CI; the stale ARM `--sysroot` flag is gone (R-21, R-38; S-5).
- CI runs on `develop` pushes and on pull requests; `tools/gates.sh` runs the local quality gates and a documentation reminder (S-9, S-11, S-12).
- Driver tick delivery is a separate function, for testing (S-22).
- Documentation brought up to date: research status blocks, overview, roadmap, deployment runbook, BACnet and MQTT setup guides (R-32, R-44; S-10).

### Removed
- Retired scripts moved to `tools/kaldirilanlar/` (kept for reference; not packaged) (S-52).

### Tests
- Tests for IPC command dispatch, the shared SOX operation layer and the `/live` write, link and event flows (R-33, S-54).
- 2,998 tests pass (`cargo test --workspace --no-fail-fast`), 0 clippy warnings.

## [2.9.1] — 2026-04-29

Phase 14.0G fix-pack, tagged `v2.9.1` (`44de90b`). The workspace version in `Cargo.toml` was not raised for this tag.

### Fixed
- `writeFloat` slot ordering on `/live`.
- Adding a link replaces an existing link to the same input.
- Dataflow runs after a mutation made over `/live`.

## [2.9.0] — 2026-04-25

Phase 14.0G: native editor backend, tagged `v2.9.0` (`03b4979`).

### Added
- The external Vue editor is served directly from the Rust binary, replacing the SkySpark + Fantom pod bridge: shared SOX op dispatcher with 5 editor-parity ops, `/live` WebSocket (Fantom dialect), `/live/sedonaState` bulk loader, `/file/*` kit and manifest endpoints.
- Editor integration test suite.

### Fixed
- End-to-end fixes found during production bring-up on the field device.

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
