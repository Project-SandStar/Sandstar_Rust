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

Academic Free License ("AFL") v. 3.0 This Academic Free License (the "License")
applies to any original work of authorship (the "Original Work") whose owner
(the "Licensor") has placed the following licensing notice adjacent to the
copyright notice for the Original Work:

Licensed under the Academic Free License version 3.0

1) Grant of Copyright License. Licensor grants You a worldwide, royalty-free,
non-exclusive, sublicensable license, for the duration of the copyright, to do
the following:

a) to reproduce the Original Work in copies, either alone or as part of a
collective work;

b) to translate, adapt, alter, transform, modify, or arrange the Original Work,
thereby creating derivative works ("Derivative Works") based upon the Original
Work;

c) to distribute or communicate copies of the Original Work and Derivative
Works to the public, under any license of your choice that does not contradict
the terms and conditions, including Licensor's reserved rights and remedies, in
this Academic Free License;

d) to perform the Original Work publicly; and

e) to display the Original Work publicly.

2) Grant of Patent License. Licensor grants You a worldwide, royalty-free,
non-exclusive, sublicensable license, under patent claims owned or controlled
by the Licensor that are embodied in the Original Work as furnished by the
Licensor, for the duration of the patents, to make, use, sell, offer for sale,
have made, and import the Original Work and Derivative Works.

3) Grant of Source Code License. The term "Source Code" means the preferred
form of the Original Work for making modifications to it and all available
documentation describing how to modify the Original Work. Licensor agrees to
provide a machine-readable copy of the Source Code of the Original Work along
with each copy of the Original Work that Licensor distributes. Licensor
reserves the right to satisfy this obligation by placing a machine-readable
copy of the Source Code in an information repository reasonably calculated to
permit inexpensive and convenient access by You for as long as Licensor
continues to distribute the Original Work.

4) Exclusions From License Grant. Neither the names of Licensor, nor the names
of any contributors to the Original Work, nor any of their trademarks or
service marks, may be used to endorse or promote products derived from this
Original Work without express prior permission of the Licensor. Except as
expressly stated herein, nothing in this License grants any license to
Licensor's trademarks, copyrights, patents, trade secrets or any other
intellectual property. No patent license is granted to make, use, sell, offer
for sale, have made, or import embodiments of any patent claims other than the
licensed claims defined in Section 2. No license is granted to the trademarks
of Licensor even if such marks are included in the Original Work. Nothing in
this License shall be interpreted to prohibit Licensor from licensing under
terms different from this License any Original Work that Licensor otherwise
would have a right to license.

5) External Deployment. The term "External Deployment" means the use,
distribution, or communication of the Original Work or Derivative Works in any
way such that the Original Work or Derivative Works may be used by anyone other
than You, whether those works are distributed or communicated to those persons
or made available as an application intended for use over a network. As an
express condition for the grants of license hereunder, You must treat any
External Deployment by You of the Original Work or a Derivative Work as a
distribution under section 1(c).

6) Attribution Rights. You must retain, in the Source Code of any Derivative
Works that You create, all copyright, patent, or trademark notices from the
Source Code of the Original Work, as well as any notices of licensing and any
descriptive text identified therein as an "Attribution Notice." You must cause
the Source Code for any Derivative Works that You create to carry a prominent
Attribution Notice reasonably calculated to inform recipients that You have
modified the Original Work.

7) Warranty of Provenance and Disclaimer of Warranty. Licensor warrants that
the copyright in and to the Original Work and the patent rights granted herein
by Licensor are owned by the Licensor or are sublicensed to You under the terms
of this License with the permission of the contributor(s) of those copyrights
and patent rights. Except as expressly stated in the immediately preceding
sentence, the Original Work is provided under this License on an "AS IS" BASIS
and WITHOUT WARRANTY, either express or implied, including, without limitation,
the warranties of non-infringement, merchantability or fitness for a particular
purpose. THE ENTIRE RISK AS TO THE QUALITY OF THE ORIGINAL WORK IS WITH YOU.
This DISCLAIMER OF WARRANTY constitutes an essential part of this License. No
license to the Original Work is granted by this License except under this
disclaimer.

8) Limitation of Liability. Under no circumstances and under no legal theory,
whether in tort (including negligence), contract, or otherwise, shall the
Licensor be liable to anyone for any indirect, special, incidental, or
consequential damages of any character arising as a result of this License or
the use of the Original Work including, without limitation, damages for loss of
goodwill, work stoppage, computer failure or malfunction, or any and all other
commercial damages or losses. This limitation of liability shall not apply to
the extent applicable law prohibits such limitation.

9) Acceptance and Termination. If, at any time, You expressly assented to this
License, that assent indicates your clear and irrevocable acceptance of this
License and all of its terms and conditions. If You distribute or communicate
copies of the Original Work or a Derivative Work, You must make a reasonable
effort under the circumstances to obtain the express assent of recipients to
the terms of this License. This License conditions your rights to undertake the
activities listed in Section 1, including your right to create Derivative Works
based upon the Original Work, and doing so without honoring these terms and
conditions is prohibited by copyright law and international treaty. Nothing in
this License is intended to affect copyright exceptions and limitations
(including "fair use" or "fair dealing"). This License shall terminate
immediately and You may no longer exercise any of the rights granted to You by
this License upon your failure to honor the conditions in Section 1(c).

10) Termination for Patent Action. This License shall terminate automatically
and You may no longer exercise any of the rights granted to You by this License
as of the date You commence an action, including a cross-claim or counterclaim,
against Licensor or any licensee alleging that the Original Work infringes a
patent. This termination provision shall not apply for an action alleging
patent infringement by combinations of the Original Work with other software or
hardware.

11) Jurisdiction, Venue and Governing Law. Any action or suit relating to this
License may be brought only in the courts of a jurisdiction wherein the
Licensor resides or in which Licensor conducts its primary business, and under
the laws of that jurisdiction excluding its conflict-of-law provisions. The
application of the United Nations Convention on Contracts for the International
Sale of Goods is expressly excluded. Any use of the Original Work outside the
scope of this License or after its termination shall be subject to the
requirements and penalties of copyright or patent law in the appropriate
jurisdiction. This section shall survive the termination of this License.

12) Attorneys' Fees. In any action to enforce the terms of this License or
seeking damages relating thereto, the prevailing party shall be entitled to
recover its costs and expenses, including, without limitation, reasonable
attorneys' fees and costs incurred in connection with such action, including
any appeal of such action. This section shall survive the termination of this
License.

13) Miscellaneous. If any provision of this License is held to be
unenforceable, such provision shall be reformed only to the extent necessary to
make it enforceable.

14) Definition of "You" in This License. "You" throughout this License, whether
in upper or lower case, means an individual or a legal entity exercising rights
under, and complying with all of the terms of, this License. For legal
entities, "You" includes any entity that controls, is controlled by, or is
under common control with you. For purposes of this definition, "control" means
(i) the power, direct or indirect, to cause the direction or management of such
entity, whether by contract or otherwise, or (ii) ownership of fifty percent
(50%) or more of the outstanding shares, or (iii) beneficial ownership of such
entity.

15) Right to Use. You may use the Original Work in all ways not otherwise
restricted or conditioned by this License or by law, and Licensor promises not
to interfere with or be responsible for such uses by You.

16) Modification of This License. This License is Copyright © 2005 Lawrence
Rosen. Permission is granted to copy, distribute, or communicate this License
without modification. Nothing in this License permits You to modify this
License as applied to the Original Work or to Derivative Works. However, You
may modify the text of this License and copy, distribute or communicate your
modified version (the "Modified License") and apply it to other original works
of authorship subject to the following conditions: (i) You may not indicate in
any way that your Modified License is the "Academic Free License" or "AFL" and
you may not use those names in the name of your Modified License; (ii) You must
replace the notice specified in the first paragraph above with the notice
"Licensed under <insert your license name here>" or with a notice of your own
that is not confusingly similar to the notice in this License; and (iii) You
may not claim that your original works are open source software unless your
Modified License has been approved by Open Source Initiative (OSI) and You
comply with its license review and certification process.


