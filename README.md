# Sandstar Engine (Rust)

Embedded IoT control engine for BeagleBone, replacing a C/C++ system.

**Version:** 2.0.0

## Overview

Sandstar is a channel-based hardware I/O abstraction layer for HVAC control. It reads sensors, applies conversions via lookup tables, runs control logic (PID loops, sequencers), and drives actuators. The system follows the Project Haystack data model.

- **Target:** BeagleBone Black (ARM Cortex-A8, 512MB RAM, Debian Linux)
- **Replaces:** ~27,000 lines C/C++ engine + 500,000 lines POCO framework
- **Result:** ~40,000+ lines pure Rust (no C code), 1,637 tests, 0 critical security issues

## Architecture

```
crates/
  sandstar-engine/     # Core: channels, tables, conversions, filters, polls, watches, PID, sequencer
  sandstar-hal/        # HAL trait definitions + MockHal for testing
  sandstar-hal-linux/  # Linux sysfs drivers: GPIO, ADC, I2C, PWM, UART
  sandstar-ipc/        # IPC wire protocol (length-prefixed bincode over TCP/Unix socket)
  sandstar-server/     # HTTP server (Axum), WebSocket, REST API, control engine, config loading
  sandstar-cli/        # CLI tool (clap): status, channels, read, write, reload, history, convert-sax
  sandstar-svm/        # Pure Rust Sedona VM interpreter (240 opcodes, 131 native methods, no C/FFI)
```

All crates share `version = "2.0.0"` and `edition = "2021"` via workspace configuration.

## Quick Start

```bash
# Demo mode (5 channels, MockHal)
cargo run -p sandstar-server

# With real config
SANDSTAR_CONFIG_DIR="/path/to/EacIo" cargo run -p sandstar-server

# CLI
cargo run -p sandstar-cli -- status
cargo run -p sandstar-cli -- channels
cargo run -p sandstar-cli -- read 1113
```

## Building

```bash
# Development (Windows/Linux/macOS)
cargo build --workspace
cargo test --workspace

# ARM cross-compile for BeagleBone
cargo zigbuild --target armv7-unknown-linux-gnueabihf --release -p sandstar-server
cargo zigbuild --target armv7-unknown-linux-gnueabihf --release -p sandstar-cli

# Package as .deb
cargo deb -p sandstar-server --target armv7-unknown-linux-gnueabihf

# With TLS support
cargo build -p sandstar-server --features tls
```

## Configuration

### Config Directory

The config directory (set via `--config-dir` or `SANDSTAR_CONFIG_DIR`) contains:

- `points.csv` -- channel definitions (channel number, I/O type, conversion params)
- `tables.csv` -- lookup table mappings (tag to file path)
- `database.zinc` -- Haystack point database with tags
- `control.toml` -- PID loops, sequencers, and components (optional)

### Server Flags

| Flag | Env Var | Default | Description |
|------|---------|---------|-------------|
| `--config-dir`, `-c` | `SANDSTAR_CONFIG_DIR` | (demo mode) | Config directory path |
| `--http-port` | `SANDSTAR_HTTP_PORT` | 8085 | HTTP listen port |
| `--http-bind` | `SANDSTAR_HTTP_BIND` | 127.0.0.1 | HTTP bind address |
| `--poll-interval-ms`, `-p` | `SANDSTAR_POLL_INTERVAL_MS` | 1000 | Poll cycle interval |
| `--auth-token` | `SANDSTAR_AUTH_TOKEN` | (none) | Bearer token for POST endpoints |
| `--auth-user` | `SANDSTAR_AUTH_USER` | (none) | SCRAM-SHA-256 username |
| `--auth-pass` | `SANDSTAR_AUTH_PASS` | (none) | SCRAM-SHA-256 password |
| `--rate-limit` | `SANDSTAR_RATE_LIMIT` | 100 | Max requests/sec (0 = unlimited) |
| `--read-only` | | false | Reject output writes (validation mode) |
| `--no-control` | | false | Disable PID/sequencer engine |
| `--control-config` | `SANDSTAR_CONTROL_CONFIG` | (auto) | Control config file path |
| `--tls-cert` | `SANDSTAR_TLS_CERT` | (none) | TLS certificate (PEM) |
| `--tls-key` | `SANDSTAR_TLS_KEY` | (none) | TLS private key (PEM) |
| `--log-level` | `RUST_LOG` | info | Log level filter |
| `--log-file` | `SANDSTAR_LOG_FILE` | (none) | Log file path |
| `--socket`, `-s` | `SANDSTAR_SOCKET` | Unix socket or 127.0.0.1:9813 | IPC socket |
| `--sedona` | | false | Enable Sedona VM |
| `--scode-path` | `SANDSTAR_SCODE_PATH` | (none) | Sedona scode image path |
| `--no-rest` | | false | Disable REST API |
| `--no-pid-file` | | false | Skip PID file creation |

## REST API

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/about` | GET | Server metadata |
| `/api/ops` | GET | Available operations |
| `/api/read` | GET | Read channels by id or filter |
| `/api/status` | GET | Engine status |
| `/api/channels` | GET | List all channels |
| `/api/polls` | GET | List poll groups |
| `/api/tables` | GET | List lookup tables |
| `/api/pointWrite` | GET | Read priority array for a point |
| `/api/pointWrite` | POST | Write value at priority level |
| `/api/watchSub` | POST | Subscribe to channel changes |
| `/api/watchPoll` | POST | Poll for changes |
| `/api/watchUnsub` | POST | Unsubscribe |
| `/api/history/{channel}` | GET | Historical data |
| `/api/pollNow` | POST | Trigger immediate poll |
| `/api/reload` | POST | Reload configuration |
| `/api/auth` | POST | SCRAM-SHA-256 authentication |
| `/api/metrics` | GET | Runtime metrics |
| `/api/ws` | GET | WebSocket upgrade |
| `/health` | GET | Health check |

All endpoints support JSON and Zinc wire format (via `Accept: text/zinc` header).

## Control Engine

The built-in control engine replaces the Sedona VM for standard HVAC applications:

- **PID controllers** with anti-windup, max delta, direct/reverse action
- **Lead sequencers** with N-stage hysteresis
- **20 built-in components:** arithmetic (Add, Sub, Mul, Div, Min, Max, Avg), logic (And, Or, Not, Mux), timing (Delay, Ramp, Tpd, Ewma), HVAC (Deadband, Economizer), scheduling (WeeklySchedule, HolidaySchedule), constants
- **TOML configuration** (`control.toml`) with `[[loop]]`, `[[sequencer]]`, and `[[component]]` sections

Convert legacy Sedona `.sax` files:

```bash
cargo run -p sandstar-cli -- convert-sax app.sax --output control.toml
```

## Security

- **Bind address:** 127.0.0.1 by default (loopback only)
- **Bearer token auth:** protects POST endpoints (`--auth-token`)
- **SCRAM-SHA-256:** RFC 5802 challenge-response auth (`--auth-user`/`--auth-pass`)
- **TLS:** optional via rustls (`--tls-cert`/`--tls-key`, requires `tls` feature)
- **Rate limiting:** 100 req/s default, configurable (`--rate-limit`)
- **CORS:** restricted method/header whitelist
- **Filter depth limit:** max 32 nested expressions
- **Watch cap:** max 64 subscriptions, 256 channels per watch
- **Path sanitization:** config paths canonicalized before use

## Deployment

### Systemd Services

- `sandstar-engine.service` -- production service
- `sandstar-rust-validate.service` -- read-only validation alongside C engine

### Scripts

| Script | Purpose |
|--------|---------|
| `tools/installSandstar.sh` | Deploy .deb to device |
| `tools/validate-engines.sh` | Compare Rust vs C engine output |
| `tools/soak-monitor.sh` | Automated health monitoring (memory, match rate, errors) |
| `tools/cutover-to-rust.sh` | Production switch from C to Rust |
| `tools/rollback-to-c.sh` | Rollback from Rust to C |

## Testing

```bash
# All tests
cargo test --workspace

# Individual crates
cargo test -p sandstar-engine
cargo test -p sandstar-server
cargo test -p sandstar-hal
cargo test -p sandstar-hal-linux
cargo test -p sandstar-ipc
cargo test -p sandstar-cli
cargo test -p sandstar-svm
```

1,637 tests across all crates. Platform-specific tests (sysfs GPIO, I2C, PWM) are gated behind `#[cfg(target_os = "linux")]`.

## License

Proprietary.
