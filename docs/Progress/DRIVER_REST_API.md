# Driver REST API Guide

> **Status (2026-11-18)**: verified against `master` `b48484b`. Routes, the public/protected split, body shapes, status codes (201/400/404/409/501/503) and the Prometheus metric names match the code. Corrections:
> - **Auth options:** `--auth-token` / `SANDSTAR_AUTH_TOKEN` (bearer) or SCRAM-SHA-256 via `--auth-user` / `--auth-pass` (`SANDSTAR_AUTH_USER` / `SANDSTAR_AUTH_PASS`). With neither configured, the auth gate lets every request through. The packaged service binds `0.0.0.0` with no auth, so on the field device the "auth-gated" routes below are open.
> - Other write surfaces (`/api/sox/*`, `/live`, `/file/*`, `/api/tags*`, `/api/alerts/*`, `/api/cluster/query`) stay unauthenticated even when auth is configured.
> - **REST-created drivers** (`POST /api/drivers`) get no poll tick task, so their values never reach engine channels, and they are lost on restart. Only drivers loaded from the environment at startup are polled.
> - Rate limiting is on by default (100 req/s, `--rate-limit` / `SANDSTAR_RATE_LIMIT`; 0 disables).
> - The sample build info in §7 shows 2.8.10; current builds report `2.9.0` with the git hash.
> - Device addresses in the examples were replaced with `<device-ip>` (2026-11-18).

Sandstar v2.8.5+ exposes runtime driver management through a REST surface. You can **list, create, open, close, ping, delete, write, batch-read, and send custom messages** to any registered driver (BACnet, MQTT, LocalIoDriver, or future types) without restarting the service.

This guide covers:

1. [Overview and auth](#1-overview-and-auth)
2. [List + status endpoints (public read)](#2-list--status-endpoints-public-read)
3. [Lifecycle endpoints (auth-gated)](#3-lifecycle-endpoints-auth-gated)
4. [I/O endpoints (auth-gated)](#4-io-endpoints-auth-gated)
5. [Custom messages (`on_receive`)](#5-custom-messages-on_receive)
6. [Real-time push over WebSocket (`/api/ws`)](#6-real-time-push-over-websocket-apiws)
7. [Prometheus metrics (`GET /metrics`)](#7-prometheus-metrics-get-metrics)
8. [Error codes](#8-error-codes)
9. [End-to-end recipes](#9-end-to-end-recipes)
10. [Troubleshooting](#10-troubleshooting)

---

## 1. Overview and auth

All endpoints below are mounted under the main HTTP listener (port **8085** by default). The base URL in the examples is `http://<device-ip>:8085`.

### Auth model

| Endpoint type | Auth required? |
|---|---|
| `GET /api/drivers`, `GET /api/drivers/{id}/status`, `GET /api/drivers/{id}/learn` | **No** (public read) |
| All `POST` and `DELETE` endpoints below | **Yes, when a bearer token or SCRAM auth is configured** (see below) |

When the server is started without any auth configuration (dev mode), even the mutating endpoints are open. In production the `--auth-token <token>` flag (legacy) or a SCRAM store enables auth. Authenticated requests include one of:

```http
Authorization: Bearer <legacy-token-or-scram-session-token>
```

Return codes for auth:
- `401 Unauthorized` — no/invalid Authorization header and auth is required
- Per-endpoint codes otherwise

### What drivers are there?

Every running Sandstar registers a `localIo` driver automatically (the engine-channel façade added in Phase 12.0F). BACnet and MQTT drivers appear when their respective env vars are set at startup (`SANDSTAR_BACNET_CONFIGS`, `SANDSTAR_MQTT_CONFIGS` — see [BACNET_SETUP.md](BACNET_SETUP.md) + [MQTT_SETUP.md](MQTT_SETUP.md)). Additional drivers can be created at runtime via `POST /api/drivers` (§3).

---

## 2. List + status endpoints (public read)

### `GET /api/drivers`

List every registered driver with its status and poll statistics.

```bash
curl -s http://<device-ip>:8085/api/drivers | python3 -m json.tool
```

Response:
```json
[
  {
    "id": "bacnet-local",
    "driverType": "bacnet",
    "status": "Ok",
    "pollMode": "Buckets",
    "pollBuckets": 1,
    "pollPoints": 1
  },
  {
    "id": "localIo",
    "driverType": "localIo",
    "status": "Ok",
    "pollMode": "Buckets",
    "pollBuckets": 0,
    "pollPoints": 0
  }
]
```

### `GET /api/drivers/{id}/status`

Driver-level status plus per-point status inheritance (Phase 12.0B).

```bash
curl -s http://<device-ip>:8085/api/drivers/bacnet-local/status | python3 -m json.tool
```

Response:
```json
{
  "id": "bacnet-local",
  "status": "Ok",
  "points": [
    {"pointId": 102, "status": "Ok"}
  ]
}
```

When the remote device is down, a point's status will be `"Down"` (RemoteDown) or `"Disabled"` (RemoteDisabled) or `{"Fault": "<reason>"}` (RemoteFault) — see research doc 18 for the full `PointStatus` semantic.

### `GET /api/drivers/{id}/learn`

Ask the driver to discover its points. Returns a Haystack-style grid. Drivers that don't implement learn return `501 Not Implemented`.

```bash
curl -s http://<device-ip>:8085/api/drivers/bacnet-local/learn | python3 -m json.tool
```

---

## 3. Lifecycle endpoints (auth-gated)

### `POST /api/drivers` — create a driver

Body: `{"driver_type": "bacnet"|"mqtt", "config": { ... }}`. The `config` must match the corresponding driver's JSON schema (see [BACNET_SETUP.md §2](BACNET_SETUP.md) / [MQTT_SETUP.md §2](MQTT_SETUP.md)).

The driver is registered but **not auto-opened**. Call `POST /api/drivers/{id}/open` to bring it up.

```bash
curl -s -X POST http://<device-ip>:8085/api/drivers \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $TOKEN" \
  -d '{
    "driver_type": "bacnet",
    "config": {
      "id": "bac-shop",
      "broadcast": "192.168.1.255",
      "port": 47808,
      "objects": [{"point_id": 9001, "device_id": 77, "object_type": 0, "instance": 0}]
    }
  }'
```

Response: `{"ok": true, "id": "bac-shop"}` (HTTP 201) or `{"ok": false, "error": "..."}` (409 on duplicate id, 400 on bad config).

### `POST /api/drivers/{id}/open`

Bring the driver up. For network drivers this runs their discovery/handshake (e.g. BACnet Who-Is, MQTT CONNECT). Returns driver meta:

```bash
curl -s -X POST http://<device-ip>:8085/api/drivers/bacnet-local/open \
  -H "Authorization: Bearer $TOKEN"
```

Response:
```json
{
  "ok": true,
  "id": "bacnet-local",
  "model": "BACnet/IP port=47808 (1 device)",
  "firmwareVersion": null,
  "extra": {}
}
```

**Common use:** re-discover peers after starting them *after* the Sandstar service (common with a Windows-side BACnet simulator). Instead of `systemctl restart sandstar-engine`, run `close` then `open`.

### `POST /api/drivers/{id}/close`

Shut the driver down without removing it from the registry. The driver keeps its config, so `open` can bring it back.

```bash
curl -s -X POST http://<device-ip>:8085/api/drivers/bacnet-local/close \
  -H "Authorization: Bearer $TOKEN"
# {"id": "bacnet-local", "ok": true}
```

### `POST /api/drivers/{id}/ping`

Health-check the driver. Returns the same meta shape as `open`. `503 Service Unavailable` on comm fault, `404` if driver id unknown.

```bash
curl -s -X POST http://<device-ip>:8085/api/drivers/localIo/ping \
  -H "Authorization: Bearer $TOKEN"
# {"ok": true, "id": "localIo", "model": "BeagleBone Black", ...}
```

### `DELETE /api/drivers/{id}`

Close and deregister. The driver is gone until next startup or another `POST /api/drivers`.

```bash
curl -s -X DELETE http://<device-ip>:8085/api/drivers/bac-shop \
  -H "Authorization: Bearer $TOKEN"
# {"ok": true, "id": "bac-shop"}
```

---

## 4. I/O endpoints (auth-gated)

### `POST /api/drivers/{id}/write`

Write one or more point values through the driver. For BACnet this emits WriteProperty at priority 16; for MQTT it publishes to the configured topic; for localIo it calls `engine.write_channel` at priority 16.

Body: `{"writes": [[pointId, value], ...]}` (array of `[u32, f64]` pairs).

```bash
curl -s -X POST http://<device-ip>:8085/api/drivers/bacnet-local/write \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"writes": [[102, 72.5]]}'
```

Response:
```json
{
  "driverId": "bacnet-local",
  "results": [{"pointId": 102, "ok": true}]
}
```

Failed writes carry a per-point error message:
```json
{"pointId": 999, "ok": false, "error": "no BACnet object configured for point 999"}
```

### `POST /api/syncCur` — batch read across drivers

Batch-read current values for multiple drivers in one request. Body: `{"driverPoints": {"<driver-id>": [{"pointId": N, "address": "..."}, ...]}}`.

`address` must match the driver's internal addressing (e.g. BACnet uses the text the driver tagged on its `DriverPointRef`, not that it matters for most — the driver looks up `point_id` internally).

```bash
curl -s -X POST http://<device-ip>:8085/api/syncCur \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $TOKEN" \
  -d '{
    "driverPoints": {
      "localIo": [{"pointId": 1713, "address": "AIN6"}],
      "bacnet-local": [{"pointId": 102, "address": "AI:0"}]
    }
  }'
```

Response:
```json
{
  "results": [
    {"driverId": "localIo", "pointId": 1713, "value": 121.12},
    {"driverId": "bacnet-local", "pointId": 102, "value": 72.5}
  ]
}
```

Per-point failures come back as `{"driverId": ..., "pointId": N, "error": "..."}` entries in the same `results` array.

---

## 5. Custom messages (`on_receive`)

### `POST /api/drivers/{id}/message`

Dispatch a driver-specific custom message (Phase 12.0E infrastructure). Body: `{"id": "<message-type>", "payload": {...}}`.

The default trait implementation returns `501 Not Implemented` — drivers opt in to specific message ids. No shipping driver currently implements custom messages; this is infrastructure for future driver-specific commands (e.g. force BACnet Who-Is, request MQTT reconnect, export driver stats).

```bash
curl -s -X POST http://<device-ip>:8085/api/drivers/bacnet-local/message \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"id": "whoIs", "payload": {}}'
# {"error": "not supported: on_receive"}  ← expected, 501
```

---

## 6. Real-time push over WebSocket (`/api/ws`)

Sandstar v2.8.7+ routes driver `CovEvent`s (change-of-value broadcasts) into Haystack WebSocket sessions (Phase 12.0D.WS).

**Client-requested** `pollInterval` is still honored as an upper bound on push latency when values are static. When values are actually changing, matching watches are force-polled on the next push-timer tick (**~200 ms bound**). A client can request a 10-second polling cadence and still see updates at the underlying driver tick rate (typically 5 s for BACnet/MQTT) whenever values change.

### Quick wire test

`tools/ws_latency_test.py` exercises this end-to-end against a running device:

```bash
py tools/ws_latency_test.py <device-ip>:8085 102 45
# connects, subscribes with pollInterval=10000, records updates for 45 s,
# prints gap statistics and declares bridge LIVE when median gap << client interval.
```

Expected against `bacnet_sim.py --vary`:
- **8–9 updates in 45 s** (~5 s cadence = BACnet tick rate)
- If the bridge were *not* live, you'd see ~4 updates (one per 10-second poll)

---

## 7. Prometheus metrics (`GET /metrics`)

Sandstar v2.8.10+ exposes internal counters and gauges in the Prometheus text exposition format (version 0.0.4) at `GET /metrics` — **no auth** (intended for internal scrape; gate with a reverse proxy if external exposure is a concern).

```bash
curl -s http://<device-ip>:8085/metrics | head -30
```

Sample output:

```
# HELP sandstar_build_info Build metadata (version + git hash) as labels; value is always 1.
# TYPE sandstar_build_info gauge
sandstar_build_info{version="2.8.10",git_hash="abc1234"} 1
# HELP sandstar_poll_count Total number of poll-cycle iterations the engine has executed.
# TYPE sandstar_poll_count counter
sandstar_poll_count 4823
...
# HELP sandstar_driver_cov_events_total Phase 12.0D: total CovEvents broadcast from the driver actor.
# TYPE sandstar_driver_cov_events_total counter
sandstar_driver_cov_events_total 1217
```

### Exposed metrics

Counters (monotonic):
- `sandstar_poll_count`, `sandstar_poll_overrun_count`
- `sandstar_hal_errors_total`
- `sandstar_rest_requests_total`, `sandstar_ipc_requests_total`
- `sandstar_history_points_total`
- `sandstar_ws_total`, `sandstar_ws_messages_in_total`, `sandstar_ws_messages_out_total`
- `sandstar_rows_total`, `sandstar_rows_messages_in_total`, `sandstar_rows_messages_out_total`
- `sandstar_driver_sync_total` (Phase 12.0D sync-cycle count)
- `sandstar_driver_sync_errors_total` (Phase 12.0B per-point errors)
- `sandstar_driver_cov_events_total` (Phase 12.0D change-of-value broadcasts)

Gauges (instantaneous):
- `sandstar_poll_duration_microseconds_last`, `sandstar_poll_duration_microseconds_max`
- `sandstar_watch_active`
- `sandstar_ws_active`, `sandstar_rows_active`
- `sandstar_build_info{version,git_hash}` — value always `1`, labels identify the build

### Minimal Prometheus scrape config

```yaml
scrape_configs:
  - job_name: sandstar
    scrape_interval: 30s
    static_configs:
      - targets: ["<device-ip>:8085"]
```

### Legacy JSON format

The older `GET /api/metrics` endpoint continues to return the same counters in JSON form (camelCase keys) — useful for UI dashboards that don't speak the Prometheus format.

---

## 8. Error codes

| Status | Meaning |
|---|---|
| `200 OK` / `201 Created` | Success |
| `400 Bad Request` | Malformed body, unknown `driver_type`, missing required field |
| `401 Unauthorized` | Auth required but missing/invalid `Authorization: Bearer ...` |
| `404 Not Found` | Driver id doesn't exist |
| `409 Conflict` | Duplicate driver id on `POST /api/drivers` |
| `500 Internal Server Error` | Generic driver / actor error with message |
| `501 Not Implemented` | `learn` or `on_receive` called on a driver that doesn't support it |
| `503 Service Unavailable` | `ping` failed with a comm fault — remote isn't reachable |

Error responses are always `{"error": "<message>"}` with a human-readable reason.

---

## 9. End-to-end recipes

### Recreate a BACnet driver at runtime

```bash
# remove the current one
curl -X DELETE http://<device-ip>:8085/api/drivers/bacnet-local \
  -H "Authorization: Bearer $TOKEN"

# create a new config
curl -X POST http://<device-ip>:8085/api/drivers \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $TOKEN" \
  -d @new-bacnet-config.json

# bring it up
curl -X POST http://<device-ip>:8085/api/drivers/bacnet-local/open \
  -H "Authorization: Bearer $TOKEN"
```

### Recover from "sim started after Sandstar" (common dev flow)

When the BACnet/MQTT simulator on your Windows dev box was not running at Sandstar startup, its driver discovers zero peers. Re-discovery **without** a full service restart:

```bash
curl -X POST http://<device-ip>:8085/api/drivers/bacnet-local/close \
  -H "Authorization: Bearer $TOKEN"
# brief pause so the socket releases
sleep 1
curl -X POST http://<device-ip>:8085/api/drivers/bacnet-local/open \
  -H "Authorization: Bearer $TOKEN"
# expect: "model": "BACnet/IP port=47808 (N devices)"
```

### Drive an HVAC actuator via localIo

```bash
# local digital output channel 5000
curl -X POST http://<device-ip>:8085/api/drivers/localIo/write \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"writes": [[5000, 1.0]]}'
```

The write goes to `engine.write_channel(5000, Some(1.0), level=16, who="localIo:localIo", duration=30s)` — low-priority background write. Higher-priority SOX or manual `/api/pointWrite` calls will override.

---

## 10. Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `POST /api/drivers/{id}/open` returns `Address already in use (os error 98)` | The driver is currently registered and its socket is bound. `open` tries to re-bind the same port. | `close` first, then `open`. See §9. |
| `POST /api/drivers` returns `409 Conflict` | A driver with that id is already registered. | `DELETE` first, or pick a new id. |
| `POST /api/drivers/{id}/message` always returns `501` | The driver doesn't implement `on_receive` for that message id. This is expected for all shipping drivers today — the infrastructure is there, opt-in per driver. | Wait for a driver to add support, or implement custom messages in your own out-of-tree driver. |
| Long-pollInterval WS client misses value changes | The server doesn't have a `driver_handle` in its `WsState` (shouldn't happen in production; occurs in some test harnesses). | Check that `rest::mod.rs` passes `Some(driver_handle.clone())` to `WsState`. |
| `POST /api/syncCur` returns empty `results` despite requesting valid points | The driver id doesn't match a registered driver — the actor silently skips unknown ids. | `GET /api/drivers` first to confirm the id, check spelling. |
| Drivers show status `Ok` but all points `Down` | COV cache miss + remote unreachable. In Phase B8.1, the BACnet driver caches COV values for up to 600 s — stale cache still reads as Ok; the individual point reports `Down` when its underlying read fails. | Start the remote / fix network; status propagates to `Ok` on next successful sync. |

For deeper protocol debugging, see [IMPLEMENTATION_PLAN_DRIVER_FRAMEWORK.md](IMPLEMENTATION_PLAN_DRIVER_FRAMEWORK.md) for the Phase 12 design and per-phase commit log.
