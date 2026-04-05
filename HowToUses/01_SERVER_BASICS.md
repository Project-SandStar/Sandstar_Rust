# Server Basics

## Starting the Server

### Demo Mode (no hardware, mock channels)
```bash
cargo run -p sandstar-server
```
Starts with 5 demo channels on `http://localhost:8080`.

### With SOX Protocol (Sedona editor support)
```bash
cargo run -p sandstar-server -- --sox
```
Enables SOX/DASP on UDP port 1876 for the Sedona Application Editor.

### With Real Config (production channels)
```bash
SANDSTAR_CONFIG_DIR="/path/to/EacIo" cargo run -p sandstar-server
```
Loads channels from `points.csv`, tables from `tables.csv`, database from `database.zinc`.

### Full Production (BeagleBone)
```bash
sandstar-engine-server \
    --config-dir /home/eacio/sandstar/etc/EacIo \
    --log-file /var/log/sandstar/sandstar-engine.log \
    --log-level info \
    --http-bind 0.0.0.0 \
    --sox
```

## Common CLI Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--config-dir <path>` | `.` | Config directory (points.csv, tables.csv) |
| `--http-bind <addr>` | `127.0.0.1` | HTTP bind address |
| `--http-port <port>` | `8085` | HTTP port |
| `--sox` | off | Enable SOX/DASP protocol on UDP 1876 |
| `--sox-port <port>` | `1876` | SOX UDP port |
| `--log-file <path>` | stdout | Log file path |
| `--log-level <level>` | `info` | Log level (trace, debug, info, warn, error) |
| `--pid-file <path>` | none | PID file path |
| `--read-only` | off | Disable write operations |

## Ports

| Port | Protocol | Service |
|------|----------|---------|
| 8085 | HTTP | REST API, Dashboard, Editor, RoWS WebSocket |
| 1876 | UDP | SOX/DASP (Sedona Application Editor) |
| 7443 | TCP/WSS | roxWarp cluster gossip (when --cluster enabled) |
