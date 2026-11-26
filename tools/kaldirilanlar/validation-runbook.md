# Sandstar Rust Engine -- Validation Runbook

Side-by-side validation of the Rust engine against the C engine on BeagleBone.

**Goal**: Prove that the Rust engine reads identical sensor values from the same
hardware, produces identical conversions, and can run for weeks without crashes
or memory growth -- all before replacing the C engine in production.

**Architecture during validation**:
```
BeagleBone (ARM Cortex-A8, 512MB RAM)
  |
  +-- C engine (sandstar.service)    port 8085   [production, read/write]
  +-- Rust engine (sandstar-rust-validate.service)  port 8086   [--read-only]
  |
  +-- 138 channels, 16 lookup tables, 1s poll interval
  +-- Hardware: ADC (iio), I2C (SDP810), GPIO, PWM
```

Both engines share the same config files and read from the same physical hardware.
The Rust engine runs with `--read-only` so it never writes outputs or kicks the
hardware watchdog, letting the C engine remain the sole controller.

---

## Table of Contents

1. [Pre-Deployment Checklist](#1-pre-deployment-checklist)
2. [Step-by-Step Deployment](#2-step-by-step-deployment)
3. [Validation Tests (Manual)](#3-validation-tests-manual)
4. [Automated Validation](#4-automated-validation)
5. [Soak Test Protocol](#5-soak-test-protocol)
6. [Troubleshooting](#6-troubleshooting)
7. [Success Criteria](#7-success-criteria)
8. [Rollback Procedure](#8-rollback-procedure)

---

## 1. Pre-Deployment Checklist

Run through each item before deploying the Rust engine. All checks happen on the
BeagleBone unless noted otherwise.

### 1.1 C Engine Running

```bash
systemctl status sandstar
# Should show: active (running)

curl -sf http://localhost:8085/api/status | python3 -m json.tool
# Should return JSON with channelCount=138, tableCount=16
```

If the C engine is not running, the validation is meaningless. Do not proceed
until it is healthy.

### 1.2 Config Directory Available

```bash
ls /home/eacio/sandstar/etc/EacIo/points.csv
ls /home/eacio/sandstar/etc/EacIo/tables.csv
wc -l /home/eacio/sandstar/etc/EacIo/points.csv
# Expected: 139 lines (header + 138 channels)
wc -l /home/eacio/sandstar/etc/EacIo/tables.csv
# Expected: 17 lines (header + 16 tables)
```

### 1.3 Lookup Tables Installed

```bash
ls /home/eacio/sandstar/etc/config/*.txt | wc -l
# Expected: 16 files (10kF.txt, 0-10v.txt, etc.)

# Spot-check a table file
head -5 /home/eacio/sandstar/etc/config/10kF.txt
# Should show numeric values (one per line, ADC lookup data)
```

### 1.4 Rust Binary Installed

```bash
ls -la /home/eacio/sandstar/bin/sandstar-engine-server
file /home/eacio/sandstar/bin/sandstar-engine-server
# Should report: ELF 32-bit LSB executable, ARM, EABI5

/home/eacio/sandstar/bin/sandstar-engine-server --version
# Should print version number
```

### 1.5 Port 8086 Free

```bash
ss -tlnp | grep 8086
# Should show no results. If occupied, find and stop the conflicting process.
```

### 1.6 Systemd Service File Present

```bash
ls /etc/systemd/system/sandstar-rust-validate.service
```

### 1.7 Log Directory Writable

```bash
ls -ld /var/log/sandstar/
# Should be owned by eacio or have group write for eacio
```

### 1.8 Sufficient Disk Space

```bash
df -h /var/log/sandstar/
# Need at least 100MB free for logs during soak test
```

---

## 2. Step-by-Step Deployment

### 2.1 Copy Binary to BeagleBone

From your build machine (host with ARM cross-compiled binary):

```bash
# Option A: Using the installSandstar.sh tool
/home/parallels/code/ssCompile/tools/installSandstar.sh BahaHost2Device

# Option B: Manual scp
scp target/armv7-unknown-linux-gnueabihf/release/sandstar-engine-server \
    eacio@172.28.211.135:/home/eacio/sandstar/bin/sandstar-engine-server
```

On the BeagleBone, set permissions:

```bash
chmod 755 /home/eacio/sandstar/bin/sandstar-engine-server
```

### 2.2 Install Systemd Service File

```bash
# Copy the validation service file
sudo cp /home/eacio/sandstar/etc/sandstar-rust-validate.service \
        /etc/systemd/system/sandstar-rust-validate.service

# If the service file is not already on the device, scp it from your host:
# scp etc/sandstar-rust-validate.service \
#     eacio@172.28.211.135:/tmp/sandstar-rust-validate.service
# ssh eacio@172.28.211.135 "echo <password> | sudo -S cp /tmp/sandstar-rust-validate.service /etc/systemd/system/"

# Reload systemd
sudo systemctl daemon-reload
```

### 2.3 Start Rust Engine in Validation Mode

```bash
sudo systemctl start sandstar-rust-validate

# Verify it started
systemctl status sandstar-rust-validate
```

Watch the first few seconds of logs for startup errors:

```bash
journalctl -u sandstar-rust-validate -f --no-pager -n 30
```

You should see lines like:
```
INFO  sandstar_server: Config loaded: 138 channels, 16 tables
INFO  sandstar_server: REST API listening on 0.0.0.0:8086
INFO  sandstar_server: Read-only mode enabled
INFO  sandstar_server: IPC listening on /tmp/sandstar-rust.sock
```

### 2.4 Verify Both Engines Running

```bash
# C engine
curl -sf http://localhost:8085/api/status
# Rust engine
curl -sf http://localhost:8086/api/status

# Both should return valid JSON. Compare channel counts:
echo "C engine:"
curl -sf http://localhost:8085/api/status | python3 -c "import json,sys; d=json.load(sys.stdin); print(f'  channels={d[\"channelCount\"]}, tables={d[\"tableCount\"]}')"

echo "Rust engine:"
curl -sf http://localhost:8086/api/status | python3 -c "import json,sys; d=json.load(sys.stdin); print(f'  channels={d[\"channelCount\"]}, tables={d[\"tableCount\"]}')"
```

Both should report `channels=138, tables=16`.

---

## 3. Validation Tests (Manual)

For all tests below, the device IP is represented as `$DEVICE`. Set it once:

```bash
DEVICE=172.28.211.135
# Or, if running on the BeagleBone directly:
DEVICE=localhost
```

### Test 1: Status Check

Both engines respond to `/api/status` and report consistent metadata.

```bash
echo "=== C Engine Status ==="
curl -sf "http://${DEVICE}:8085/api/status" | python3 -m json.tool

echo "=== Rust Engine Status ==="
curl -sf "http://${DEVICE}:8086/api/status" | python3 -m json.tool
```

**Expected**: Both return JSON with matching `channelCount`, `pollCount`, and
`tableCount`. The `uptimeSecs` will differ (different start times).

**Pass criteria**: `channelCount` and `tableCount` are identical.

### Test 2: Channel List

Both engines return the same set of channel IDs and metadata.

```bash
echo "=== C Engine Channels ==="
curl -sf "http://${DEVICE}:8085/api/channels" | python3 -c "
import json, sys
channels = json.load(sys.stdin)
print(f'Total: {len(channels)} channels')
for ch in sorted(channels, key=lambda c: c['id'])[:5]:
    print(f'  {ch[\"id\"]:5d}  {ch[\"label\"]:20s}  {ch[\"type\"]:10s}  {ch[\"direction\"]:5s}  enabled={ch[\"enabled\"]}')
print(f'  ... and {len(channels)-5} more')
"

echo ""
echo "=== Rust Engine Channels ==="
curl -sf "http://${DEVICE}:8086/api/channels" | python3 -c "
import json, sys
channels = json.load(sys.stdin)
print(f'Total: {len(channels)} channels')
for ch in sorted(channels, key=lambda c: c['id'])[:5]:
    print(f'  {ch[\"id\"]:5d}  {ch[\"label\"]:20s}  {ch[\"type\"]:10s}  {ch[\"direction\"]:5s}  enabled={ch[\"enabled\"]}')
print(f'  ... and {len(channels)-5} more')
"
```

**Automated channel set comparison** (run on device or any machine with access):

```bash
C_IDS=$(curl -sf "http://${DEVICE}:8085/api/channels" | python3 -c "
import json, sys
ids = sorted([c['id'] for c in json.load(sys.stdin)])
print(' '.join(str(i) for i in ids))
")

R_IDS=$(curl -sf "http://${DEVICE}:8086/api/channels" | python3 -c "
import json, sys
ids = sorted([c['id'] for c in json.load(sys.stdin)])
print(' '.join(str(i) for i in ids))
")

if [ "$C_IDS" = "$R_IDS" ]; then
    echo "PASS: Channel sets are identical"
else
    echo "FAIL: Channel sets differ"
    diff <(echo "$C_IDS" | tr ' ' '\n') <(echo "$R_IDS" | tr ' ' '\n')
fi
```

**Pass criteria**: Both engines list the same 138 channel IDs with matching labels,
types, and directions.

### Test 3: Poll Values Match

Compare current sensor readings from both engines.

```bash
echo "=== C Engine Polls (first 10) ==="
curl -sf "http://${DEVICE}:8085/api/polls" | python3 -c "
import json, sys
polls = json.load(sys.stdin)
for p in sorted(polls, key=lambda x: x['channel'])[:10]:
    print(f'  ch={p[\"channel\"]:5d}  cur={p[\"lastCur\"]:10.4f}  status={p[\"lastStatus\"]}')
print(f'Total: {len(polls)} polled channels')
"

echo ""
echo "=== Rust Engine Polls (first 10) ==="
curl -sf "http://${DEVICE}:8086/api/polls" | python3 -c "
import json, sys
polls = json.load(sys.stdin)
for p in sorted(polls, key=lambda x: x['channel'])[:10]:
    print(f'  ch={p[\"channel\"]:5d}  cur={p[\"lastCur\"]:10.4f}  status={p[\"lastStatus\"]}')
print(f'Total: {len(polls)} polled channels')
"
```

**Side-by-side poll comparison with tolerance**:

```bash
python3 - <<'PYEOF'
import json, urllib.request, sys

DEVICE = "172.28.211.135"
CUR_TOLERANCE = 0.5  # engineering units

c_data = json.loads(urllib.request.urlopen(f"http://{DEVICE}:8085/api/polls").read())
r_data = json.loads(urllib.request.urlopen(f"http://{DEVICE}:8086/api/polls").read())

c_map = {p["channel"]: p for p in c_data}
r_map = {p["channel"]: p for p in r_data}

match = close = mismatch = missing = 0
for ch, cp in sorted(c_map.items()):
    if ch not in r_map:
        print(f"MISSING  ch={ch} -- not in Rust engine")
        missing += 1
        continue
    rp = r_map[ch]
    delta = abs(cp["lastCur"] - rp["lastCur"])
    if delta < 0.001:
        match += 1
    elif delta <= CUR_TOLERANCE:
        close += 1
        print(f"NEAR     ch={ch}  C={cp['lastCur']:.4f}  Rust={rp['lastCur']:.4f}  delta={delta:.4f}")
    else:
        mismatch += 1
        print(f"DIFF     ch={ch}  C={cp['lastCur']:.4f}  Rust={rp['lastCur']:.4f}  delta={delta:.4f}")

total = match + close + mismatch + missing
print(f"\n--- Summary ---")
print(f"Exact match:     {match}")
print(f"Within tolerance:{close}")
print(f"MISMATCH:        {mismatch}")
print(f"MISSING:         {missing}")
print(f"Total:           {total}")
if total > 0:
    print(f"Match rate:      {(match + close) / total * 100:.1f}%")
PYEOF
```

**Pass criteria**: 99%+ match rate. Small NEAR differences (<0.5 units) are
expected due to ADC sampling timing differences between the two engines.

### Test 4: Read Specific Channels

Compare individual channel reads using the `/api/read` endpoint.

```bash
# Pick a few representative channels to test:
# - An analog input with thermistor conversion
# - A digital input
# - A 4-20mA current loop
# Adjust channel numbers to match your actual config.

for CH in 1113 1106 7122; do
    echo "--- Channel $CH ---"
    echo -n "C engine:    "
    curl -sf "http://${DEVICE}:8085/api/read?id=${CH}" | python3 -c "
import json, sys
rows = json.load(sys.stdin)
if rows:
    r = rows[0]
    print(f'status={r[\"status\"]}  raw={r[\"raw\"]:.2f}  cur={r[\"cur\"]:.4f}')
else:
    print('(no data)')
"
    echo -n "Rust engine: "
    curl -sf "http://${DEVICE}:8086/api/read?id=${CH}" | python3 -c "
import json, sys
rows = json.load(sys.stdin)
if rows:
    r = rows[0]
    print(f'status={r[\"status\"]}  raw={r[\"raw\"]:.2f}  cur={r[\"cur\"]:.4f}')
else:
    print('(no data)')
"
    echo ""
done
```

Also test the Haystack-style filter syntax:

```bash
# Read by filter expression
curl -sf "http://${DEVICE}:8086/api/read?filter=channel==1113" | python3 -m json.tool
```

**Pass criteria**: For each channel, `status` matches and `cur` is within 0.5
engineering units.

### Test 5: History Capture

Verify the Rust engine's in-memory history ring buffer is working. The C engine
may not have this endpoint (Rust-only feature).

```bash
# Wait at least 30 seconds after starting the Rust engine so some history
# accumulates, then query it.

echo "=== Last 10 history points for channel 1113 ==="
curl -sf "http://${DEVICE}:8086/api/history/1113?duration=5m&limit=10" | python3 -c "
import json, sys
points = json.load(sys.stdin)
print(f'Returned {len(points)} points')
for p in points:
    print(f'  ts={p[\"ts\"]}  cur={p[\"cur\"]:.4f}  raw={p[\"raw\"]:.2f}  status={p[\"status\"]}')
"

echo ""
echo "=== Last 1 hour of history for channel 7122 ==="
curl -sf "http://${DEVICE}:8086/api/history/7122?duration=1h&limit=50" | python3 -c "
import json, sys
points = json.load(sys.stdin)
print(f'Returned {len(points)} points')
if points:
    print(f'  First: ts={points[0][\"ts\"]}  cur={points[0][\"cur\"]:.4f}')
    print(f'  Last:  ts={points[-1][\"ts\"]}  cur={points[-1][\"cur\"]:.4f}')
"
```

**Pass criteria**: History endpoint returns data points with timestamps that
increase monotonically and `cur` values consistent with current readings.

### Test 6: Zinc Wire Format

Verify the Rust engine serves Haystack Zinc 3.0 grid format when requested via
the `Accept: text/zinc` header.

```bash
echo "=== /api/about (Zinc) ==="
curl -sf -H "Accept: text/zinc" "http://${DEVICE}:8086/api/about"
echo ""

echo "=== /api/status (Zinc) ==="
curl -sf -H "Accept: text/zinc" "http://${DEVICE}:8086/api/status"
echo ""

echo "=== /api/ops (Zinc) ==="
curl -sf -H "Accept: text/zinc" "http://${DEVICE}:8086/api/ops"
echo ""

echo "=== /api/polls (Zinc, first few rows) ==="
curl -sf -H "Accept: text/zinc" "http://${DEVICE}:8086/api/polls" | head -5
echo ""

echo "=== /api/read?filter=channel==1113 (Zinc) ==="
curl -sf -H "Accept: text/zinc" "http://${DEVICE}:8086/api/read?filter=channel==1113"
echo ""

echo "=== /api/history/1113?limit=3 (Zinc) ==="
curl -sf -H "Accept: text/zinc" "http://${DEVICE}:8086/api/history/1113?limit=3"
echo ""
```

**Expected Zinc format** (example for /api/status):
```
ver:"3.0"
uptimeSecs,channelCount,pollCount,tableCount,pollIntervalMs
3600,138,138,16,1000
```

**Pass criteria**: Response Content-Type is `text/zinc; charset=utf-8`. Body starts
with `ver:"3.0"` and contains a valid Zinc grid with correct columns.

### Test 7: Write Rejection (Read-Only Mode)

The Rust engine runs with `--read-only`. Writes must be rejected.

```bash
echo "=== Attempt pointWrite (should fail) ==="
curl -sf -X POST "http://${DEVICE}:8086/api/pointWrite" \
     -H "Content-Type: application/json" \
     -d '{"channel": 1113, "value": 50.0}' \
     -w "\nHTTP Status: %{http_code}\n"

echo ""
echo "=== Attempt reload (should fail or be safe) ==="
curl -sf -X POST "http://${DEVICE}:8086/api/reload" \
     -w "\nHTTP Status: %{http_code}\n"

echo ""
echo "=== Attempt pollNow (should succeed -- read-only does not block polls) ==="
curl -sf -X POST "http://${DEVICE}:8086/api/pollNow" | python3 -m json.tool
```

**Pass criteria**:
- `pointWrite` returns HTTP 500 with an error message containing "read-only" or
  "rejected".
- `pollNow` succeeds (polling is a read operation).
- The C engine's output channels are unaffected.

### Test 8: Error Handling

Test that invalid requests produce clean error responses, not panics.

```bash
echo "=== Read non-existent channel ==="
curl -sf "http://${DEVICE}:8086/api/read?id=99999" \
     -w "\nHTTP Status: %{http_code}\n"

echo ""
echo "=== Read with invalid filter ==="
curl -sf "http://${DEVICE}:8086/api/read?filter=!!!invalid!!!" \
     -w "\nHTTP Status: %{http_code}\n"

echo ""
echo "=== History for non-existent channel ==="
curl -sf "http://${DEVICE}:8086/api/history/99999?limit=5" \
     -w "\nHTTP Status: %{http_code}\n"

echo ""
echo "=== pointWrite with missing fields ==="
curl -sf -X POST "http://${DEVICE}:8086/api/pointWrite" \
     -H "Content-Type: application/json" \
     -d '{}' \
     -w "\nHTTP Status: %{http_code}\n"

echo ""
echo "=== watchPoll with invalid watch ID ==="
curl -sf -X POST "http://${DEVICE}:8086/api/watchPoll" \
     -H "Content-Type: application/json" \
     -d '{"watchId": "does-not-exist", "refresh": false}' \
     -w "\nHTTP Status: %{http_code}\n"
```

**Pass criteria**: All requests return a JSON error response `{"err": "..."}` with
an appropriate HTTP status code. The Rust engine does not crash. Verify it is still
running after the test:

```bash
curl -sf "http://${DEVICE}:8086/api/status" > /dev/null && echo "Engine still running: OK"
```

### Test 9: Watch Subscriptions

Test the Haystack watch subscription lifecycle.

```bash
echo "=== Create a watch ==="
WATCH_RESP=$(curl -sf -X POST "http://${DEVICE}:8086/api/watchSub" \
    -H "Content-Type: application/json" \
    -d '{"dis": "validation-test", "ids": [1113, 1106, 7122]}')
echo "$WATCH_RESP" | python3 -m json.tool
WATCH_ID=$(echo "$WATCH_RESP" | python3 -c "import json,sys; print(json.load(sys.stdin)['watchId'])")
echo "Watch ID: $WATCH_ID"

echo ""
echo "=== Poll the watch (should return current values) ==="
sleep 2
curl -sf -X POST "http://${DEVICE}:8086/api/watchPoll" \
    -H "Content-Type: application/json" \
    -d "{\"watchId\": \"$WATCH_ID\", \"refresh\": true}" | python3 -m json.tool

echo ""
echo "=== Close the watch ==="
curl -sf -X POST "http://${DEVICE}:8086/api/watchUnsub" \
    -H "Content-Type: application/json" \
    -d "{\"watchId\": \"$WATCH_ID\", \"close\": true}" | python3 -m json.tool
```

**Pass criteria**: Watch creation returns a `watchId` and initial values. Poll returns
values for subscribed channels. Unsub with `close: true` succeeds.

### Test 10: About and Ops (Haystack Compliance)

```bash
echo "=== /api/about ==="
curl -sf "http://${DEVICE}:8086/api/about" | python3 -m json.tool

echo ""
echo "=== /api/ops ==="
curl -sf "http://${DEVICE}:8086/api/ops" | python3 -c "
import json, sys
ops = json.load(sys.stdin)
print(f'{len(ops)} operations:')
for op in ops:
    print(f'  {op[\"name\"]:15s}  {op[\"summary\"]}')
"
```

**Pass criteria**: `/api/about` returns serverName, vendorName, productVersion,
haystackVersion="3.0", and valid ISO timestamps. `/api/ops` lists all 14 operations.

---

## 4. Automated Validation

### 4.1 Running validate-engines.sh

The automated comparison script continuously polls both engines and reports
discrepancies.

**From a machine with network access to the BeagleBone**:

```bash
# Default: 2-second interval, run forever
./tools/validate-engines.sh 172.28.211.135

# Custom: 1-second interval, stop after 60 minutes
./tools/validate-engines.sh 172.28.211.135 1 60

# Local testing (on the BeagleBone itself)
./tools/validate-engines.sh localhost 2 0
```

**What the script does**:
1. Connects to both engines and prints their status
2. Every N seconds, fetches `/api/polls` from both
3. Compares each channel's `lastCur` and `lastStatus`
4. Classifies each comparison as OK, NEAR, DIFF, or MISS
5. Prints a summary every 60 cycles
6. On Ctrl+C, prints a final summary

### 4.2 Interpreting Output

**Log entries** (written to `/tmp/sandstar_validate/validate_TIMESTAMP.log`):

| Prefix | Meaning | Action |
|--------|---------|--------|
| `OK` | Values match exactly (<0.001 delta) | Good. Logged to file only. |
| `NEAR` | Values within tolerance (0.001 -- 0.5) | Normal for ADC timing. Logged to file only. |
| `DIFF` | Values exceed tolerance (>0.5 units) | Investigate. Printed to console AND log. |
| `MISS` | Channel in C but missing from Rust | Config loading bug. Printed to console AND log. |
| `STAT` | Status mismatch (e.g., "Ok" vs "Fault") | Investigate. Printed to console AND log. |
| `ERR` | One engine's API call failed | Network or engine crash. Printed to console AND log. |

**Summary block** (every 60 cycles and on exit):

```
=== Validation Summary (120 cycles, 240s elapsed) ===
Total comparisons:  16560
  Exact match:      16412
  Within tolerance:  140
  MISMATCH:         8
  C engine errors:  0
  Rust engine errors: 0
  Match rate:       99.95%
===============================================
```

### 4.3 Tolerance Thresholds

Defined in `validate-engines.sh`:

| Threshold | Value | Rationale |
|-----------|-------|-----------|
| `CUR_TOLERANCE` | 0.5 engineering units | Covers ADC jitter + poll timing offset |
| `RAW_TOLERANCE` | 5.0 ADC counts | Raw ADC noise floor on BeagleBone (12-bit) |
| Exact match | <0.001 | Floating-point precision boundary |

These tolerances account for:
- The two engines poll hardware at slightly different times within a 1-second cycle
- ADC noise on the BeagleBone's 12-bit converter (TI AM335x) is typically 2-3 LSBs
- Thermistor lookup table interpolation may produce slightly different results at
  boundary points due to floating-point differences between C `double` and Rust `f64`

### 4.4 Running from Cron (Unattended)

To run a 24-hour validation and email results:

```bash
# On a machine with network access to the BeagleBone
nohup ./tools/validate-engines.sh 172.28.211.135 2 1440 > /dev/null 2>&1 &
echo "PID: $!"

# The script writes to /tmp/sandstar_validate/validate_TIMESTAMP.log
# Check on it:
tail -f /tmp/sandstar_validate/validate_*.log
```

---

## 5. Soak Test Protocol

### 5.1 Duration

| Phase | Duration | Purpose |
|-------|----------|---------|
| Smoke test | 1 hour | Verify no immediate crashes or mismatches |
| Short soak | 24 hours | Confirm stability over day/night temperature swing |
| Full soak | 1--2 weeks | Prove production readiness; catch slow memory leaks |

### 5.2 Monitoring: What to Watch

**A. Match rate** (via validate-engines.sh or periodic manual checks):

```bash
# Quick spot-check match rate
./tools/validate-engines.sh 172.28.211.135 1 5
# Run for 5 minutes, check final summary
```

**B. Memory usage** (MemoryMax is set to 64MB in the service file):

```bash
# Current RSS
ps -o pid,rss,vsz,comm -p $(pgrep -f sandstar-engine-server | head -1)

# Systemd cgroup memory
systemctl status sandstar-rust-validate | grep Memory

# Over-time monitoring (log every 5 minutes to a file)
while true; do
    echo "$(date '+%Y-%m-%dT%H:%M:%S') $(ps -o rss= -p $(pgrep -f 'sandstar-engine-server.*8086') 2>/dev/null || echo 'DEAD')" \
        >> /tmp/sandstar_validate/memory_usage.log
    sleep 300
done
```

Watch for:
- Steady-state RSS should be 15--30MB
- Growth >1MB/hour indicates a memory leak
- Hard kill at 64MB (systemd MemoryMax)

**C. CPU usage**:

```bash
# Snapshot
top -b -n1 -p $(pgrep -f 'sandstar-engine-server.*8086')

# Over time (1-minute averages)
pidstat -p $(pgrep -f 'sandstar-engine-server.*8086') 60
```

Expected: <5% CPU steady-state (1s poll interval, 138 channels).

**D. Engine uptime and restarts**:

```bash
# Check if systemd restarted the Rust engine
systemctl show sandstar-rust-validate --property=NRestarts
# Should be 0

# Uptime from the engine itself
curl -sf http://localhost:8086/api/status | python3 -c "
import json, sys
s = json.load(sys.stdin)
hours = s['uptimeSecs'] / 3600
print(f'Uptime: {hours:.1f} hours ({s[\"uptimeSecs\"]} seconds)')
"
```

**E. Log file size**:

```bash
ls -lh /var/log/sandstar/sandstar-rust.log
# Should not grow unboundedly at info level
```

### 5.3 Alerting: How to Detect Divergence

**Simple cron-based alert** (check every 10 minutes):

```bash
# /home/eacio/check_rust_engine.sh
#!/bin/bash
# Exit silently if everything is fine

# 1. Check engine is running
if ! curl -sf -o /dev/null --connect-timeout 5 http://localhost:8086/api/status; then
    echo "ALERT: Rust engine not responding on port 8086" | \
        mail -s "Sandstar Rust Engine DOWN" admin@example.com
    exit 1
fi

# 2. Check systemd restarts
RESTARTS=$(systemctl show sandstar-rust-validate --property=NRestarts --value)
if [ "$RESTARTS" -gt 0 ]; then
    echo "ALERT: Rust engine has restarted $RESTARTS times" | \
        mail -s "Sandstar Rust Engine Restarted" admin@example.com
fi

# 3. Check memory (warn at 50MB)
RSS_KB=$(ps -o rss= -p $(pgrep -f 'sandstar-engine-server.*8086') 2>/dev/null)
if [ -n "$RSS_KB" ] && [ "$RSS_KB" -gt 51200 ]; then
    echo "ALERT: Rust engine RSS=${RSS_KB}KB (>50MB)" | \
        mail -s "Sandstar Rust Engine High Memory" admin@example.com
fi
```

Install the cron job:
```bash
# Check every 10 minutes
echo "*/10 * * * * /home/eacio/check_rust_engine.sh" | crontab -
```

If mail is not configured, write alerts to a file:

```bash
# Replace the mail line with:
echo "$(date) ALERT: ..." >> /tmp/sandstar_validate/alerts.log
```

### 5.4 Log Management

**Rust engine log file**: `/var/log/sandstar/sandstar-rust.log`

At `info` level, the log grows approximately 1--5 MB/day depending on channel count
and whether any anomalies are logged.

**Rotation with logrotate**:

```bash
# /etc/logrotate.d/sandstar-rust
/var/log/sandstar/sandstar-rust.log {
    daily
    rotate 7
    compress
    delaycompress
    missingok
    notifempty
    postrotate
        systemctl reload sandstar-rust-validate 2>/dev/null || true
    endscript
}
```

**Validation script logs**: `/tmp/sandstar_validate/validate_*.log`

These grow faster (one line per channel per cycle). At 138 channels x 2s interval,
that is approximately 100 MB/day. Clean up old runs:

```bash
# Delete validation logs older than 7 days
find /tmp/sandstar_validate -name "validate_*.log" -mtime +7 -delete
```

**Journald logs** (systemd):

```bash
# View recent logs
journalctl -u sandstar-rust-validate --since "1 hour ago"

# Export to file
journalctl -u sandstar-rust-validate --since "2026-03-01" > /tmp/rust-engine-journal.log
```

---

## 6. Troubleshooting

### 6.1 Rust Engine Won't Start

```bash
# Check the journal for error messages
journalctl -u sandstar-rust-validate -n 50 --no-pager

# Common causes:
# a) Port 8086 already in use
ss -tlnp | grep 8086

# b) Config directory not found
ls -la /home/eacio/sandstar/etc/EacIo/

# c) Binary not executable or wrong architecture
file /home/eacio/sandstar/bin/sandstar-engine-server
# Must show: ELF 32-bit LSB executable, ARM, EABI5

# d) Missing shared libraries
ldd /home/eacio/sandstar/bin/sandstar-engine-server
# Static Rust binaries should show "not a dynamic executable" -- this is fine.
# If it shows missing .so files, the build is broken.

# e) Permission denied on hardware paths
ls -la /dev/i2c-2
ls -la /sys/bus/iio/devices/iio:device0/
```

### 6.2 Engines Show Different Channel Counts

```bash
# Compare channel lists in detail
diff <(curl -sf http://localhost:8085/api/channels | python3 -c "
import json, sys
for c in sorted(json.load(sys.stdin), key=lambda x: x['id']):
    print(c['id'])
") <(curl -sf http://localhost:8086/api/channels | python3 -c "
import json, sys
for c in sorted(json.load(sys.stdin), key=lambda x: x['id']):
    print(c['id'])
")
```

If channels are missing from the Rust engine, check:
- points.csv parsing (does `wc -l` match expected count?)
- Error messages in the Rust log about skipped channels
- CSV encoding (must be UTF-8, line endings LF or CRLF)

### 6.3 Persistent Value Mismatches on Specific Channels

```bash
# Repeatedly read a problematic channel from both engines
for i in $(seq 1 10); do
    C_VAL=$(curl -sf http://localhost:8085/api/read?id=1113 | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['cur'])")
    R_VAL=$(curl -sf http://localhost:8086/api/read?id=1113 | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['cur'])")
    echo "$(date +%H:%M:%S)  C=$C_VAL  Rust=$R_VAL  delta=$(python3 -c "print(f'{abs($C_VAL - $R_VAL):.6f}')")"
    sleep 1
done
```

Possible causes:
- **Table interpolation difference**: The C engine and Rust engine may use slightly
  different interpolation at table boundaries. Log the raw ADC value to check.
- **Conversion parameter mismatch**: Check that offset, scale, low, high, min, max
  are parsed identically from points.csv.
- **I2C sensor timing**: I2C reads (SDP810) can vary if the two engines issue bus
  transactions at different times.

### 6.4 Rust Engine Crashes After Hours

```bash
# Check for OOM kill
journalctl -u sandstar-rust-validate | grep -i "kill\|oom\|memory"
dmesg | grep -i "out of memory\|killed process"

# Check core dump (if configured)
coredumpctl list | tail -5

# Increase log level for next run to capture more context
# Edit the service file, change --log-level info to --log-level debug
sudo systemctl edit sandstar-rust-validate
# Add under [Service]:
#   ExecStart=
#   ExecStart=/home/eacio/sandstar/bin/sandstar-engine-server \
#       --config-dir /home/eacio/sandstar/etc/EacIo \
#       --http-port 8086 \
#       --socket /tmp/sandstar-rust.sock \
#       --log-file /var/log/sandstar/sandstar-rust.log \
#       --log-level debug \
#       --read-only
sudo systemctl daemon-reload
sudo systemctl restart sandstar-rust-validate
```

**Warning**: Debug logging at 1s poll interval with 138 channels produces
significant I/O. Only use temporarily for diagnosis.

### 6.5 How to Restart Without Affecting C Engine

The two engines are completely independent processes. Restarting the Rust engine
has zero effect on the C engine.

```bash
# Restart Rust engine only
sudo systemctl restart sandstar-rust-validate

# Verify C engine is still running
curl -sf http://localhost:8085/api/status | python3 -m json.tool
# Verify Rust engine restarted
curl -sf http://localhost:8086/api/status | python3 -m json.tool
```

### 6.6 Engine Responds but Returns No Polls

```bash
# Check if the engine has started polling yet
curl -sf http://localhost:8086/api/status | python3 -c "
import json, sys
s = json.load(sys.stdin)
print(f'pollCount={s[\"pollCount\"]}  uptimeSecs={s[\"uptimeSecs\"]}')
"
# If pollCount is 0 after >5 seconds uptime, the HAL may not be initialized
```

Check that the hardware initialization script ran for the C engine service
(the validation service depends on `sandstar.service` which runs `initialize.sh`):

```bash
systemctl status sandstar
# Must be active -- the Rust validation service has Wants=sandstar.service
```

---

## 7. Success Criteria

The Rust engine is validated and ready for production replacement when ALL of the
following criteria are met:

### Functional

| Criterion | Threshold | How to Verify |
|-----------|-----------|---------------|
| Channel count matches | 138/138 | Test 2 |
| Table count matches | 16/16 | Test 1 |
| Poll value match rate | >= 99.9% over 24h | validate-engines.sh summary |
| No MISS channels | 0 | validate-engines.sh |
| Status match | All channels Ok | Test 3 |
| Zinc format valid | All endpoints | Test 6 |
| Write rejection works | 100% | Test 7 |
| Error handling clean | No panics | Test 8 |
| History recording | Working | Test 5 |
| Watch subscriptions | Working | Test 9 |

### Stability

| Criterion | Threshold | How to Verify |
|-----------|-----------|---------------|
| Uptime without crash | >= 24h (smoke), >= 7d (full) | `systemctl show NRestarts` |
| Systemd restarts | 0 | `systemctl show NRestarts` |
| No OOM kills | 0 | `dmesg`, `journalctl` |

### Resource Usage

| Resource | Limit | How to Verify |
|----------|-------|---------------|
| RSS memory | < 64MB (steady-state < 30MB) | `ps -o rss` |
| Memory growth | < 1MB/day | memory_usage.log trend |
| CPU usage | < 5% (steady-state) | `pidstat` |
| Log file growth | < 5MB/day at info level | `ls -lh` |

### Sign-Off Checklist

Before replacing the C engine:

- [ ] 24-hour smoke test passes all criteria above
- [ ] 1-week soak test shows no memory leaks or crashes
- [ ] Match rate has been >= 99.9% for the entire soak period
- [ ] No unexplained DIFF entries in the last 48 hours
- [ ] Admin has reviewed validation logs
- [ ] Rollback procedure has been tested (Section 8)

---

## 8. Rollback Procedure

If the Rust engine causes problems during validation, remove it in under 60 seconds.
The C engine is unaffected at all times.

### Immediate Stop

```bash
sudo systemctl stop sandstar-rust-validate
sudo systemctl disable sandstar-rust-validate
```

### Full Removal

```bash
sudo systemctl stop sandstar-rust-validate
sudo systemctl disable sandstar-rust-validate
sudo rm /etc/systemd/system/sandstar-rust-validate.service
sudo systemctl daemon-reload

# Optionally remove the binary
rm /home/eacio/sandstar/bin/sandstar-engine-server

# Clean up logs
rm -f /var/log/sandstar/sandstar-rust.log
rm -rf /tmp/sandstar_validate/
```

### Verify C Engine Unaffected

```bash
systemctl status sandstar
curl -sf http://localhost:8085/api/status | python3 -m json.tool
```

The C engine should show no interruption in uptime or behavior.

---

## Appendix A: REST API Quick Reference

All endpoints available on the Rust engine (port 8086).

| Method | Endpoint | Description | Zinc? |
|--------|----------|-------------|-------|
| GET | `/api/about` | Server metadata, Haystack version | Yes |
| GET | `/api/ops` | List of available operations | Yes |
| GET | `/api/read?id=N` | Read channel by ID | Yes |
| GET | `/api/read?filter=EXPR` | Read channels by Haystack filter | Yes |
| GET | `/api/status` | Engine uptime, counts | Yes |
| GET | `/api/channels` | List all 138 channels | Yes |
| GET | `/api/polls` | List polled channels with values | Yes |
| GET | `/api/tables` | List lookup table names | No |
| GET | `/api/history/:ch` | Channel value history ring buffer | Yes |
| POST | `/api/pointWrite` | Write to output (blocked in --read-only) | No |
| POST | `/api/pollNow` | Trigger immediate poll cycle | No |
| POST | `/api/reload` | Reload config from disk | No |
| POST | `/api/watchSub` | Subscribe to channel changes | Yes |
| POST | `/api/watchUnsub` | Unsubscribe or close watch | No |
| POST | `/api/watchPoll` | Poll for changed watch values | Yes |

**Zinc format**: Add `Accept: text/zinc` header to GET requests marked "Yes" above.

**History query params**: `since` (Unix epoch), `duration` ("1h", "24h", "7d"),
`limit` (default 100, max 10000).

## Appendix B: File Locations on BeagleBone

| File | Path |
|------|------|
| Rust binary | `/home/eacio/sandstar/bin/sandstar-engine-server` |
| Systemd service | `/etc/systemd/system/sandstar-rust-validate.service` |
| Engine log | `/var/log/sandstar/sandstar-rust.log` |
| PID file | `/var/run/sandstar/sandstar-rust.pid` |
| IPC socket | `/tmp/sandstar-rust.sock` |
| Config dir | `/home/eacio/sandstar/etc/EacIo/` |
| Lookup tables | `/home/eacio/sandstar/etc/config/` |
| points.csv | `/home/eacio/sandstar/etc/EacIo/points.csv` |
| tables.csv | `/home/eacio/sandstar/etc/EacIo/tables.csv` |
| Validation logs | `/tmp/sandstar_validate/` |
| HW init script | `/home/eacio/sandstar/etc/init/initialize.sh` |

## Appendix C: Port Assignments

| Port | Service | Owner |
|------|---------|-------|
| 8085 | Haystack REST API | C engine (sandstar.service) |
| 8086 | Haystack REST API | Rust engine (sandstar-rust-validate.service) |
| 9813 | IPC (TCP, Windows only) | sandstar-engine-server |
| -- | `/tmp/sandstar-engine.sock` | C engine IPC (Unix) |
| -- | `/tmp/sandstar-rust.sock` | Rust engine IPC (Unix) |
