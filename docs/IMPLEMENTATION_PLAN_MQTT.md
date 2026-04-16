# Implementation Plan: MQTT Driver

**Date:** 2026-04-17
**Scope:** Pure-Rust MQTT v3.1.1 / v5 client driver for Sandstar, following the same patterns proven in the BACnet/IP driver (v2.1.0 – v2.7.0).
**Target:** Sandstar Rust v2.8.0+ on BeagleBone
**Baseline:** v2.7.0 (BACnet fully validated end-to-end)

---

## Why MQTT next

MQTT is the most common "next driver" for IoT buildings. Building it now:

- Stress-tests the driver-framework patterns we proved with BACnet (poll bucket + tick task + `write_channel` at priority 16 + value caching).
- Produces a third concrete driver (Modbus, BACnet, MQTT) before we attempt any Phase 12 "Driver Framework v2" abstraction — three examples is a much better starting point than two.
- Maps real customer demand: MQTT brokers are ubiquitous in IoT, and configuring Sandstar against an existing broker is a common ask.
- Smaller scope than BACnet — no binary framing, no discovery protocol.

## Architecture sketch

```
MQTT broker (mosquitto / cloud)
    ↕ rumqttc client
MqttDriver (event loop task)
    ↓ push via shared value cache
sync_cur() returns cached values (no network read)
    ↓ tick task (from rest/mod.rs, shared with BACnet)
engine_handle.write_channel(point_id, value, level=16, "mqtt:<id>", 30s)
    ↓
GET /api/read?id=N → value appears in Haystack
```

Writes go in the opposite direction: `AsyncDriver::write()` publishes to the configured `publish_topic`.

## Dependency

Add `rumqttc = "0.24"` (the most popular Rust MQTT client crate) to `crates/sandstar-server/Cargo.toml`. Must cross-compile cleanly under our ARMv7 + zig CC wrappers.

## Config shape

```json
[
  {
    "id": "mqtt-local",
    "host": "broker.example.com",
    "port": 1883,
    "client_id": "sandstar-1",
    "username": null,
    "password": null,
    "tls": false,
    "keep_alive_secs": 60,
    "objects": [
      {
        "point_id": 103,
        "subscribe_topic": "bldg/zone1/temp",
        "publish_topic": "bldg/zone1/setpoint",
        "value_path": "/value",
        "qos": 1
      }
    ]
  }
]
```

Env var: `SANDSTAR_MQTT_CONFIGS` — mirrors `SANDSTAR_BACNET_CONFIGS`.

| Field | Type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Unique per driver instance. |
| `host` | string | yes | MQTT broker hostname. |
| `port` | u16 | no | Default `1883` (plain) or `8883` (TLS). |
| `client_id` | string | yes | Unique client ID registered with broker. |
| `username` / `password` | string? | no | Plain auth. |
| `tls` | bool | no | Default `false`. |
| `keep_alive_secs` | u16 | no | Default `60`. |
| `objects[].point_id` | u32 | yes | Sandstar channel ID; must be a VirtualAnalog in `database.zinc`. |
| `objects[].subscribe_topic` | string? | no | Topic to subscribe for reads. |
| `objects[].publish_topic` | string? | no | Topic to publish for writes. |
| `objects[].value_path` | string? | no | RFC 6901 JSON Pointer into message payload. `None` = payload is a plain number. |
| `objects[].qos` | u8 | no | 0 or 1. Default `1`. QoS 2 is out of scope. |

## Phase breakdown

### M1 — Client + lifecycle
**Status:** ✅ COMPLETE (2026-04-17)

Replace the `MqttDriver` stub in `crates/sandstar-server/src/drivers/mqtt.rs`:

- Add `rumqttc` dependency and `MqttConfig` / `MqttObjectConfig` structs.
- `MqttDriver` fields: config, `rumqttc::AsyncClient`, `rumqttc::EventLoop`, event-loop `JoinHandle`, value cache.
- `open()` — build the rumqttc client, spawn a tokio task that drives the event loop, await `Incoming::ConnAck`, subscribe to every configured `subscribe_topic`.
- `close()` — disconnect the client, `abort()` the event loop task.
- `ping()` — return Ok if the task is alive.
- `learn()` — return one `LearnPoint` per configured object.

Unit tests: driver lifecycle with a mock event loop (no real broker).

### M2 — Value cache + `sync_cur`
**Status:** ✅ COMPLETE (2026-04-17)

- `MqttValueCache` (similar shape to `CovCache`): `HashMap<String (topic), CacheEntry { value, updated_at }>`.
- Event loop task pushes incoming messages into the cache:
  - If `value_path` is `Some`, parse payload as JSON and extract via `serde_json::Value::pointer()`.
  - Else parse as plain f64.
- `sync_cur()` looks up each point's `subscribe_topic` in the cache and returns `Ok(value)` if fresh (< 600s old), else `Err(CommFault("stale"))`.

Tests: cache population, stale expiry, JSON path extraction, plain-number parsing.

### M3 — `write()` + engine integration
**Status:** ⬜ NOT STARTED

- `AsyncDriver::write()` — for each `(point_id, value)`:
  - Look up `publish_topic`; if `None`, return `ConfigFault("no publish_topic")`.
  - Format payload:
    - If `value_path` is `None`, publish `value.to_string()`.
    - Else publish `{"value": <value>}` as JSON.
  - `client.publish(topic, qos, retain=false, payload).await`.
- Reuse the shared tick task from `rest/mod.rs` (already proven for BACnet) — sync_cur results flow via `engine.write_channel`.

Tests: publish payload format, QoS, missing publish_topic handling.

### M4 — Server wiring + E2E + docs
**Status:** ⬜ NOT STARTED

- `load_mqtt_drivers(&DriverHandle, &EngineHandle)` in `rest/mod.rs`, mirroring `load_bacnet_drivers`.
  - Reads `SANDSTAR_MQTT_CONFIGS` env.
  - Registers driver, registers points, adds poll bucket, spawns tick task, calls `open_all()`.
- E2E test using an embedded test broker (rumqttc's test utilities, or `rumqttd` embedded).
- `MQTT_SETUP.md` operator guide — structure mirrors `BACNET_SETUP.md`.
- Deploy to 1-11 and verify against a reachable broker (e.g. local mosquitto in Docker, or test.mosquitto.org).

Tests: full cycle (publish to broker → Sandstar receives → /read returns the value).

## Testing strategy

**Per-phase unit tests:** in `crates/sandstar-server/src/drivers/mqtt.rs` following the pattern from `bacnet/mod.rs`.

**E2E tests:** use an embedded MQTT broker so tests don't depend on external network. Candidates:
- [`rumqttd`](https://docs.rs/rumqttd) — Rust MQTT broker, embeddable in tests
- [`mqttest`](https://crates.io/crates/mqttest) — minimal test broker

**Integration against real broker:** deploy to 1-11 and point at either:
- Local mosquitto in Docker on the Windows dev box
- `test.mosquitto.org:1883` (public test broker, non-authenticated)

## Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `rumqttc` doesn't cross-compile under Zig CC wrappers | Medium | High | Check early in M1. Fallback: `paho-mqtt` or hand-rolled v3.1.1 client. |
| Keep-alive ping failures under flaky network | Medium | Medium | rumqttc handles reconnect automatically — just log and continue. |
| Event-loop task panics silently | Medium | Medium | Wrap in `tokio::spawn` that logs on panic; `ping()` returns Err if task died. |
| JSON payload parsing edge cases | Low | Low | Test with nested/missing/non-numeric paths; treat as `CommFault`. |
| Broker auth mis-config results in silent disconnect | Medium | Low | Log `ConnAck::BadUserNameOrPassword` clearly; surface via driver status. |

## Completion criteria

MQTT driver is "done" when:
- `SANDSTAR_MQTT_CONFIGS` env var is read at startup
- Configured broker is connected, topics subscribed
- Incoming messages update cached values
- `sync_cur` returns fresh values, stale-check honored
- `AsyncDriver::write()` publishes to broker
- `/api/read?id=<point_id>` returns current broker-reported value
- All tests pass, clippy clean
- Deployed to 1-11 and validated against a live broker
- `MQTT_SETUP.md` documents the full setup + troubleshooting

## Progress log

| Phase | Commit | Date | Version |
|---|---|---|---|
| M1 | (pending commit) | 2026-04-17 | 2.8.0-dev |
| M2 | (pending commit) | 2026-04-17 | 2.8.0-dev |
| M3 | — | — | — |
| M4 | — | — | — |
