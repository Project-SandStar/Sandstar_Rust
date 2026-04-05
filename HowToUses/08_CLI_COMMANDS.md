# CLI Commands (sandstar-cli)

## Overview
The CLI communicates with the running engine via IPC (TCP on Windows, Unix socket on Linux).

## Usage
```bash
cargo run -p sandstar-cli -- <command>
```

## Commands

### Status
```bash
# Engine status (uptime, channel count, poll count)
cargo run -p sandstar-cli -- status

# JSON output
cargo run -p sandstar-cli -- --json status
```

### Channels
```bash
# List all channels
cargo run -p sandstar-cli -- channels

# Read single channel
cargo run -p sandstar-cli -- read 1113

# Write to channel
cargo run -p sandstar-cli -- write 360 1.0
```

### Polls
```bash
# List poll schedule
cargo run -p sandstar-cli -- polls
```

### Tables
```bash
# List lookup tables
cargo run -p sandstar-cli -- tables
```

### Engine Control
```bash
# Reload configuration
cargo run -p sandstar-cli -- reload

# Graceful shutdown
cargo run -p sandstar-cli -- shutdown
```

## JSON Output Mode
Add `--json` before the command for machine-readable output:
```bash
cargo run -p sandstar-cli -- --json status
cargo run -p sandstar-cli -- --json channels
```

## IPC Connection
- **Windows**: TCP `127.0.0.1:9813`
- **Linux**: Unix domain socket `/var/run/sandstar/sandstar-engine.sock`
- Protocol: Length-prefixed bincode over TCP
