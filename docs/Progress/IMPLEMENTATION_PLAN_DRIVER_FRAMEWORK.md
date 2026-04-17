# Implementation Plan: Driver Framework v2 (Phase 12)

**Date:** 2026-04-17 (created)
**Baseline:** v2.8.0 (Modbus + BACnet + MQTT drivers live, poll integration done)
**Reference:** [research/18_SEDONA_DRIVER_FRAMEWORK_V2.md](../research/18_SEDONA_DRIVER_FRAMEWORK_V2.md) — full Haxall-inspired vision

---

## Why this plan exists

The research doc (18) lays out a comprehensive "Driver Framework v2" — Haxall-inspired trait callbacks, actor-based `DriverManager`, `PollScheduler` with buckets, `WatchManager`, status inheritance, typed errors. **Most of that vision is already implemented** in the current workspace:

- `AsyncDriver` trait has `open`/`close`/`ping`/`learn`/`sync_cur`/`write`/`on_watch`/`on_unwatch`
- `spawn_driver_actor` provides the tokio actor with mpsc + oneshot pattern
- `DriverHandle.add_poll_bucket` + `register_point` + `add_watch` work
- `DriverError` enum and `DriverStatus` cascade exist

**What the research doc didn't anticipate:** the glue code in `rest/mod.rs` that each driver type (`load_bacnet_drivers`, `load_mqtt_drivers`) uses. These two functions are ~90% identical (~150 lines of near-duplication). The second was written by copy-paste from the first during MQTT M4.

This plan targets that concrete duplication. It's NOT a rewrite of the framework. Deeper trait refactors are deferred until there's a concrete 4th driver.

---

## Phase breakdown

### 12.0A — Extract driver-loader boilerplate (this session)
**Status:** ✅ COMPLETE (2026-04-17)

**Goal:** Collapse `load_bacnet_drivers` + `load_mqtt_drivers` + their tick-task spawns into a generic helper parameterized by driver type. Pure refactor, zero behavior change.

**Deliverables:**

1. A new `DriverLoader` trait (or similar minimal abstraction) in `rest/mod.rs` or a new `drivers/loader.rs` that captures the shared shape:
   - env var name → JSON config array parse
   - per-config: extract id + point_ids, construct the driver via type-specific factory, register + points + poll bucket
   - `open_all()` after registration
   - tick task spawn: 5s interval, `MissedTickBehavior::Skip`, sync_all → log → write_channel at priority 16

2. A generic function:
   ```rust
   async fn load_drivers<L: DriverLoader>(
       handle: &DriverHandle,
       engine_handle: &EngineHandle,
   );
   ```
   that performs the full flow for the given loader type.

3. Two loader impls:
   - `BacnetLoader` — `ENV_VAR="SANDSTAR_BACNET_CONFIGS"`, config type `BacnetConfig`, factory `BacnetDriver::from_config`, who prefix `"bacnet"`
   - `MqttLoader` — `ENV_VAR="SANDSTAR_MQTT_CONFIGS"`, config type `MqttConfig`, factory `MqttDriver::from_config`, who prefix `"mqtt"`

4. Update `router_with_auth` to call `load_drivers::<BacnetLoader>` and `load_drivers::<MqttLoader>` via `tokio::spawn`.

5. Delete the old `load_bacnet_drivers` and `load_mqtt_drivers` functions.

**Non-goals:**
- No changes to BacnetDriver or MqttDriver themselves
- No trait changes to AsyncDriver
- No new framework concepts (SyncContext, WriteContext, broadcast channels, etc.)

**Success criteria:**
- `cargo test --workspace` — still 2,643 passing
- `cargo clippy -p sandstar-server -- -D warnings` — clean
- `rest/mod.rs` shorter by ~100 lines (from ~200 lines of duplicated loader/tick code down to ~50 lines of loader + one generic helper)
- Deployed v2.8.1 to 1-11, both drivers still work

**Estimate:** 1 session.

---

### 12.0B — Extract shared helpers into `drivers/framework.rs` module (FUTURE)
**Status:** ⬜ DEFERRED

**Trigger:** when we add a 4th driver OR find another source of duplication to collapse.

Candidates (not committed):
- Move `DriverLoader` trait to a proper module
- Shared config-file parsing helper (currently ad-hoc per driver)
- Shared value cache abstraction (`CovCache` and `MqttValueCache` are structurally similar)
- Shared JSON pointer extraction (currently only in MQTT)

---

### 12.0C — Trait abstractions per research doc §"Driver Trait" (FUTURE)
**Status:** ⬜ DEFERRED

**Trigger:** when current trait is proven insufficient by a concrete need.

The research doc proposes `SyncContext` / `WriteContext` wrappers, `DriverMeta` output from `ping`, a richer `on_receive` message API. Our current `AsyncDriver` doesn't have these. Not needed yet.

---

### 12.0D — LocalIoDriver unification (FUTURE)
**Status:** ⬜ DEFERRED

**Trigger:** if we want to unify local I/O under the async driver trait so e.g. `/api/drivers` shows the LocalIo driver alongside BACnet/MQTT.

Current local I/O is handled via the engine's channel layer, not through `AsyncDriver`. They coexist fine. The research doc proposes unifying them. Not needed yet.

---

## Progress log

| Phase | Commit | Date | Version |
|---|---|---|---|
| 12.0A | (pending) | 2026-04-17 | 2.8.1 (target) |
