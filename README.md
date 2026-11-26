# Sandstar Engine (Rust)

Embedded IoT control engine for BeagleBone, replacing a C/C++ system.

**Version:** 2.10.0 (released, tag `v2.10.0`; `develop` carries unreleased changes, see [`CHANGELOG.md`](CHANGELOG.md)) · **Production:** one field BeagleBone running 2.10.0 (local I/O + control loop; the BACnet and MQTT drivers are available but not configured on it) · **Tests:** 3,147 passing on `develop`, 0 clippy warnings · **Dependencies:** 0 C/C++ (pure Rust, no FFI)

## Overview

Sandstar is a channel-based hardware I/O abstraction layer for HVAC control. It reads sensors, applies conversions via lookup tables, runs control logic (PID loops, sequencers), drives actuators, speaks to industrial-control peers over BACnet/IP and MQTT, and exposes everything through Project Haystack REST + WebSocket APIs. The Sedona VM runs in-process as pure Rust bytecode.

- **Target:** BeagleBone Black (ARM Cortex-A8, 512 MB RAM, Debian Linux)
- **Replaces:** ~27,000 lines C/C++ engine + 6,839 lines C SVM + 500,000 lines POCO framework
- **Result:** ~125,000 lines of Rust including tests (server ~87,000, Sedona VM ~21,000, engine ~11,000), zero C sources, statically-linked ARM binary (no `.so` dependencies)
- **Observability:** Prometheus `/metrics` endpoint with 18 counters + 5 gauges
- **Clustering:** roxWarp gossip protocol with mDNS peer discovery (opt-in)

## Architecture

```
crates/
  sandstar-engine/     # Core: channels, tables, conversions, filters, polls, watches, PID, sequencer
  sandstar-hal/        # HAL trait definitions + MockHal for testing
  sandstar-hal-linux/  # Linux sysfs drivers: GPIO, ADC, I2C, PWM, UART
  sandstar-ipc/        # IPC wire protocol (length-prefixed bincode over TCP/Unix socket)
  sandstar-server/     # HTTP/WS server, REST, SOX/DASP, driver framework, roxWarp, control engine
  sandstar-cli/        # CLI (clap): status, channels, read, write, reload, history, convert-sax
  sandstar-svm/        # Pure Rust Sedona VM (240 opcodes, 131 native methods, no C/FFI)
```

All crates share the workspace version and `edition = "2021"`.

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

# ARM cross-compile for BeagleBone (Zig CC toolchain; no Docker required)
export CC_armv7_unknown_linux_gnueabihf="C:\\czb\\zigcc-arm-cc.bat"
export AR_armv7_unknown_linux_gnueabihf="C:\\czb\\zigar-arm.bat"
cargo arm-build                  # alias defined in .cargo/config.toml
cargo arm-deb --no-strip         # packages as target/debian/sandstar_<ver>-1_armhf.deb

# With TLS support
cargo build -p sandstar-server --features tls
```

See [`docs/Progress/DEPLOYMENT_CHECKLIST.md`](docs/Progress/DEPLOYMENT_CHECKLIST.md) for the full rollout runbook.

## Configuration

### Config directory

Set via `--config-dir` or `SANDSTAR_CONFIG_DIR`; contains:

- `points.csv` — channel definitions (channel number, I/O type, conversion params)
- `tables.csv` — lookup table mappings (tag to file path)
- `database.zinc` — Haystack point database with tags
- `control.toml` — PID loops, sequencers, components (optional)

### Environment variables for optional drivers

- `SANDSTAR_BACNET_CONFIGS` — JSON array of `BacnetConfig`; see [`docs/Progress/BACNET_SETUP.md`](docs/Progress/BACNET_SETUP.md)
- `SANDSTAR_MQTT_CONFIGS` — JSON array of `MqttConfig`; see [`docs/Progress/MQTT_SETUP.md`](docs/Progress/MQTT_SETUP.md)

### Server flags

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
| `--cluster` | | false | Enable roxWarp gossip clustering (see `--cluster-config`) |
| `--cluster-config` | | (none) | Path to `ClusterConfig` JSON (peers, mTLS, mDNS) |
| `--cluster-insecure` | | false | Allow the cluster over plain WebSocket, no peer auth, when no mTLS certificates are given. Without it the cluster network stays off |
| `--tls-cert` | `SANDSTAR_TLS_CERT` | (none) | TLS certificate (PEM) |
| `--tls-key` | `SANDSTAR_TLS_KEY` | (none) | TLS private key (PEM) |
| `--sedona` | | false | Enable Sedona VM |
| `--scode-path` | `SANDSTAR_SCODE_PATH` | (none) | Sedona scode image path |
| `--log-level` | `RUST_LOG` | info | Log level filter |
| `--log-file` | `SANDSTAR_LOG_FILE` | (none) | Log file path |
| `--socket`, `-s` | `SANDSTAR_SOCKET` | (platform default) | IPC socket (UDS on Linux, TCP on Windows) |
| `--no-rest` | | false | Disable REST API |
| `--no-pid-file` | | false | Skip PID file creation |

## REST API

Core Haystack + Sandstar endpoints:

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/about` | GET | Server metadata |
| `/api/ops` | GET | Available operations |
| `/api/read` | GET | Read channels by id or filter (supports dynamic tag filters) |
| `/api/status` | GET | Engine status |
| `/api/channels` | GET | List all channels |
| `/api/polls` | GET | List poll groups |
| `/api/tables` | GET | List lookup tables |
| `/api/pointWrite` | GET/POST | Read priority array / write value at priority level |
| `/api/watchSub` | POST | Subscribe to channel changes |
| `/api/watchPoll` | POST | Poll for changes |
| `/api/watchUnsub` | POST | Unsubscribe |
| `/api/history/{channel}` | GET | Historical data |
| `/api/pollNow` | POST | Trigger immediate poll |
| `/api/reload` | POST | Reload configuration and, if it changed, `control.toml` (error reply and nothing changed if a configuration file cannot be read) |
| `/api/auth` | POST | SCRAM-SHA-256 authentication |
| `/api/metrics` | GET | Internal metrics as JSON |
| `/metrics` | GET | **Prometheus exposition format** (scraper-friendly) |
| `/api/ws` | GET | Haystack WebSocket upgrade (sub-second COV push via driver CovEvent bridge) |
| `/health` | GET | Health check |

Driver framework (Phase 12.0G — runtime driver lifecycle, auth-gated):

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/drivers` | GET | List registered drivers |
| `/api/drivers` | POST | Create a driver from `{driver_type, config}` JSON |
| `/api/drivers/{id}` | DELETE | Deregister + close a driver |
| `/api/drivers/{id}/status` | GET | Driver + per-point status |
| `/api/drivers/{id}/learn` | GET | Discover points |
| `/api/drivers/{id}/open` | POST | Bring the driver up |
| `/api/drivers/{id}/close` | POST | Shut down without removing |
| `/api/drivers/{id}/ping` | POST | Health check |
| `/api/drivers/{id}/write` | POST | Batch write values |
| `/api/drivers/{id}/message` | POST | Driver-specific custom message (12.0E `on_receive`) |
| `/api/syncCur` | POST | Batch read across drivers |

Dynamic slots / tags (Phase 13):

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/tags/{comp_id}` | GET/PUT/DELETE | Read / merge+replace / delete dynamic tags on a component |

Visual-editor SOX REST (Phase 14.0A — 11 endpoints):

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/sox/tree` | GET | Full component tree |
| `/api/sox/comp/{id}` | GET | Component detail |
| `/api/sox/palette` | GET | Available component types from manifest |
| `/api/sox/names` | GET | Interned name table stats |
| `/api/sox/comp` | POST | Add a component |
| `/api/sox/comp/{id}` | DELETE | Remove a component |
| `/api/sox/comp/{id}/name` | PUT | Rename |
| `/api/sox/comp/{id}/slot/{idx}` | PUT | Write slot value |
| `/api/sox/comp/{id}/invoke/{slot}` | POST | Invoke an action slot |
| `/api/sox/comp/{id}/pos` | PUT | Update editor position |
| `/api/sox/comp/{id}/reorder` | PUT | Reorder children |
| `/api/sox/link` | POST/DELETE | Add / remove a link between slots |

Cluster + SOX WebSocket:

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/rows` | GET | RoWS (SOX-over-WebSocket) session — tree, slot, link, readTags/setTags/deleteTag |
| `/roxwarp` | GET | roxWarp cluster peer WebSocket (only with `--cluster --cluster-insecure`; with mTLS peers use the cluster port) |

All endpoints support JSON by default; `Accept: text/zinc` returns Haystack Zinc. Per-endpoint details in [`docs/Progress/DRIVER_REST_API.md`](docs/Progress/DRIVER_REST_API.md).

## Control Engine

Built-in, Sedona-free control primitives:

- **PID controllers** with anti-windup, max delta, direct/reverse action
- **Lead sequencers** with N-stage hysteresis
- **35 built-in components:** arithmetic (Add, Sub, Mul, Div, Min, Max, Avg), logic (And, Or, Not, Mux), timing (Delay, Ramp, Tpd, Ewma), HVAC (Deadband, Economizer, Tstat), scheduling (WeeklySchedule, HolidaySchedule), constants
- **TOML configuration** (`control.toml`) with `[[loop]]`, `[[sequencer]]`, `[[component]]` sections
- **Channel-to-logic bridge** — rename a `ConstFloat` to `"chXXXX"` (e.g. `ch1713`) to proxy live sensor data into the component graph

Convert legacy Sedona `.sax` files:

```bash
cargo run -p sandstar-cli -- convert-sax app.sax --output control.toml
```

## Drivers

Network drivers (Project B / Project C / Phase 12):

- **BACnet/IP** — discovery, ReadProperty, WriteProperty, RPM batching, SubscribeCOV with renewal, BBMD foreign-device registration
- **MQTT v3.1.1** — subscribe with value caching, publish on write (plain `f64` or `{"value":N}` envelope)
- **LocalIoDriver** — engine-channel façade so local GPIO/ADC/I2C/PWM points are queryable through the same `/api/drivers` surface

All async drivers share a generic loader (Phase 12.0A), `PointStatus::Remote*` per-point error reporting (12.0B), `SyncContext`/`WriteContext` callback API (12.0C), `CovEvent` broadcast channel (12.0D), WebSocket push bridge (12.0D.WS), custom `on_receive` messaging (12.0E), and runtime REST lifecycle (12.0G).

## Clustering (roxWarp)

Multi-device gossip protocol for distributed state:

- **Binary Trio diff** over WebSocket (optionally WSS + mTLS)
- **Version vectors** with LWW conflict resolution
- **mDNS peer discovery** (opt-in, `ClusterConfig.enable_mdns`)
- **Static peer list** (back-compat; both can coexist)
- **Fantom pod for SkySpark** — external project (not in this repo)

Protocol spec in [`docs/research/16_ROXWARP_PROTOCOL.md`](docs/research/16_ROXWARP_PROTOCOL.md).

## Security

- **Bind address:** 127.0.0.1 by default (loopback only)
- **Bearer-token auth:** protects POST endpoints (`--auth-token`)
- **SCRAM-SHA-256:** RFC 5802 challenge-response auth (`--auth-user` / `--auth-pass`)
- **TLS:** optional via rustls (`--tls-cert` / `--tls-key`)
- **mTLS** for cluster peers (`ClusterConfig.{cert_path, key_path, ca_path}`)
- **Rate limiting:** 100 req/s default, configurable (`--rate-limit`)
- **CORS:** restricted method/header whitelist
- **Filter depth limit:** max 32 nested expressions
- **Watch cap:** max 64 subscriptions, 256 channels per watch
- **Path sanitization:** config paths canonicalized before use
- **Vulnerability reporting:** see [`SECURITY.md`](SECURITY.md)

## Deployment

### Systemd services

- `sandstar-engine.service` — production service

### Scripts

| Script | Purpose |
|--------|---------|
| `tools/soak-monitor.sh` | Long-running check of the service on the device (shipped in the `.deb`) |
| `tools/health-monitor.sh` | Cron health line every 5 minutes (shipped in the `.deb`) |
| `tools/kaldirilanlar/` | Retired C-era and early deploy scripts; not used, not packaged |
| `tools/bacnet_sim.py` | Hand-crafted BACnet/IP simulator for driver validation |
| `tools/ws_latency_test.py` | Live validator for the 12.0D.WS push bridge |
| `tools/gates.sh` | Quality gates in one command: fmt, clippy, tests, ARM feature check |

## Testing

```bash
# All tests
cargo test --workspace

# Individual crates
cargo test -p sandstar-engine
cargo test -p sandstar-server
# etc.

# Ignored (multicast / live-hardware) tests
cargo test -- --ignored
```

3,147 tests across the workspace on `develop` (3,117 in the 2.10.0 release). Platform-specific tests (sysfs GPIO, I2C, PWM) are gated behind `#[cfg(target_os = "linux")]`. Live-network tests (mDNS multicast) are marked `#[ignore]` and require a multicast-capable host.

## Development

- **Branches:** `master` is the stable branch; field builds are made from it. `develop` is the integration branch. Each change is made on its own branch cut from `develop` and merged through a pull request; both branches require the CI `Test` job.
- **Before a pull request:** `bash tools/gates.sh` runs `cargo fmt --check`, `cargo clippy --workspace -D warnings`, `cargo test --workspace --no-fail-fast` and a check of the ARM feature set (`linux-hal,svm`). It prints one line per gate and exits non-zero if one fails.
- **Toolchain:** pinned in `rust-toolchain.toml`; the minimum supported Rust version is 1.82.
- **Device and site credentials** are not in the repository.

## Documentation

- [`docs/Progress/OVERVIEW.md`](docs/Progress/OVERVIEW.md) — project status + doc map
- [`docs/Progress/ROADMAP_v2.md`](docs/Progress/ROADMAP_v2.md) — full roadmap, master status tracker, research-doc coverage
- [`docs/Progress/DRIVER_REST_API.md`](docs/Progress/DRIVER_REST_API.md) — operator + integrator guide for the REST surface
- [`docs/Progress/BACNET_SETUP.md`](docs/Progress/BACNET_SETUP.md), [`docs/Progress/MQTT_SETUP.md`](docs/Progress/MQTT_SETUP.md) — driver enablement
- [`docs/Progress/DEPLOYMENT_CHECKLIST.md`](docs/Progress/DEPLOYMENT_CHECKLIST.md) — rollout runbook: backup, install, verification
- [`docs/Progress/HARDCODED_LIMITS.md`](docs/Progress/HARDCODED_LIMITS.md) — constants and limits, with where they live
- [`CHANGELOG.md`](CHANGELOG.md) — per-version highlights and behavior changes to read before upgrading
- [`SECURITY.md`](SECURITY.md) — supported versions and vulnerability reporting
- `docs/research/` — research documents from the original migration analysis, each with a status block (done, done differently, open)

## License

Licensed under the Academic Free License version 3.0. Full text in [`LICENSE`](LICENSE).
