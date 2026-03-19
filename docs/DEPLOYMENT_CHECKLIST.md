# Sandstar Rust — Hardware Deployment Checklist

**Date:** 2026-03-10
**Software Status:** 800+ tests passing, 0 clippy warnings, all phases complete
**Target:** BeagleBone at 172.28.211.135 (SSH port 1919, user eacio)
**Jump host:** solidyne@172.28.109.221
**Blocker:** BeagleBone unreachable from Windows — deploy from Parallels Linux VM

---

## Pre-Deployment (from Parallels Linux VM)

### 1. Build ARM .deb Package

```bash
# Set PATH for cargo
export PATH="$HOME/.cargo/bin:$PATH"

# Cross-compile for ARM (uses cargo-zigbuild + Zig cc)
cd /home/parallels/code/ssCompile/ssCompile/sandstar_rust
rtk cargo build --target armv7-unknown-linux-gnueabihf --release --no-default-features --features linux-hal

# Package as .deb
rtk cargo deb --target armv7-unknown-linux-gnueabihf --no-build -p sandstar-server --variant linux-hal
```

The `.deb` will be at:
`target/armv7-unknown-linux-gnueabihf/debian/sandstar_*.deb`

**Shorthand** (if `cargo arm-build` / `cargo arm-deb` aliases are set up):
```bash
rtk cargo arm-build && rtk cargo arm-deb
```

### 2. Transfer to Device

**Option A: Using the install script (recommended)**
```bash
# Copy the .deb to the ssCompile root where installSandstar.sh expects it
cp target/armv7-unknown-linux-gnueabihf/debian/sandstar_*.deb /home/parallels/code/ssCompile/

# Install (handles jump host, scp, dpkg, service restart)
/home/parallels/code/ssCompile/tools/installSandstar.sh BahaHost2Device
```

**Option B: Manual transfer via jump host**
```bash
# scp through jump host
scp -C -o ProxyJump=solidyne@172.28.109.221 -P 1919 \
    target/armv7-unknown-linux-gnueabihf/debian/sandstar_*.deb \
    eacio@172.28.211.135:/home/eacio/
```

### 3. Network Verification

```bash
# From Parallels VM: verify BeagleBone reachable via jump host
ssh -J solidyne@172.28.109.221 -p 1919 eacio@172.28.211.135 "echo 'Connection OK'; uname -a"

# Verify C engine is running
ssh -J solidyne@172.28.109.221 -p 1919 eacio@172.28.211.135 \
    "curl -sf http://localhost:8085/api/status | python3 -m json.tool"
```

---

## Phase 1: Install (15 min)

> All commands below run ON the BeagleBone (SSH in first).

```bash
ssh -J solidyne@172.28.109.221 -p 1919 eacio@172.28.211.135
```

### 4. Install .deb Package

```bash
# If using installSandstar.sh (Option A above), skip this — it does dpkg for you.

# Manual install:
echo "$SANDSTAR_SUDO_PASS" | sudo -S dpkg -i /home/eacio/sandstar_*.deb
sudo chmod -R 755 /home/eacio/sandstar
sudo chown eacio:root -R /home/eacio/sandstar
sudo systemctl daemon-reload
```

### 5. Verify Installation

```bash
# Binary is correct architecture
file /home/eacio/sandstar/bin/sandstar-engine-server
# Expected: ELF 32-bit LSB executable, ARM, EABI5

# Version check
/home/eacio/sandstar/bin/sandstar-engine-server --version

# Config files present
wc -l /home/eacio/sandstar/etc/EacIo/points.csv   # Expected: 139 lines (header + 138 channels)
wc -l /home/eacio/sandstar/etc/EacIo/tables.csv    # Expected: 17 lines (header + 16 tables)
ls /home/eacio/sandstar/etc/config/*.txt | wc -l     # Expected: 16 lookup table files

# Systemd service files installed
ls -la /etc/systemd/system/sandstar-engine.service
ls -la /etc/systemd/system/sandstar-rust-validate.service

# C engine still running (must NOT be disrupted)
systemctl status sandstar
curl -sf http://localhost:8085/api/status | python3 -m json.tool
```

---

## Phase 2: Side-by-Side Validation (4-8 hours)

### 6. Start Rust Engine in Read-Only Mode

```bash
# Start the validation service (port 8086, --read-only)
echo "$SANDSTAR_SUDO_PASS" | sudo -S systemctl start sandstar-rust-validate

# Verify it started
systemctl status sandstar-rust-validate

# Watch initial logs (Ctrl+C to stop following)
journalctl -u sandstar-rust-validate -f --no-pager -n 30
# Look for:
#   INFO: Config loaded: 138 channels, 16 tables
#   INFO: REST API listening on 0.0.0.0:8086
#   INFO: Read-only mode enabled

# Verify both engines responding
curl -sf http://localhost:8085/api/status | python3 -m json.tool   # C engine
curl -sf http://localhost:8086/api/status | python3 -m json.tool   # Rust engine
```

### 7. Start Soak Monitor

```bash
# Run soak monitor for 8 hours (checks every 60s)
nohup /home/eacio/sandstar/bin/soak-monitor.sh --duration 8h --interval 60 \
    > /dev/null 2>&1 &
echo "Soak monitor PID: $!"

# Verify it's running
tail -5 /var/log/sandstar/soak-monitor.log
```

Also start the automated comparison script:
```bash
# From a machine with network access (or on the BeagleBone itself)
nohup /home/eacio/sandstar/bin/validate-engines.sh localhost 2 480 \
    > /dev/null 2>&1 &
echo "Validate PID: $!"
```

### 8. Validation Checks (every hour)

Run these checks hourly during the soak period:

```bash
# A. Match rate
tail -5 /var/log/sandstar/soak-monitor.log

# B. Memory usage
ps -o pid,rss,vsz,comm -p $(pgrep -f sandstar-engine-server | head -1)
systemctl status sandstar-rust-validate | grep Memory

# C. Systemd restarts (must be 0)
systemctl show sandstar-rust-validate --property=NRestarts

# D. API health
curl -sf http://localhost:8086/api/status | python3 -c "
import json, sys
s = json.load(sys.stdin)
hours = s['uptimeSecs'] / 3600
print(f'Uptime: {hours:.1f}h, Channels: {s[\"channelCount\"]}, Polls: {s[\"pollCount\"]}')
"

# E. Check for alerts
cat /var/log/sandstar/soak-alerts.log 2>/dev/null || echo "No alerts"

# F. Check for panics/errors
journalctl -u sandstar-rust-validate --since "1 hour ago" | grep -ciE 'panic|fatal|SIGSE' || echo "0 errors"
```

### 9. Pass/Fail Criteria

| Criterion | Threshold | Status |
|-----------|-----------|--------|
| Match rate | >99% for all 138 channels | [ ] |
| Memory RSS | Stable <64MB, growth <512KB/check | [ ] |
| API health | /api/about returns 200 consistently | [ ] |
| Panics/fatal | 0 panics, 0 fatal errors | [ ] |
| Systemd restarts | 0 | [ ] |
| Poll latency | <100ms average | [ ] |
| Channel count | 138/138 matches C engine | [ ] |
| Table count | 16/16 matches C engine | [ ] |

**If any criterion fails:** Do NOT proceed to cutover. Investigate using the
[Troubleshooting section of the validation runbook](../tools/validation-runbook.md#6-troubleshooting).

---

## Phase 3: Cutover (30 min)

### 10. Pre-Cutover Checklist

Run through each item before switching:

- [ ] Soak monitor ran for >= 4 hours with 0 alerts
- [ ] Match rate has been >= 99% for the entire soak period
- [ ] RSS memory stable (no growth trend)
- [ ] 0 systemd restarts
- [ ] 0 panics or fatal errors in journal
- [ ] validate-engines.sh shows 0 MISS, 0 persistent DIFF
- [ ] Rollback dry run completed (step 11)

### 11. Rollback Dry Run

**Verify rollback works BEFORE committing to cutover:**

```bash
# Stop Rust validation, verify C engine unaffected
echo "$SANDSTAR_SUDO_PASS" | sudo -S systemctl stop sandstar-rust-validate

# C engine must still be healthy
curl -sf http://localhost:8085/api/status | python3 -m json.tool
# Confirm: channelCount=138, tableCount=16

# Restart Rust validation for the actual cutover
echo "$SANDSTAR_SUDO_PASS" | sudo -S systemctl start sandstar-rust-validate
sleep 3
curl -sf http://localhost:8086/api/status > /dev/null && echo "Rust engine back up"
```

### 12. Execute Cutover

```bash
# Run the cutover script (interactive — asks for confirmation)
echo "$SANDSTAR_SUDO_PASS" | sudo -S /home/eacio/sandstar/bin/cutover-to-rust.sh
```

What the script does:
1. Verifies Rust validation service is healthy on port 8086
2. Creates backup snapshot in `/home/eacio/sandstar/backup/cutover-TIMESTAMP/`
3. Stops Rust validation service (sandstar-rust-validate)
4. Stops C engine (sandstar.service)
5. Starts Rust engine as production (sandstar-engine.service on port 8085)
6. Verifies API responds on port 8085

For automation (no confirmation prompt):
```bash
echo "$SANDSTAR_SUDO_PASS" | sudo -S /home/eacio/sandstar/bin/cutover-to-rust.sh --force
```

### 13. Post-Cutover Verification

```bash
# Rust engine is now production on port 8085
systemctl status sandstar-engine

# API responding
curl -sf http://localhost:8085/api/status | python3 -c "
import json, sys
s = json.load(sys.stdin)
print(f'Engine: Rust')
print(f'Channels: {s[\"channelCount\"]}')
print(f'Tables: {s[\"tableCount\"]}')
print(f'Uptime: {s[\"uptimeSecs\"]}s')
"

# All channels reading
curl -sf http://localhost:8085/api/polls | python3 -c "
import json, sys
polls = json.load(sys.stdin)
ok = sum(1 for p in polls if p.get('lastStatus') == 'Ok')
print(f'Polling: {ok}/{len(polls)} channels OK')
"

# Verify from external machine (via jump host)
# ssh -J solidyne@172.28.109.221 -p 1919 eacio@172.28.211.135 \
#     "curl -sf http://localhost:8085/api/status"

# C engine should be stopped
systemctl status sandstar 2>/dev/null || echo "C engine stopped (expected)"
```

---

## Phase 4: Post-Cutover Monitoring (24 hours)

### 14. Continuous Monitoring

```bash
# Start 24-hour soak monitor on production port
nohup /home/eacio/sandstar/bin/soak-monitor.sh --duration 24h --interval 60 \
    > /dev/null 2>&1 &
echo "Production soak PID: $!"

# Monitor logs live (from SSH session)
journalctl -u sandstar-engine -f

# Check periodically
tail -20 /var/log/sandstar/soak-monitor.log
cat /var/log/sandstar/soak-alerts.log 2>/dev/null || echo "No alerts"
```

### 15. Success Declaration

The deployment is considered successful when ALL of the following are true:

- [ ] 24-hour post-cutover soak with 0 alerts
- [ ] RSS memory < 30MB steady-state, growth < 1MB/day
- [ ] CPU usage < 5% steady-state
- [ ] 0 systemd restarts (NRestarts=0)
- [ ] 0 panics, 0 OOM kills
- [ ] All 138 channels reading with status=Ok
- [ ] REST API responds to all 14 endpoints
- [ ] History data accumulating correctly

```bash
# Final verification commands
systemctl show sandstar-engine --property=NRestarts           # Must be 0
ps -o pid,rss,vsz,comm -p $(pgrep -f sandstar-engine-server) # RSS < 30MB
curl -sf http://localhost:8085/api/status | python3 -m json.tool
dmesg | grep -ci "out of memory\|killed process"              # Must be 0
```

---

## Emergency Rollback

### If anything goes wrong

```bash
# Run the rollback script (restores C engine, optionally restarts Rust in validation mode)
echo "$SANDSTAR_SUDO_PASS" | sudo -S /home/eacio/sandstar/bin/rollback-to-c.sh
```

What the script does:
1. Stops Rust engine (sandstar-engine.service)
2. Starts C engine (sandstar.service) on port 8085
3. Optionally restarts Rust in validation mode on port 8086

For unattended rollback:
```bash
echo "$SANDSTAR_SUDO_PASS" | sudo -S /home/eacio/sandstar/bin/rollback-to-c.sh --force
```

To rollback WITHOUT restarting Rust in validation mode:
```bash
echo "$SANDSTAR_SUDO_PASS" | sudo -S /home/eacio/sandstar/bin/rollback-to-c.sh --force --no-validate
```

### Verification After Rollback

```bash
# C engine running
systemctl status sandstar
curl -sf http://localhost:8085/api/status | python3 -c "
import json, sys
s = json.load(sys.stdin)
print(f'Channels: {s[\"channelCount\"]}')
print(f'Tables: {s[\"tableCount\"]}')
"

# Rust engine stopped (or in validation mode on 8086)
systemctl status sandstar-engine 2>/dev/null || echo "Rust production: stopped (expected)"
systemctl status sandstar-rust-validate 2>/dev/null || echo "Rust validation: stopped"
```

### Full Rust Removal (if needed)

```bash
echo "$SANDSTAR_SUDO_PASS" | sudo -S bash -c '
systemctl stop sandstar-rust-validate 2>/dev/null
systemctl stop sandstar-engine 2>/dev/null
systemctl disable sandstar-rust-validate 2>/dev/null
systemctl disable sandstar-engine 2>/dev/null
rm -f /etc/systemd/system/sandstar-rust-validate.service
rm -f /etc/systemd/system/sandstar-engine.service
systemctl daemon-reload
rm -f /home/eacio/sandstar/bin/sandstar-engine-server
rm -f /home/eacio/sandstar/bin/sandstar-cli
rm -f /var/log/sandstar/sandstar-rust.log
rm -rf /tmp/sandstar_validate/
'
```

---

## Quick Reference

| Action | Command |
|--------|---------|
| Build ARM .deb | `rtk cargo arm-build && rtk cargo arm-deb` |
| Deploy to device | `/home/parallels/code/ssCompile/tools/installSandstar.sh BahaHost2Device` |
| SSH to device | `ssh -J solidyne@172.28.109.221 -p 1919 eacio@172.28.211.135` |
| Start read-only | `sudo systemctl start sandstar-rust-validate` |
| Start soak monitor | `/home/eacio/sandstar/bin/soak-monitor.sh --duration 8h` |
| Run validation comparison | `/home/eacio/sandstar/bin/validate-engines.sh localhost` |
| Execute cutover | `sudo /home/eacio/sandstar/bin/cutover-to-rust.sh` |
| Emergency rollback | `sudo /home/eacio/sandstar/bin/rollback-to-c.sh` |
| Check health (C) | `curl -sf http://localhost:8085/api/status` |
| Check health (Rust) | `curl -sf http://localhost:8086/api/status` |
| Check health (remote) | `curl -sf http://172.28.211.135:8085/api/status` |
| Check match rate | `/home/eacio/sandstar/bin/validate-engines.sh localhost 1 5` |
| View soak alerts | `cat /var/log/sandstar/soak-alerts.log` |
| View engine logs | `journalctl -u sandstar-engine -f` |
| Check memory | `ps -o pid,rss,vsz,comm -p $(pgrep -f sandstar-engine-server)` |
| Check restarts | `systemctl show sandstar-engine --property=NRestarts` |

---

## File Locations on BeagleBone

| File | Path |
|------|------|
| Rust binary | `/home/eacio/sandstar/bin/sandstar-engine-server` |
| CLI binary | `/home/eacio/sandstar/bin/sandstar-cli` |
| Production service | `/etc/systemd/system/sandstar-engine.service` |
| Validation service | `/etc/systemd/system/sandstar-rust-validate.service` |
| Engine log | `/var/log/sandstar/sandstar-rust.log` |
| Soak monitor log | `/var/log/sandstar/soak-monitor.log` |
| Soak alert log | `/var/log/sandstar/soak-alerts.log` |
| Config dir | `/home/eacio/sandstar/etc/EacIo/` |
| Lookup tables | `/home/eacio/sandstar/etc/config/` |
| Validation logs | `/tmp/sandstar_validate/` |
| Backup (cutover) | `/home/eacio/sandstar/backup/cutover-TIMESTAMP/` |

---

## Port Assignments

| Port | Service | Mode |
|------|---------|------|
| 8085 | Production engine | C engine (pre-cutover) or Rust engine (post-cutover) |
| 8086 | Rust validation | Read-only, side-by-side with C engine |
| 9813 | IPC (TCP) | Windows only |
| `/tmp/sandstar-engine.sock` | IPC (Unix) | C engine |
| `/tmp/sandstar-rust.sock` | IPC (Unix) | Rust validation |

---

## Related Documentation

- [Validation Runbook](../tools/validation-runbook.md) — detailed manual test procedures
- [Roadmap v2](ROADMAP_v2.md) — full feature roadmap and phase status
