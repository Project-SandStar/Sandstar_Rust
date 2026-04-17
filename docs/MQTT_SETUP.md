# MQTT Setup Guide

Sandstar v2.8.0+ supports MQTT v3.1.1 as a network driver: subscribes to broker topics to receive live values, publishes to broker topics on writes, caches incoming values for fast `sync_cur()`, and integrates with the shared driver-framework tick task used by BACnet.

This guide covers:
1. Enabling the driver
2. Configuring points via `SANDSTAR_MQTT_CONFIGS`
3. Firewall / network requirements
4. Live values (`/api/read`)
5. Verification
6. Troubleshooting

---

## 1. Enabling the driver

The MQTT driver is compiled into every Sandstar build. It is **quiescent** until you set the `SANDSTAR_MQTT_CONFIGS` environment variable via a systemd drop-in.

### Create the env file

```bash
sudo mkdir -p /etc/sandstar
sudo tee /etc/sandstar/mqtt.env > /dev/null <<'EOF'
SANDSTAR_MQTT_CONFIGS=[{"id":"mqtt-local","host":"broker","port":1883,"client_id":"sandstar-1","objects":[]}]
EOF
sudo chmod 644 /etc/sandstar/mqtt.env
```

Replace `broker` with your MQTT broker's hostname or IP. For testing against the public Mosquitto test broker, use `test.mosquitto.org`.

### Create the systemd drop-in

```bash
sudo mkdir -p /etc/systemd/system/sandstar-engine.service.d
sudo tee /etc/systemd/system/sandstar-engine.service.d/mqtt.conf > /dev/null <<'EOF'
[Service]
EnvironmentFile=-/etc/sandstar/mqtt.env
EOF
sudo systemctl daemon-reload
sudo systemctl restart sandstar-engine.service
```

The `-` prefix on `EnvironmentFile` makes it optional — if the file disappears the service still starts.

---

## 2. Configuring points

Each entry in `SANDSTAR_MQTT_CONFIGS` is a JSON object describing one MQTT driver instance. Most deployments need only one.

### Schema

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

### Driver fields

| Field | Type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Unique per driver instance; shown in `/api/drivers`. |
| `host` | string | yes | MQTT broker hostname or IP. |
| `port` | u16 | no | Default `1883` (plain), use `8883` for TLS. |
| `client_id` | string | yes | Must be unique to the broker (duplicates get disconnected). |
| `username` / `password` | string? | no | Plain auth. Both must be set to take effect. |
| `tls` | bool | no | Reserved — not yet wired; leave `false`. |
| `keep_alive_secs` | u16 | no | Default `60`. Broker pings at ~half this interval. |
| `objects` | array | yes | Points to bind. May be empty (driver still connects). |

### `objects[]` fields

| Field | Type | Required | Notes |
|---|---|---|---|
| `point_id` | u32 | yes | Sandstar channel ID. Must match a VirtualAnalog in `database.zinc`. |
| `subscribe_topic` | string? | no | Topic to subscribe to for reads. If `None`, `sync_cur` returns `ConfigFault`. |
| `publish_topic` | string? | no | Topic to publish to on writes. If `None`, `write()` returns `ConfigFault`. |
| `value_path` | string? | no | RFC 6901 JSON Pointer into the payload. `None` = payload is a plain number. |
| `qos` | u8 | no | `0` or `1`. Default `1`. QoS 2 is out of scope (falls back to 1). |

### Payload shape

- **`value_path` is `None`** — payload is parsed directly as an `f64`. For example: `"42.5"`, `"72"`, `"0"`. Whitespace is trimmed.
- **`value_path` is `Some(pointer)`** — payload is parsed as JSON, and the pointer is resolved via RFC 6901. Number / integer / boolean / numeric-string values are all coerced to `f64`.
  - Example: `value_path = "/value"` matches payload `{"value": 42.5}`.
  - Example: `value_path = "/data/reading"` matches `{"data":{"reading":42.5}}`.

For **writes**: if `value_path` is set (any read-side JSON path), Sandstar publishes a flat `{"value": <N>}` envelope. If `value_path` is unset, Sandstar publishes the raw number via Rust's `f64::Display` (so `42.0` serializes as `"42"`, not `"42.0"`).

### What happens after config

On startup, for each driver:
1. The driver registers with the async driver actor.
2. rumqttc connects to `host:port` with the configured client ID and credentials.
3. The event-loop task spawns and awaits `Incoming::ConnAck`.
4. Every `subscribe_topic` is subscribed at the configured QoS.
5. Every `point_id` in `objects[]` is registered and added to a 5-second poll bucket.
6. Incoming PUBLISH packets update the value cache asynchronously.
7. Every 5 s, the shared tick task calls `sync_cur()`, which reads from the cache and routes values into Sandstar channels via `engine.write_channel(point_id, value, level=16, "mqtt:<id>", duration=30s)`.

Once `/api/drivers` shows `status: "Ok"` and `pollPoints > 0`, the driver is live.

---

## 3. Firewall / network

MQTT runs over TCP — **outbound** from Sandstar to the broker. Most networks allow this by default; no inbound rules are required on the BeagleBone.

| Port | Protocol | Direction | Notes |
|---|---|---|---|
| `1883` | TCP | outbound | Plain MQTT. |
| `8883` | TCP | outbound | MQTT-over-TLS (once `tls=true` is wired). |

If you're using `test.mosquitto.org`, ensure the BeagleBone has general internet access. On a locked-down site network, you may need to whitelist the broker's address.

### Local Mosquitto broker (dev box)

To validate end-to-end against a local broker on the Windows dev box:

```powershell
# Install mosquitto via chocolatey / winget / manual
net start mosquitto
# Bind to 0.0.0.0:1883 by default — edit mosquitto.conf if needed
```

Then point the BeagleBone at the dev box IP:
```bash
SANDSTAR_MQTT_CONFIGS=[{"id":"mqtt-dev","host":"192.168.1.9","port":1883,"client_id":"sandstar-dev","objects":[...]}]
```

---

## 4. Live values

Assuming a subscribed topic like `bldg/zone1/temp`:

```bash
# Publish a test value from any MQTT client
mosquitto_pub -h broker -t bldg/zone1/temp -m '72.5'

# Read it back via the Haystack REST API
curl -s "http://localhost:8085/api/read?id=103"
# [{"channel":103,"cur":72.5,"raw":72.5,"status":"Ok"}]
```

**Payload format note:** Rust's `f64::Display` omits the trailing `.0` on whole numbers. A published `"42"` and a published `"42.0"` both parse to `42.0` internally, but a write from Sandstar at value `42.0` publishes as `"42"`. If your downstream consumer is strict about trailing decimals, wrap both sides with `value_path`.

**Important: the `point_id` in `SANDSTAR_MQTT_CONFIGS` must correspond to an existing VirtualAnalog channel** defined in `database.zinc` / `points.csv`. The driver does NOT auto-create channels — writes to non-existent channels fail with `channel N not found` (logged as WARN).

To add a new MQTT-backed channel, define a VirtualAnalog entry in `database.zinc`:

```zinc
// In database.zinc:
id,dis,point,virtual,kind,unit
103,"MQTT Zone Temp",M,M,Number,"°F"
```

MQTT writes use priority **16** (lowest — automatically relinquished) so operator manual writes at lower levels always take precedence. Write duration is 30 s; the value expires if the driver stops updating the cache. Use this to detect driver outages: a channel whose `cur` hasn't changed in >30 s indicates either the broker stopped publishing or the driver disconnected.

---

## 5. Verification

### Driver status

```bash
curl -s http://localhost:8085/api/drivers | jq .
```

Expect:
```json
[{
  "driverType": "mqtt",
  "id": "mqtt-local",
  "pollBuckets": 1,
  "pollPoints": <your object count>,
  "status": "Ok"
}]
```

- `status: "Pending"` → `open()` hasn't run or failed silently
- `pollPoints: 0` → you configured zero objects, or registration failed

### Discovered points

```bash
curl -s http://localhost:8085/api/drivers/mqtt-local/learn | jq .
```

Returns one `LearnPoint` per configured object: `name = subscribe_topic`, `address = point_id`.

### Inject a test value

If you have `mosquitto_pub` installed:

```bash
mosquitto_pub -h <broker> -t bldg/zone1/temp -m '72.5'
# Wait up to 5s for the next tick, then:
curl -s "http://localhost:8085/api/read?id=103"
```

### Logs

```bash
sudo journalctl -u sandstar-engine.service -f | grep -iE 'mqtt'
```

Key log lines to look for:
- `MQTT driver registered driver=mqtt-local` — config loaded
- `MQTT poll bucket added (5s interval) driver=... points=N` — points enrolled
- `MQTT drivers opened count=N` — broker connection established
- `mqtt connected` (debug) — CONNACK received
- `mqtt cache updated` (debug) — a PUBLISH arrived and was parsed
- `MQTT poll tick task spawned (5s interval)` — tick task alive
- `MQTT poll tick complete ok=N err=0` — every 5 s, one per tick
- `MQTT sync_cur -> write_channel driver=... point_id=... value=...` — a cached value flowed into the engine
- `MQTT sync_cur failed driver=... point_id=... error=...` — per-point failure (usually stale cache)

---

## 6. Troubleshooting

### `pollPoints=0` but I configured objects

The `objects[]` array in your JSON is empty, or JSON parse failed. Check:

```bash
sudo journalctl -u sandstar-engine.service --since '5 min ago' | grep -i SANDSTAR_MQTT_CONFIGS
```

If you see `failed to parse JSON`, fix the syntax. Test with `jq`:

```bash
grep SANDSTAR_MQTT_CONFIGS /etc/sandstar/mqtt.env | cut -d= -f2- | jq .
```

### `status: "Pending"` stays that way

`open()` never completed or is retrying. Causes:
- Broker hostname doesn't resolve (`getaddrinfo` error in logs).
- Broker refused the connection — log will show `ConnectionRefused` or `NetworkError`.
- Wrong port (plain `1883` vs TLS `8883`).
- Duplicate `client_id` — the broker may kick off the first connection when a second claims the same ID. Pick a unique one per Sandstar instance.

### Connection failures

```
mqtt event loop error error=connection-error: I/O: connection refused
```

- `ping <broker>` — confirms the BeagleBone can reach the broker at layer 3.
- `nc -zv <broker> 1883` — confirms port reachability.
- Check the broker's access log — many brokers log rejected connections (bad auth, banned client ID).

### Broker auth failures

```
mqtt event loop error error=connection-error: BadUserNameOrPassword
```

Set both `username` and `password` in the config — if one is present without the other, neither is sent. Some brokers also require TLS before accepting plaintext auth; that case is not yet supported (TLS wiring is post-M4).

### Stale values

If `/api/read?id=<point>` returns the same value indefinitely or `cur` is missing:
- The cache has an entry but it's >600 s old, OR no PUBLISH has ever arrived on that topic.
- Use `mosquitto_sub -h <broker> -t 'bldg/zone1/temp' -v` on another machine to confirm messages are actually being published.
- Check the `subscribe_topic` spelling — MQTT is case-sensitive and wildcards (`+`, `#`) aren't resolved to a single cache key here.

### JSON path issues

If a PUBLISH arrives but `mqtt cache updated` doesn't show in logs, the payload probably didn't parse. The log line will be `MQTT value parse failed`:
- `path /value not found` — your `value_path` doesn't exist in the payload's JSON tree.
- `value at /value is not numeric` — the target value isn't a number, boolean, or numeric string.
- `json parse: ...` — the payload isn't valid JSON at all. If the topic publishes plain numbers, leave `value_path` unset.

### `channel N not found` in logs

The `point_id` in your MQTT config doesn't correspond to a defined channel. Add a VirtualAnalog entry to `database.zinc` (see §4, "Live values") and restart the service.

### Value is published but `/read` returns the wrong number

The channel has a higher-priority writer overriding our level-16 MQTT write. Check the priority array:

```bash
curl -s 'http://localhost:8085/api/pointWrite?id=<channel>&channel=<channel>'
```

If a SOX/HVAC control component is writing at level 1-15, its value wins. Either pick a different channel for MQTT, or reconfigure the SOX logic.

---

## File reference

| Path | Purpose |
|---|---|
| `/etc/sandstar/mqtt.env` | `SANDSTAR_MQTT_CONFIGS` JSON |
| `/etc/systemd/system/sandstar-engine.service.d/mqtt.conf` | Loads the env file |

## See also

- `docs/IMPLEMENTATION_PLAN_MQTT.md` — implementation phases M1-M4
- `docs/BACNET_SETUP.md` — sibling network-driver setup guide
- MQTT v3.1.1 spec: https://docs.oasis-open.org/mqtt/mqtt/v3.1.1/
