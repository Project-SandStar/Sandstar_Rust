# Implementation Plan: Driver Framework v2 (Phase 12)

**Date:** 2026-04-17 (created), 2026-04-17 (expanded with 12.0B–G, 12.0C shipped)
**Baseline:** v2.8.3 (12.0A + 12.0B + 12.0C shipped)
**Reference:** [research/18_SEDONA_DRIVER_FRAMEWORK_V2.md](../research/18_SEDONA_DRIVER_FRAMEWORK_V2.md) — full Haxall-inspired vision

---

## Why this plan exists

The research doc (18) proposes a comprehensive "Driver Framework v2" — Haxall-inspired trait callbacks, actor-based `DriverManager`, `PollScheduler` with buckets, `WatchManager`, status inheritance, typed errors, custom messages, broadcast COV. **~70% of that vision was already implemented** before this plan started:

- ✅ `AsyncDriver` trait: `open`/`close`/`ping`/`learn`/`sync_cur`/`write`/`on_watch`/`on_unwatch`
- ✅ `spawn_driver_actor` tokio actor with mpsc + oneshot
- ✅ `DriverHandle.add_poll_bucket` + `register_point` + `add_watch`
- ✅ `DriverError` + `DriverStatus` enums
- ✅ `PointStatus` enum (simpler shape than research doc; to be enriched in 12.0B)
- ✅ HAL crates (gpio-cdev, i2cdev, industrial-io) via `sandstar-hal-linux`
- ✅ Sedona VM native method bridge (v2.0.0)

What remains is closing the gaps phase by phase. Each phase lands independently with tests + doc updates + deploy to 1-11.

---

## Phase breakdown

### 12.0A — Extract driver-loader boilerplate
**Status:** ✅ COMPLETE (2026-04-17, `763bdea` → 2.8.1)

Generic `DriverLoader` trait + `load_drivers<L>` helper collapses ~417 lines of near-duplication between `load_bacnet_drivers` and `load_mqtt_drivers`. See § Progress log.

---

### 12.0B — PointStatus enrichment + Remote* variants
**Status:** ✅ COMPLETE (2026-04-17, v2.8.2)

**Goal:** match the research doc's `PointStatus` semantic so per-point remote errors are distinguishable from driver-wide errors. Drives better `/api/drivers/{id}/status` diagnostics.

**Deliverables:**

1. Extend `PointStatus` enum in `drivers/mod.rs`:
   ```rust
   pub enum PointStatus {
       Inherited,                     // existing — inherits driver status
       Own(DriverStatus),             // existing — explicit override
       RemoteDisabled,                // NEW — remote says this point disabled
       RemoteDown,                    // NEW — remote says this point down
       RemoteFault(String),           // NEW — remote-reported fault with reason
   }
   ```

2. Update `PointStatus::resolve(&driver_status)` — Remote* variants are terminal (don't fall back to driver).

3. Add `PointStatus::from_driver_error(err: &DriverError) -> Self`:
   - `DriverError::RemoteStatus(_)` → `RemoteFault(msg)`
   - `DriverError::CommFault(_)` → `RemoteDown`
   - `DriverError::ConfigFault(_)` → `Own(DriverStatus::Fault(msg))`
   - other → `Own(DriverStatus::Fault(msg))`

4. Wire into actor's error path: when `sync_cur` returns `(point_id, Err(driver_err))`, the manager should call `set_point_status(point_id, PointStatus::from_driver_error(&err))`.

5. Tests: round-trip serialization for new variants; `resolve()` behavior; `from_driver_error` mapping.

**Non-goals:** no REST API changes; no breaking changes to existing `Own`/`Inherited` variants.

**Estimate:** 1 session. Small blast radius (6 non-test callsites for `PointStatus`).

---

### 12.0C — `SyncContext` / `WriteContext` callback refactor
**Status:** ✅ COMPLETE (2026-04-17, v2.8.3)

**Goal:** replace the direct-return model (`sync_cur` returns `Vec<(u32, Result<f64>)>`) with the research doc's callback model where drivers call `ctx.update_cur_ok(id, value)` / `ctx.update_cur_err(id, err)`. Purely cosmetic — no behavioral change.

**Deliverables:**

1. New types in `drivers/mod.rs` or `async_driver.rs`:
   ```rust
   pub struct SyncContext {
       results: Vec<(u32, Result<f64, DriverError>)>,
   }
   impl SyncContext {
       pub fn update_cur_ok(&mut self, point_id: u32, value: f64);
       pub fn update_cur_err(&mut self, point_id: u32, err: DriverError);
   }

   pub struct WriteContext { /* same shape but Result<(), _> */ }
   ```

2. Update `AsyncDriver` trait:
   ```rust
   async fn sync_cur(&mut self, points: &[DriverPointRef], ctx: &mut SyncContext);
   async fn write(&mut self, writes: &[(u32, f64)], ctx: &mut WriteContext);
   ```

3. Update all 3 driver impls (local_io, bacnet, modbus, mqtt) + the shared helpers.

4. Update the actor to construct a fresh context per call, drain results into the return-tuple format for backward compatibility with `handle.sync_all` callers.

**Estimate:** 1 session. Touches every driver but mechanically.

**Risk:** API change — any future out-of-tree driver impl will need to migrate.

---

### 12.0D — Broadcast `CovEvent` channel
**Status:** ⬜ NOT STARTED

**Goal:** add a `tokio::sync::broadcast::Sender<CovEvent>` to `DriverManager` so any consumer (REST WebSocket, SOX COV push, metrics) can subscribe to live value changes without polling.

**Deliverables:**

1. New type:
   ```rust
   pub struct CovEvent {
       pub point_id: u32,
       pub value: f64,
       pub status: PointStatus,
       pub timestamp: std::time::Instant,
   }
   ```

2. Add `cov_tx: broadcast::Sender<CovEvent>` field to `DriverManagerInner`. Initialize in `spawn_driver_actor`.

3. Emit CovEvent when `sync_cur` returns `Ok(value)` AND the value differs from the last emitted one for that point (change-of-value semantics).

4. Expose subscriber via `DriverHandle::subscribe_cov() -> broadcast::Receiver<CovEvent>`.

5. Wire into `/api/ws` (Haystack WebSocket) — when a watch is subscribed, bridge CovEvents to the WebSocket client.

**Estimate:** 1 session. Structural but contained.

**Value:** enables real-time UI push without the current polling model.

---

### 12.0E — Custom driver messages (`on_receive`)
**Status:** ⬜ DEFERRED

**Rationale:** the research doc's `on_receive(DriverMessage) -> Result<DriverMessage, DriverError>` is infrastructure with no current caller. Don't build it until a concrete need appears (e.g., a driver-specific command from REST or SOX).

---

### 12.0F — LocalIoDriver unification
**Status:** ⬜ DEFERRED (risk-based)

**Rationale:** unifying the local hardware I/O (GPIO / ADC / I2C / PWM) under the `AsyncDriver` trait would rewrite the path that Device 1-3's production HVAC uses. The existing engine-channel layer works; replacing it risks regressing live sensors. Defer until:
- A concrete 4th network driver needs to coexist with local I/O under `/api/drivers`
- OR we have non-production hardware to validate against

---

### 12.0G — Extended REST endpoints
**Status:** ⬜ NOT STARTED

**Goal:** make drivers manageable at runtime via REST — add/remove/open/close/ping without restarting the service. Matches the research doc's REST API section.

**Deliverables:**

1. `POST /api/drivers` — create driver from `{driver_type, config}` JSON body. Mirrors `DriverLoader::build_driver` but dispatches by `driver_type` string.

2. `POST /api/drivers/{id}/open` — call `handle.open_driver(id)`.

3. `POST /api/drivers/{id}/close` — call `handle.close_driver(id)`.

4. `POST /api/drivers/{id}/ping` — call `handle.ping_driver(id)` and return `DriverMeta` JSON.

5. `DELETE /api/drivers/{id}` — call `handle.remove(id)`.

6. `POST /api/syncCur` — batch sync with `{driver_points: HashMap<DriverId, Vec<point_id>>}` body.

7. Auth: these mutations are gated behind the existing bearer-token auth layer.

**Estimate:** 1 session.

---

## Phase sequencing

Recommended order (small + valuable first):

1. **12.0B** — PointStatus enrichment (this session)
2. **12.0C** — SyncContext refactor
3. **12.0D** — Broadcast CovEvent
4. **12.0G** — Extended REST endpoints
5. **12.0E** — (only if triggered by a concrete use case)
6. **12.0F** — (only when risk is justified by demand)

Total: ~4 sessions for 12.0B + 12.0C + 12.0D + 12.0G.

---

## Progress log

| Phase | Commit | Date | Version |
|---|---|---|---|
| 12.0A | (Phase 12.0A commit) | 2026-04-17 | 2.8.1 |
| 12.0B | (Phase 12.0B commit) | 2026-04-17 | 2.8.2 |
| 12.0C | (pending commit)     | 2026-04-17 | 2.8.3 |

**12.0C summary (2026-04-17, v2.8.3):** Added `SyncContext` and `WriteContext` types to `drivers/mod.rs` with `update_cur_ok/err` and `update_write_ok/err` methods. Changed `Driver` and `AsyncDriver` trait `sync_cur`/`write` signatures to take `&mut SyncContext`/`&mut WriteContext` instead of returning a `Vec`. `AnyDriver::sync_cur` and `AnyDriver::write` (the call points from the actor and the REST layer) still return the `Vec<(u32, Result<_>)>` shape — they construct a fresh context per call and drain it — so callers are untouched. Four drivers migrated: `LocalIoDriver`, `ModbusDriver`, `BacnetDriver`, `MqttDriver`. All mocks in tests updated. Two `#[cfg(test)]` inherent helpers (`sync_cur_vec` / `write_vec`) added on `BacnetDriver` and `MqttDriver` to keep existing test assertions concise. Net diff: +5 unit tests covering ctx behavior (insertion order, Ok/Err capture, `with_capacity`). 2660 tests pass, 0 clippy warnings in the drivers module.
