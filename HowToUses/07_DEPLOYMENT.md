# ARM Cross-Compile & BeagleBone Deployment

## Prerequisites

- Rust 1.93+ with `armv7-unknown-linux-gnueabihf` target
- Zig compiler (via `pip install ziglang`)
- Zig CC wrappers at `C:\czb\` (zigcc-arm.bat, zigcc-arm-cc.bat, zigar-arm.bat)

## Cross-Compile

### Set Environment
```bash
export PATH="$HOME/.cargo/bin:$PATH"
export CC_armv7_unknown_linux_gnueabihf="C:\\czb\\zigcc-arm-cc.bat"
export AR_armv7_unknown_linux_gnueabihf="C:\\czb\\zigar-arm.bat"
```

### Build ARM Binary
```bash
# Pure Rust (no Sedona VM)
rtk cargo arm-build

# With Sedona VM C code
rtk cargo arm-build-svm
```

### Package as .deb
```bash
rtk cargo arm-deb --no-strip
# Output: target/debian/sandstar_1.6.0-1_armhf.deb
```

## Deploy to BeagleBone

### Todd Air Flow (192.168.1.3)
```bash
# Transfer
scp -P 1919 -o StrictHostKeyChecking=no \
  target/debian/sandstar_1.6.0-1_armhf.deb \
  eacio@192.168.1.3:/home/eacio/

# Install (stops service, installs, restarts)
ssh -p 1919 eacio@192.168.1.3 \
  "echo 'PASSWORD' | sudo -S dpkg -i /home/eacio/sandstar_1.6.0-1_armhf.deb"

# Verify
ssh -p 1919 eacio@192.168.1.3 \
  "echo 'PASSWORD' | sudo -S systemctl status sandstar-engine.service"
```

### Using Deploy Script
```bash
SANDSTAR_SUDO_PASS="PASSWORD" bash tools/installSandstarRust.sh 30-113
```

### Validate Deployment
```bash
bash tools/validate-engines.sh 192.168.1.3
```

## Systemd Service

Located at `/etc/systemd/system/sandstar-engine.service`:
```ini
[Service]
Type=simple
ExecStart=/home/eacio/sandstar/bin/sandstar-engine-server \
    --config-dir /home/eacio/sandstar/etc/EacIo \
    --log-file /var/log/sandstar/sandstar-engine.log \
    --log-level info --http-bind 0.0.0.0 --sox
User=eacio
Group=root
MemoryLimit=128M
Restart=on-failure
```

### Service Commands (on device)
```bash
sudo systemctl start sandstar-engine
sudo systemctl stop sandstar-engine
sudo systemctl restart sandstar-engine
sudo systemctl status sandstar-engine
journalctl -u sandstar-engine -f
```

## Key Paths on BeagleBone

| Path | Purpose |
|------|---------|
| `/home/eacio/sandstar/bin/` | Binaries |
| `/home/eacio/sandstar/etc/EacIo/` | Config (points.csv, tables.csv) |
| `/home/eacio/sandstar/etc/config/` | Lookup tables (*.txt) |
| `/home/eacio/sandstar/etc/manifests/` | Kit manifest XMLs |
| `/var/log/sandstar/` | Log files |
| `/home/eacio/sandstar/etc/config/sox_components.json` | Persisted SOX components |

## BeagleBone Devices

| Device | IP | Port | User |
|--------|-----|------|------|
| Todd Air Flow (30-113) | 192.168.1.3 | 1919 | eacio |
| Baha (211-135) | 10.1.10.229 | 1919 | eacio (via jump host) |
