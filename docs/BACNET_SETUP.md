# BACnet/IP Setup Guide

Sandstar v2.6.0+ supports BACnet/IP as a network driver: discovery (Who-Is / I-Am), ReadProperty, WriteProperty, ReadPropertyMultiple batching, SubscribeCOV notifications with 240s renewal, and BBMD foreign-device registration for multi-subnet deployments.

This guide covers:
1. Enabling the driver
2. Configuring points via `SANDSTAR_BACNET_CONFIGS`
3. Firewall requirements
4. BBMD (multi-subnet) setup
5. Verification
6. Troubleshooting

---

## 1. Enabling the driver

The BACnet driver is compiled into every Sandstar build. It is **quiescent** until you set the `SANDSTAR_BACNET_CONFIGS` environment variable via a systemd drop-in.

### Create the env file

```bash
sudo mkdir -p /etc/sandstar
sudo tee /etc/sandstar/bacnet.env > /dev/null <<'EOF'
SANDSTAR_BACNET_CONFIGS=[{"id":"bacnet-local","broadcast":"192.168.1.255","port":47808,"objects":[]}]
EOF
sudo chmod 644 /etc/sandstar/bacnet.env
```

Adjust the broadcast address to match your subnet (e.g. `10.0.0.255`). The port should stay at `47808` (BACnet default `0xBAC0`).

### Create the systemd drop-in

```bash
sudo mkdir -p /etc/systemd/system/sandstar-engine.service.d
sudo tee /etc/systemd/system/sandstar-engine.service.d/bacnet.conf > /dev/null <<'EOF'
[Service]
EnvironmentFile=-/etc/sandstar/bacnet.env
EOF
sudo systemctl daemon-reload
sudo systemctl restart sandstar-engine.service
```

The `-` prefix on `EnvironmentFile` makes it optional — if the file disappears the service still starts.

---

## 2. Configuring points

Each entry in `SANDSTAR_BACNET_CONFIGS` is a JSON object describing one BACnet driver instance. Most deployments only need one.

### Schema

```json
[
  {
    "id": "bacnet-local",
    "broadcast": "192.168.1.255",
    "port": 47808,
    "bbmd": null,
    "objects": [
      {
        "point_id": 3001,
        "device_id": 12345,
        "object_type": 0,
        "instance": 0,
        "unit": "degF",
        "scale": 1.0,
        "offset": 0.0
      }
    ]
  }
]
```

### Fields

| Field | Type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Unique per driver instance; shown in `/api/drivers`. |
| `broadcast` | string | no | Broadcast address for Who-Is. Default `255.255.255.255`. |
| `port` | u16 | no | UDP port. Default `47808`. |
| `bbmd` | string | no | `"host:port"` of a BBMD for multi-subnet discovery. See §4. |
| `objects` | array | yes | Points to poll. Can be empty; driver still does discovery. |

### `objects[]` fields

| Field | Type | Notes |
|---|---|---|
| `point_id` | u32 | Sandstar channel ID. Must match a channel in `points.csv`. |
| `device_id` | u32 | BACnet device instance number. |
| `object_type` | u16 | `0`=AI, `1`=AO, `2`=AV, `3`=BI, `4`=BO, `5`=BV. |
| `instance` | u32 | BACnet object instance number on that device. |
| `unit` | string? | Engineering unit label (informational). |
| `scale` | f64? | Multiplicative scale. Default `1.0`. |
| `offset` | f64? | Additive offset. Default `0.0`. |

The value flow is: **raw device value → × scale → + offset → Sandstar channel**. For write, the inverse is applied: **Sandstar value → − offset → ÷ scale → device**.

### What happens after config

On startup, for each driver:
1. The driver registers with the async driver actor.
2. UDP socket is bound to port 47808, broadcast is enabled.
3. (If `bbmd` is set) Foreign-device registration is attempted with 300s TTL.
4. Who-Is broadcast is sent. I-Am responses are collected for 2 seconds.
5. Every `point_id` in `objects[]` is registered with the actor and added to a 5-second poll bucket.
6. `sync_cur()` fires every 5s, using RPM when multiple points share a device.

Once `/api/drivers` shows `status: "Ok"` and `pollPoints > 0`, the driver is live.

---

## 3. Firewall requirements

BACnet uses UDP 47808. Broadcast Who-Is goes out, unicast I-Am and all reads/writes come back. Most systems' default firewalls drop inbound UDP.

### Linux (firewalld)

```bash
sudo firewall-cmd --permanent --zone=public --add-port=47808/udp
sudo firewall-cmd --reload
```

**Why this matters:** firewalld's stateful tracking records outbound flows, but when Sandstar sends Who-Is to the broadcast address (`192.168.1.255:47808`), the I-Am reply comes from a **different source** (`192.168.1.9:47808`). Conntrack treats it as unrelated and the default REJECT rule drops it. Without an explicit allow rule, **discovery silently fails** — the driver's `collect_i_am` never sees any packets.

### Linux (iptables)

```bash
sudo iptables -I INPUT -p udp --dport 47808 -j ACCEPT
```

### Windows (PowerShell, elevated)

```powershell
netsh advfirewall firewall add rule name="BACnet UDP 47808" dir=in action=allow protocol=UDP localport=47808
```

---

## 4. BBMD setup (multi-subnet)

BACnet broadcasts only cross switches, not routers. If your BACnet devices are on a different subnet, register Sandstar as a **foreign device** with a BBMD that bridges the segments.

```json
{
  "id": "bacnet-remote",
  "bbmd": "192.168.2.1:47808",
  "broadcast": "192.168.1.255",
  "port": 47808,
  "objects": [ ... ]
}
```

On startup Sandstar will:
1. Send `Register-Foreign-Device` to the BBMD with 300s TTL.
2. Send Who-Is **both** locally AND via `Distribute-Broadcast-To-Network` through the BBMD.

Registration failure is **non-fatal** — the driver falls back to local-broadcast only and logs a warning.

---

## 5. Verification

### Driver status

```bash
curl -s http://localhost:8085/api/drivers | jq .
```

Expect:
```json
[{
  "driverType": "bacnet",
  "id": "bacnet-local",
  "pollBuckets": 1,
  "pollPoints": <your object count>,
  "status": "Ok"
}]
```

- `status: "Pending"` → open() hasn't run or failed silently
- `pollPoints: 0` → you configured zero objects, or registration failed

### Discovered devices

```bash
curl -s http://localhost:8085/api/drivers/bacnet-local/learn | jq .
```

Returns all objects from all discovered devices (traverses Device.ObjectList + ObjectName for each). Empty response = no devices responded to Who-Is.

### Live values

```bash
curl -s "http://localhost:8085/read?filter=channel==3001"
```

Replace `3001` with your `point_id`. Values appear ~5s after a successful discovery.

### Logs

```bash
sudo journalctl -u sandstar-engine.service -f | grep -iE 'bacnet'
```

Key log lines to look for:
- `BACnet driver registered driver=bacnet-local` — config loaded
- `BACnet discovery: RX from=... bytes=...` — receiving Who-Is/I-Am traffic
- `BACnet discovery: decoded I-Am device=<id>` — a device answered
- `BACnet discovery complete devices=<n>` — final count
- `BACnet COV subscribed process_id=<id>` — COV active
- `BACnet COV subscription renewed` — lifetime renewal working

---

## 6. Troubleshooting

### `pollPoints=0` but I configured objects

The `objects[]` array in your JSON is empty, or JSON parse failed. Check:

```bash
sudo journalctl -u sandstar-engine.service --since '5 min ago' | grep -i SANDSTAR_BACNET_CONFIGS
```

If you see `failed to parse JSON`, fix the syntax. Test with `jq`:

```bash
grep SANDSTAR_BACNET_CONFIGS /etc/sandstar/bacnet.env | cut -d= -f2- | jq .
```

### `status: "Pending"` stays that way

`open()` never completed. Causes:
- UDP port 47808 already in use. `sudo ss -ulnp | grep 47808`
- JSON parse failed (see above)

### Discovery finds zero devices even though I have BACnet hardware

**First check the firewall** (§3). This is the #1 cause.

Then verify:
- Sandstar's broadcast reaches the device's subnet (same L2 segment, or BBMD configured).
- The device actually supports BACnet/IP (not MS/TP).
- The device is powered and on the network: `ping <device-ip>`.

Run the `tools/bacnet_sim.py` simulator on another machine on the same segment to verify the network path independently of vendor-specific quirks.

### Per-point errors in `/read`

If `sync_cur` succeeds for some points and fails for others:
- `CommFault("bacnet device N not in registry")` — configured `device_id` didn't respond to Who-Is. Check the device is online.
- `RemoteStatus("BACnet error class=2 code=31")` — device returned an Error PDU. `class=2 code=31` = write-access-denied (wrong property, read-only object, etc.).

### COV notifications not updating the cache

- Check the driver issued SubscribeCOV: log line `BACnet COV subscribed`.
- On 1-11 you also need the firewalld rule (§3) — COV notifications arrive as unsolicited UDP with a source IP that doesn't match any outbound conntrack entry.
- COV cache entries expire after 600s. If the device isn't pushing notifications, the cache stays stale. `sync_cur` falls back to polling.

---

## File reference

| Path | Purpose |
|---|---|
| `/etc/sandstar/bacnet.env` | `SANDSTAR_BACNET_CONFIGS` JSON |
| `/etc/systemd/system/sandstar-engine.service.d/bacnet.conf` | Loads the env file |
| `tools/bacnet_sim.py` | Hand-crafted BACnet/IP device simulator for testing |

## See also

- `docs/IMPLEMENTATION_PLAN_FILTER_AND_BACNET.md` — implementation phases B1-B10, B8.1, B8.2
- ASHRAE 135 — BACnet protocol spec
