# Sandstar Rust Migration -- Phased Roadmap

## Document Purpose

This document is the authoritative implementation plan for migrating Sandstar from C/C++ to Rust. It covers the complete migration path from a multi-process C/C++ system to a unified Rust binary, organized into independently deployable phases.

**Target platform:** BeagleBone Black (ARM Cortex-A8, 512MB RAM), Debian Linux
**Target triple:** `armv7-unknown-linux-gnueabihf`

---

## Current System Architecture

Sandstar is a three-process embedded IoT system:

```
+-----------------+     POSIX msgqueue     +-------------------+     SOX HTTP     +-------------+
| Engine (C)      | <--------------------> | Haystack REST API | <--------------> | Sedona SVM  |
| - channel mgmt  |    key: 0x454E4731    | (C++ / POCO)      |   localhost:8080 | (C / Java)  |
| - sensor I/O    |                        | port 8085         |                  | DDC logic   |
| - poll loop     |                        | - ops, filters    |                  |             |
| - value convert |                        | - zinc reader     |                  |             |
+-----------------+                        +-------------------+                  +-------------+
      |                                           |
      v                                           v
 sysfs / i2c / UART                        database.zinc (config)
 /sys/bus/iio/devices                      points.csv, tables.csv
 /sys/class/gpio
 /dev/i2c-2
 /dev/ttyO*
```

### Source Code Metrics (Current C/C++)

| Layer | Files | Lines of Code | Location |
|-------|-------|---------------|----------|
| Engine core (.c/.h) | 37 | 11,584 | `engine/src/` |
| Engine CLI tools (.c) | 12 | 2,266 | `engine/tools/` |
| Haystack types + ops (.cpp/.hpp/.h) | 66 | 15,438 | `EacIo/native/haystack/` |
| Zinc reader/writer + tokenizer | 10 | 1,870 | `EacIo/native/haystack/io/` |
| Engine IPC bridge (engineio) | 2 | 1,156 | `EacIo/native/engineio.c/.h` |
| **Total** | **127** | **32,314** | |

---

## Phase 0: Foundation (Preparation)

### Goal

Establish a Rust workspace that cross-compiles for ARM and deploys to BeagleBone with zero functionality -- the "hello world" binary that proves the toolchain works end-to-end.

### Tasks

1. **Create Cargo workspace** at `sandstar_rust/` with the crate layout specified in the Project Structure section below.

2. **Configure cross-compilation** in `.cargo/config.toml`:
   ```toml
   [target.armv7-unknown-linux-gnueabihf]
   linker = "arm-linux-gnueabihf-gcc"
   ```

3. **Add `libhaystack` dependency** to `sandstar-haystack/Cargo.toml`. Verify it compiles for `armv7-unknown-linux-gnueabihf`. This crate provides all Project Haystack value types, zinc reader/writer, and filter parsing -- eliminating roughly 5,000 lines of C++ code.

4. **Add `axum` + `tokio` dependencies** to `sandstar-haystack/Cargo.toml`. Verify async runtime compiles and runs on BeagleBone. Use `tokio` with features `rt`, `net`, `time`, `sync`, `macros`, `io-util`. Avoid `rt-multi-thread` initially; prefer single-threaded runtime for the 512MB device.

5. **Create `build.rs`** in workspace root for future Sedona C code linking (Phase 5). For now, it can be a stub that simply succeeds.

6. **Set up CI/CD** pipeline:
   - `cargo clippy --all-targets -- -D warnings`
   - `cargo test`
   - `cargo build --target armv7-unknown-linux-gnueabihf --release`
   - Cross-compilation can use the existing Docker container or `cross` (https://github.com/cross-rs/cross).

7. **Create deployment script** that produces a `.deb` package or copies the binary to the BeagleBone via `scp`. Integration with `installSandstar.sh` is optional at this stage.

### Deliverable

An empty Rust binary (`sandstar`) that cross-compiles for `armv7-unknown-linux-gnueabihf`, deploys to BeagleBone, and prints version info.

### Metrics

| Metric | Value |
|--------|-------|
| Estimated Rust LOC | 200-400 (boilerplate, config, build scripts) |
| Complexity | Low |
| Dependencies on prior phases | None |
| Parallelizable with | Nothing (must complete first) |

---

## Phase 1: Haystack REST API (Highest Value, Lowest Risk)

### Goal

Replace the entire POCO C++ HTTP stack and custom Haystack type system with Axum + libhaystack. The C engine process continues running unchanged -- the Rust server communicates with it via POSIX message queues (same protocol the C++ code uses today).

### Why This Phase First

1. **Highest value:** Eliminates the POCO dependency (~300,000 lines of vendored C++), the custom Haystack type implementations (~5,000 lines), and the zinc parser/writer (~1,900 lines). The libhaystack crate replaces all of this.

2. **Lowest risk:** The HTTP server is stateless (reads from engine via IPC, reads from `database.zinc` file). If the Rust server fails, rolling back means restarting the C++ server. The engine is untouched.

3. **Independently deployable:** The Rust HTTP server can run on port 8085 while the C engine process runs separately, exactly as the C++ server does today.

### What libhaystack Eliminates

The following C++ files are **not ported** -- their functionality is provided by the `libhaystack` crate:

| C++ Component | Files | LOC | libhaystack Replacement |
|---------------|-------|-----|------------------------|
| Value types (Str, Num, Bool, Ref, Date, Time, DateTime, DateTimeRange, Marker, Na, Coord, Uri, Bin, XStr, List) | 30 .cpp + 30 .hpp | ~3,800 | `libhaystack::val::*` |
| Dict and Grid | 4 files | ~1,000 | `libhaystack::dict::Dict`, `libhaystack::grid::Grid` |
| Zinc reader/writer/tokenizer | 6 files | ~1,900 | `libhaystack::encoding::zinc::*` |
| Filter parser and evaluator | 2 files | ~700 | `libhaystack::filter::Filter` |
| **Total eliminated** | **72 files** | **~7,400** | |

### Steps

#### Step 1a: Scaffold Axum Server (Read-Only, No Engine)

Port the stateless endpoints that do not need engine IPC:

- `GET /about` -- Return server metadata (haystackVersion, serverTime, bootTime, tz). Maps to `AboutOp` (C++ line: `op.cpp:215-222`).
- `GET /ops` -- Return list of supported operations. Maps to `OpsOp` (C++ line: `op.cpp:228-262`).
- `GET /formats` -- Return supported grid formats (text/zinc). Maps to `FormatsOp` (C++ line: `op.cpp:268-299`).

These three endpoints are self-contained and validate that Axum + libhaystack work correctly.

**Testing:**
```bash
curl -s http://172.28.211.135:8085/about
curl -s http://172.28.211.135:8085/ops
curl -s http://172.28.211.135:8085/formats
```
Compare output to C++ server byte-for-byte.

#### Step 1b: Add `/read` Endpoint (Requires Engine IPC)

This is the critical endpoint. The C++ implementation is in `ReadOp::on_service()` (`op.cpp:305-338`), which delegates to `PointServer::on_read_all()` for filter reads and `PointServer::read_by_ids()` for ID reads.

Sub-tasks:
1. Port the POSIX message queue IPC protocol from `engineio.c` (1,082 lines). The message format is defined in `engine.h`: `ENGINE_MESSAGE` struct with `nMessage` (long), `channel` (uint), `value` (status + raw + cur + flags + trigger), and `sender` (key_t). Message queue key is `0x454E4731`.
2. Implement `engineio_read_channel()` in Rust: send `ENGINE_MESSAGE_READ` (0x50), receive `ENGINE_MESSAGE_READACK` (0x51).
3. Port `PointServer::m_recs` (the in-memory record store) to a Rust `HashMap<String, libhaystack::dict::Dict>`.
4. Port the `database.zinc` file loader that populates `m_recs`. This currently uses the C++ `ZincReader`. In Rust, use `libhaystack::encoding::zinc::decode`.
5. Implement filter evaluation using `libhaystack::filter::Filter::apply()`.

**IPC Message Format (from `engine.h`):**
```c
struct ENGINE_MESSAGE {
    long nMessage;           // Message type ID
    ENGINE_CHANNEL channel;  // unsigned int
    ENGINE_VALUE value;      // { status, raw, cur, flags, trigger }
    key_t sender;            // Reply queue key
};
```

The Rust side must use `libc::msgget`, `libc::msgsnd`, `libc::msgrcv` with the exact same struct layout. Use `#[repr(C)]` to ensure ABI compatibility.

#### Step 1c: Add `/write` and `/pointWrite` Endpoints

Port `PointWriteOp::on_service()` (`op.cpp:572-596`) and the write-level priority array from `engineio.c`:
- `engineio_setwritelevel()` -- Manages 17-level priority array per writable point.
- `engineio_write_channel()` -- Sends `ENGINE_MESSAGE_WRITE` (0x60), receives `ENGINE_MESSAGE_WRITEACK` (0x61).
- `CHANNEL_WRITELEVEL` struct -- `{ nUsed, fValue, fDuration, sWho[17] }`.

#### Step 1d: Add `/watchSub`, `/watchUnsub`, `/watchPoll`

Port the watch subscription system (`op.cpp:450-536`):
- `WatchSubOp` -- Open or lookup watch, subscribe to point IDs.
- `WatchUnsubOp` -- Unsubscribe or close watch.
- `WatchPollOp` -- Poll for changes or full refresh.

The Haystack Watch layer (`watch.hpp`) manages watch sessions with unique IDs, lease timers, and COV (change-of-value) tracking. This maps naturally to a Rust struct with `tokio::time::interval` for lease expiry.

#### Step 1e: Add `/hisRead`

Port `HisReadOp::on_service()` (`op.cpp:602-617`):
- Requires `DateTimeRange` parsing (provided by libhaystack).
- Reads from `PointServer`'s history store (currently an in-memory ring buffer of `MAX_HISTORY=120` items per point).

#### Step 1f: Add `/nav`

Port `NavOp::on_service()` (`op.cpp:344-445`) and `RootOp` (`op.cpp:775-1057`):
- `/nav` -- Navigate the Haystack record tree.
- `/root` -- Sedona component tree browser. Makes HTTP request to Sedona SOX at `http://127.0.0.1:8080/spy/backup` and parses XML response.
- In Rust, use `reqwest` for the HTTP client and `quick-xml` for XML parsing (replacing POCO XML DOM).

#### Step 1g: Add `/xeto`

Port `XetoOp` (`op.cpp:1081-1606`):
- Generates XETO specs from live channel configuration.
- Reads `points.csv`, discovers tags from `m_recs`, generates `.xeto` files.
- This is a large class (~525 lines) but self-contained.

#### Step 1h: Port `database.zinc` Hot-Reload

Port the file-watching and hot-reload logic from `PointServer`:
- `m_dirty` flag and background write with debouncing (`mark_dirty()`, 500ms-1s delay).
- `m_reloadPending` flag for detecting external changes.
- File watching can use `notify` crate or `inotify` crate.

#### Step 1i: Port CORS and Authentication

Port the CORS logic from `Op::on_service()` (`op.cpp:66-106`):
- `Access-Control-Allow-Origin` from `m_allowIPs` list, or `*` if configured.
- `Access-Control-Allow-Method` echo.

Authentication (SCRAM-SHA1 and Basic) is currently minimal. Port or implement as Axum middleware.

### Deliverable

Rust HTTP server on port 8085, fully replacing the C++ POCO server. Engine process unchanged (still C). Communication via POSIX message queues.

### Metrics

| Metric | Value |
|--------|-------|
| C++ code replaced | ~15,438 LOC (haystack types/ops) + ~1,870 LOC (zinc I/O) + ~1,156 LOC (engineio) = **~18,464 LOC** |
| Estimated Rust LOC | ~3,000-4,000 (libhaystack does the heavy lifting) |
| Complexity | Medium |
| Dependencies on prior phases | Phase 0 |
| Parallelizable with | Phase 2 design work (but not implementation) |

---

## Phase 2: Engine Core

### Goal

Port the algorithmic layer of the engine to Rust. This includes channel management, value conversion, table interpolation, polling, watches, and notifications. Hardware drivers remain as direct sysfs/i2c/UART access (ported in Phase 3).

### Why This Phase Second

The engine core is pure computation with well-defined inputs (raw sensor values) and outputs (converted engineering values). It has no network dependencies and extensive existing test data (raw ADC values and expected conversions). This makes it highly testable.

### Components to Port

#### 2a: `channel.c` (1,362 LOC) --> `channel.rs`

**Current C design:** Array of `CHANNEL_ITEM` structs with `MAX_CHANNELS=10,000` slots. O(1) lookup via `index` array mapping channel ID to array index.

**Rust design:** `HashMap<u32, Channel>` replaces both the items array and the index array. Channel IDs are 4-digit numbers (XXYY format) with sparse distribution, making a HashMap ideal.

Key struct mapping:
```
C: CHANNEL_ITEM                    Rust: Channel
  .nUsed (int)                       (existence in HashMap)
  .nTrigger (int)                    .trigger: bool
  .nFailed (int)                     .failed: bool
  .nRetryCounter (int)               .retry_counter: u32
  .nExport (int)                     .exported: bool
  .channel (uint)                    (HashMap key)
  .channelIn (uint)                  .channel_in: Option<u32>
  .enable (enum)                     .enabled: bool
  .type (enum)                       .channel_type: ChannelType
  .direction (enum)                  .direction: ChannelDirection
  .device (uint)                     .device: u32
  .address (uint)                    .address: u32
  .conv (VALUE_CONV)                 .conv: ValueConv
  .value (ENGINE_VALUE)              .value: EngineValue
  .label (char[64])                  .label: String
  .flowDetected (int)                .flow_detected: bool
  .lastValidValue (double)           .last_valid_value: f64
  .spikeReadingCount (int)           .spike_reading_count: u32
  .smoothState (SMOOTH_STATE)        .smooth_state: SmoothState
  .rateLimitState (RATE_LIMIT_STATE) .rate_limit_state: RateLimitState
  .pwmDisabled (int)                 .pwm_disabled: bool
```

Functions to port:
- `channel_init()` / `channel_exit()` --> `ChannelManager::new()` / `Drop`
- `channel_add()` --> `ChannelManager::add()`
- `channel_remove()` --> `ChannelManager::remove()`
- `channel_read()` --> `ChannelManager::read()`
- `channel_write()` --> `ChannelManager::write()`
- `channel_convert()` --> `ChannelManager::convert()`
- `channel_update_metadata()` --> `ChannelManager::update_metadata()`
- `channel_update_virtual()` --> `ChannelManager::update_virtual()`
- `channel_report()` --> `impl Display for ChannelManager`

#### 2b: `value.c` (749 LOC) --> `value.rs`

**Current C design:** `VALUE_CONV` struct with bitflags controlling conversion behavior. The `value_convert()` function is a complex decision tree using 17 flag bits.

**Rust design:** Replace bitflags with a `ConversionFlags` bitflags struct (using the `bitflags` crate). Replace the `conv_func_ptr` function pointer with a Rust `Option<fn(f64) -> f64>` or an enum of known conversion functions.

Key struct mapping:
```
C: VALUE_CONV                      Rust: ValueConv
  .nTable (int)                      .table: Option<usize>
  .low (double)                      .low: f64
  .high (double)                     .high: f64
  .offset (double)                   .offset: f64
  .scale (double)                    .scale: f64
  .min (double)                      .min: f64
  .max (double)                      .max: f64
  .conv_func (fn ptr)                .conv_func: Option<fn(f64) -> f64>
  .flags (int)                       .flags: ConversionFlags
  .unit (char[16])                   .unit: String
  .kFactor (double)                  .k_factor: f64
  .deadBand (double)                 .dead_band: f64
  .hystOn (double)                   .hyst_on: f64
  .hystOff (double)                  .hyst_off: f64
  .scaleFactor (double)              .scale_factor: f64
  .spikeThreshold (double)           .spike_threshold: f64
  .startupDiscard (int)              .startup_discard: u32
  .reverseThreshold (double)         .reverse_threshold: f64
  .smoothWindow (int)                .smooth_window: u32
  .smoothMethod (int)                .smooth_method: SmoothMethod
  .maxRiseRate (double)              .max_rise_rate: f64
  .maxFallRate (double)              .max_fall_rate: f64
```

Conversion flag bits (from `value.h`):
- `0x0001` USELOW, `0x0002` USEHIGH, `0x0004` USEOFFSET, `0x0008` USESCALE
- `0x0010` USEMIN, `0x0020` USEMAX, `0x0100` ADC, `0x0200` DAC
- `0x0400` USEKFACTOR, `0x0800` USEDEADBAND, `0x1000` USEHYSTERESIS
- `0x2000` USESCALEFACTOR, `0x4000` USESPIKEFILTER, `0x8000` ALLOWREVERSE
- `0x10000` USESMOOTHING, `0x20000` USERATELIMIT

Functions to port:
- `value_convert()` --> `ValueConv::convert()`
- `value_revert()` --> `ValueConv::revert()`
- `value_status()` / `value_raw()` / `value_cur()` --> methods on `EngineValue`
- `value_cmp()` --> `impl PartialEq for EngineValue`
- `value_add_conv_func()` --> `ValueConv::set_conv_func()`

#### 2c: `table.c` (347 LOC) --> `table.rs`

**Current C design:** Array of `TABLE_ITEM` structs with `MAX_TABLES=16` slots. Each table is a sorted list of `double` values loaded from a text file, with linear interpolation between entries.

**Rust design:** `Vec<Table>` or `HashMap<String, Table>` keyed by tag name. Each `Table` holds a `Vec<f64>` with bounds-checked interpolation.

Functions to port:
- `table_init()` / `table_exit()` --> `TableManager::new()` / `Drop`
- `table_load()` --> `TableManager::load_from_csv()`
- `table_add()` --> `TableManager::add()`
- Interpolation logic in `value.c` that uses tables --> `Table::interpolate(raw: f64) -> f64`

The interpolation algorithm must exactly match the C implementation: find the two surrounding entries, linearly interpolate, respect ascending/descending direction. Include unit range fields for F/C/K temperature conversion.

#### 2d: `poll.c` (453 LOC) --> `poll.rs`

**Current C design:** Array of `POLL_ITEM` structs with `MAX_POLLS=64`. A pthread runs in a loop, sleeping for a configurable interval, then iterating all poll items to read channels.

**Rust design:** `PollManager` struct containing a `Vec<PollItem>` with an O(1) index. The poll thread becomes a `tokio::task` using `tokio::time::interval`.

Key additions over C:
- `unchangedCount` and `consecutiveFailCount` for I2C stuck detection are preserved.
- Integration with watch/notify (same as C: `poll_update()` calls `watch_update()` and `notify_update()`).

#### 2e: `watch.c` (227 LOC) --> `watch.rs`

**Current C design:** Array of `WATCH_ITEM` structs with `MAX_WATCHES=64`. Each watch associates a channel with a message queue sender key.

**Rust design:** `WatchManager` with `Vec<WatchEntry>` where each entry holds `channel: u32` and `sender: tokio::sync::mpsc::Sender<EngineValue>` (in Phase 4) or POSIX msgqueue key (during Phase 2).

#### 2f: `notify.c` (220 LOC) --> `notify.rs`

**Current C design:** Array of `NOTIFY_ITEM` structs with `MAX_NOTIFIES=8`. Each notify is a message queue that receives all value change events.

**Rust design:** `NotifyManager` with `Vec<NotifyEntry>`. Same pattern as watch but broadcasts to all registered listeners.

#### 2g: `zinc.c` (683 LOC) --> use libhaystack

The engine-side zinc parser (`zinc.c`) is used to parse `database.zinc` for channel configuration. In Rust, this is replaced entirely by `libhaystack::encoding::zinc::decode`. **No porting needed.**

#### 2h: `csv.c` (569 LOC) --> use `csv` crate

The CSV parser (`csv.c`) reads `points.csv` and `tables.csv`. In Rust, use the `csv` crate. **Minimal porting needed** -- just the struct definitions for CSV row types.

#### 2i: `engine.c` (2,146 LOC) --> `engine.rs`

The main engine loop and IPC message dispatcher. This is the largest and most complex file. It includes:
- Main loop: receive message from queue, dispatch based on `nMessage` type.
- Message handlers for 30+ message types (read, write, watch, notify, poll, status, convert, channel update, reports).
- Signal handlers (SIGTERM, SIGINT).
- PID file management.
- Stale queue detection (60-second timeout).
- Channel retry logic (30-cycle interval).

**Rust approach:** The main loop becomes a `tokio::select!` over:
- POSIX message queue (wrapped in an async-compatible reader, or polled via `tokio::task::spawn_blocking`)
- Poll timer (`tokio::time::interval`)
- Signal handler (`tokio::signal::unix::signal`)

### Deliverable

Engine core in Rust. Hardware access still uses sysfs file I/O directly (via `std::fs`). The engine can either run as a separate process (communicating with the Rust HTTP server via IPC) or be linked into the same binary.

### Metrics

| Metric | Value |
|--------|-------|
| C code replaced | 11,584 LOC (engine/src/) |
| Estimated Rust LOC | ~4,000-5,000 |
| Complexity | High (value conversion decision tree, IPC protocol, main loop state machine) |
| Dependencies on prior phases | Phase 0; optionally Phase 1 for integrated binary |
| Parallelizable with | Phase 1 steps 1e-1i; Phase 3 design work |

---

## Phase 3: Hardware Drivers

### Goal

Port the sysfs-based hardware drivers to Rust, using platform crates where available and `std::fs` where not.

### Components to Port

#### 3a: `anio.c` (39 LOC) --> `adc.rs`

The simplest driver. Reads analog values from `/sys/bus/iio/devices/iio:device{N}/in_voltage{M}_raw`.

**Rust approach:** Direct `std::fs::read_to_string()` with path formatting. Or use the `industrial-io` crate if it supports the BeagleBone's TI AM335x ADC.

```rust
pub fn read_adc(device: u32, address: u32) -> Result<f64, io::Error> {
    let path = format!("/sys/bus/iio/devices/iio:device{}/in_voltage{}_raw", device, address);
    let raw = std::fs::read_to_string(&path)?;
    Ok(raw.trim().parse::<f64>().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?)
}
```

#### 3b: `gpio.c` (146 LOC) --> `gpio.rs`

GPIO via legacy sysfs (`/sys/class/gpio`). Supports export/unexport, direction, edge, active_low, value.

**Rust approach:** Use `gpio-cdev` crate for modern chardev API (`/dev/gpiochip0`), which is the kernel-recommended approach. If the BeagleBone kernel is too old for chardev, fall back to `std::fs` sysfs access.

Functions to port:
- `gpio_exists()`, `gpio_export()`, `gpio_unexport()`
- `gpio_get_direction()`, `gpio_set_direction()`
- `gpio_get_edge()`, `gpio_set_edge()`
- `gpio_get_value()`, `gpio_set_value()`
- `gpio_get_activelow()`, `gpio_set_activelow()`

#### 3c: `i2cio.c` (473 LOC) + `i2c_worker.c` (617 LOC) --> `i2c.rs`

I2C sensor communication with async worker thread pool. Currently uses raw `ioctl(I2C_SLAVE)` and `read()`/`write()` syscalls.

**Rust approach:** Use `i2cdev` crate (`linux-embedded-hal` ecosystem). The async worker architecture maps to `tokio::task::spawn_blocking` for individual I2C transactions, or a dedicated worker task with `tokio::sync::mpsc` job queue.

Key behaviors to preserve:
- SDP810 differential pressure sensor protocol (continuous mode, 3-byte read with CRC).
- Retry with exponential backoff (`i2cio_get_measurement_with_retry()`).
- Bus reset recovery (`i2c_reset_bus()`).
- Persistent bus FD caching (`i2c_get_bus_fd()`).
- Async job submission via `i2c_submit_job()` with completion callbacks.

#### 3d: `pwmio.c` (289 LOC) --> `pwm.rs`

PWM output via sysfs (`/sys/class/pwm/pwmchip{N}/pwm{M}/`). Supports export, polarity, period, duty, enable.

**Rust approach:** `std::fs` with cached file handles (the current C code uses `io_write_cached()` to avoid FD exhaustion during continuous updates). Use `std::fs::File` with `Seek::Start(0)` and `write()` pattern.

#### 3e: `uartio.c` (341 LOC) + `uart_async.c` (423 LOC) --> `uart.rs`

UART serial communication with async epoll-based event loop.

**Rust approach:** Use `serialport` crate for port configuration. For async operation, use `tokio-serial` or wrap `serialport` with `tokio::io::unix::AsyncFd`. The epoll loop in `uart_async.c` maps directly to tokio's reactor.

#### 3f: `io.c` (432 LOC) --> integrated into individual drivers

The `io.c` file provides generic sysfs read/write with FD caching. In Rust:
- `io_read()` / `io_write()` --> `std::fs::read_to_string()` / `std::fs::write()`
- `io_read_cached()` / `io_write_cached()` --> `File` objects held in struct fields with `seek(SeekFrom::Start(0))` + `read()` / `write()` pattern
- `io_decode()` --> Pattern matching on read string
- FD cache (`IO_FD_CACHE_SIZE=64`) --> `HashMap<PathBuf, File>`

### Deliverable

All hardware access via Rust crates or `std::fs`. No C code remaining in the driver layer.

### Metrics

| Metric | Value |
|--------|-------|
| C code replaced | ~2,767 LOC (drivers + io + async) |
| Estimated Rust LOC | ~1,500-2,000 |
| Complexity | Medium (I2C async is the hardest part; GPIO/ADC/PWM are straightforward) |
| Dependencies on prior phases | Phase 2 (needs ChannelManager to drive I/O) |
| Parallelizable with | Phase 1 steps 1e-1i; individual drivers are independent of each other |

---

## Phase 4: IPC Unification

### Goal

Eliminate POSIX message queues. The engine and HTTP server are now both Rust code in the same process. Replace inter-process communication with intra-process `tokio::sync` channels.

### Tasks

1. **Remove POSIX message queue code** from both the engine side (formerly `engine.c` message loop) and the Haystack side (formerly `engineio.c`).

2. **Replace with `tokio::sync::mpsc`:**
   - Engine command channel: `mpsc::Sender<EngineCommand>` held by HTTP handlers; `mpsc::Receiver<EngineCommand>` consumed by engine task.
   - Engine response channel: Each request creates a `oneshot::Sender<EngineResponse>` for the reply.

3. **Replace pthread-based poll thread** with `tokio::task::spawn` + `tokio::time::interval`. The poll task runs within the same tokio runtime as the HTTP server.

4. **Replace message queue-based watches** with `tokio::sync::broadcast` or `tokio::sync::watch` channels. Watch subscribers receive value updates directly without serialization overhead.

5. **Unified error handling:** Without IPC serialization boundaries, errors propagate as Rust `Result<T, E>` types instead of status codes packed into `ENGINE_VALUE.status`.

### Architecture After Phase 4

```
Single Rust Process (tokio runtime)
+--------------------------------------------+
|  HTTP Server (axum)                        |
|    |                                       |
|    v (mpsc channel)                        |
|  Engine Core                               |
|    |                                       |
|    v (direct function call)                |
|  Hardware Drivers                          |
|    |                                       |
|    v                                       |
|  sysfs / i2c / UART                       |
+--------------------------------------------+
```

### Deliverable

Single Rust binary with no POSIX IPC. Engine tasks run as async tasks alongside HTTP handlers within one tokio runtime.

### Metrics

| Metric | Value |
|--------|-------|
| Code removed | ~1,156 LOC (engineio.c/.h) + IPC logic in engine.c |
| Estimated Rust LOC change | ~-500 (net reduction from removing serialization) |
| Complexity | Medium (refactoring, not new logic) |
| Dependencies on prior phases | Phase 1 + Phase 2 + Phase 3 |
| Parallelizable with | Nothing (requires all prior phases) |

---

## Phase 5: Sedona FFI

### Goal

Bridge the Sedona Virtual Machine (SVM) into the Rust binary via FFI. The SVM is written in C and is **not being ported to Rust** -- it remains C code linked into the Rust binary.

### Tasks

1. **Implement `extern "C"` functions** that Sedona native bindings call into. These are the `engineio_*` functions that Sedona uses to read/write channels and manage subscriptions.

2. **Use `cbindgen`** to auto-generate C headers from Rust code, ensuring the FFI boundary stays in sync.

3. **Port authentication** (SCRAM-SHA1 and HTTP Basic) to Rust. Currently minimal implementation.

4. **Port HTTP client** used for Sedona outbound requests (SOX protocol at `localhost:8080`) from POCO to `reqwest`.

5. **Configure `build.rs`** to compile Sedona C code and link it into the Rust binary:
   ```rust
   // build.rs
   cc::Build::new()
       .file("sedona/src/svm.c")
       // ... other Sedona source files
       .compile("sedona");
   ```

6. **Startup sequence:** The Rust `main()` spawns the tokio runtime, starts the HTTP server and engine tasks, then calls into the Sedona SVM's `main()` function (or spawns it as a thread).

### FFI Interface

The Sedona VM calls these functions (defined in `engineio.h`):

```rust
#[no_mangle]
pub extern "C" fn engineio_init() -> i32 { ... }
#[no_mangle]
pub extern "C" fn engineio_exit() -> i32 { ... }
#[no_mangle]
pub extern "C" fn engineio_read_channel(channel: u32, value: *mut EngineValue) -> i32 { ... }
#[no_mangle]
pub extern "C" fn engineio_write_channel(channel: u32, value: *mut EngineValue) -> i32 { ... }
#[no_mangle]
pub extern "C" fn engineio_setwritelevel(...) -> i32 { ... }
#[no_mangle]
pub extern "C" fn engineio_signal_trigger(channel: u32, value: *mut EngineValue) -> i32 { ... }
```

These functions bridge from C calling conventions to Rust's `ChannelManager` methods.

### Deliverable

Single binary containing Rust (HTTP + engine + drivers) and Sedona VM (C via FFI). No separate processes.

### Metrics

| Metric | Value |
|--------|-------|
| C code linked (not ported) | Sedona SVM (~50,000+ LOC, stays as-is) |
| Estimated Rust LOC | ~500-800 (FFI wrappers, build.rs, auth port) |
| Complexity | Medium-High (FFI safety, thread coordination with SVM) |
| Dependencies on prior phases | Phase 4 |
| Parallelizable with | Can prototype FFI wrappers during Phase 2-3 |

---

## Phase 6: CLI Tools

### Goal

Port the 12 engine CLI tools from C to Rust. These are small utilities that communicate with the engine via POSIX message queues (or, post-Phase 4, via a Unix socket or shared memory).

### Tools to Port

| C Tool | LOC | Purpose | Rust Approach |
|--------|-----|---------|---------------|
| `read.c` | 180 | Read a channel value | `clap` subcommand |
| `write.c` | 200 | Write a value to a channel | `clap` subcommand |
| `watch.c` | 178 | Watch a channel for changes | `clap` subcommand |
| `notify.c` | 165 | Register for change notifications | `clap` subcommand |
| `channels.c` | 201 | List all configured channels | `clap` subcommand |
| `tables.c` | 183 | List all lookup tables | `clap` subcommand |
| `polls.c` | 228 | List all poll items | `clap` subcommand |
| `watches.c` | 182 | List all active watches | `clap` subcommand |
| `notifies.c` | 181 | List all active notifications | `clap` subcommand |
| `convert.c` | 200 | Test value conversion | `clap` subcommand |
| `status.c` | 160 | Show engine status | `clap` subcommand |
| `pwmscan.c` | 208 | Scan PWM channels | `clap` subcommand |

**Total C LOC:** 2,266

### Design Decision: Single Binary vs Multiple Binaries

**Recommended: Single binary with subcommands** (like `git`):

```bash
sandstar-cli read 1113
sandstar-cli write 510 75.0
sandstar-cli channels
sandstar-cli watch 1113
sandstar-cli status
```

This reduces deployment footprint (one binary instead of 12) and shares the IPC/communication code.

### Communication Post-Phase 4

After Phase 4 eliminates POSIX message queues, CLI tools need a new way to communicate with the running engine. Options:
1. **Unix domain socket** -- CLI sends JSON or binary commands, engine responds.
2. **HTTP API** -- CLI tools use `curl` semantics against `localhost:8085`.
3. **Shared memory + semaphore** -- For highest performance.

Recommended: Unix domain socket with a simple binary protocol, or HTTP API (reuse existing Haystack endpoints).

### Deliverable

All CLI tools as a single Rust binary (`sandstar-cli`) with `clap` subcommands.

### Metrics

| Metric | Value |
|--------|-------|
| C code replaced | 2,266 LOC |
| Estimated Rust LOC | ~800-1,200 |
| Complexity | Low |
| Dependencies on prior phases | Phase 2 (for value types); Phase 4 (for communication) |
| Parallelizable with | Phases 3-5 (independent work stream) |

---

## Testing Strategy

### Per-Phase Testing

Each phase follows this testing ladder:

#### 1. Unit Tests (`cargo test`)

- Every public function gets at least one test.
- Value conversion: Test every flag combination against known C output. The C codebase has implicit test data in the form of `convert` tool invocations and sensor readings.
- Table interpolation: Test boundary conditions (exact match, below min, above max, ascending vs descending tables).
- Channel CRUD: Add, read, write, remove, report.
- Zinc parsing: Round-trip test (parse zinc string, write back, compare).

#### 2. Integration Tests Against Real Hardware

Run on BeagleBone with physical sensors connected:
```bash
# Read ADC raw value and compare to /sys/bus/iio read
sandstar-cli read 1113
cat /sys/bus/iio/devices/iio:device0/in_voltage0_raw

# Write PWM and verify with oscilloscope
sandstar-cli write 510 50.0
cat /sys/class/pwm/pwmchip*/pwm0/duty_cycle
```

#### 3. A/B Comparison

Run C and Rust versions side-by-side on the same device:
- C engine on port 8085, Rust engine on port 8086 (or vice versa).
- Script that hits both endpoints with identical requests and diffs the zinc output.
- Automated: `curl C | sort > /tmp/c.txt && curl Rust | sort > /tmp/rust.txt && diff /tmp/c.txt /tmp/rust.txt`

#### 4. Sensor Reading Validation

For each sensor type (thermistor, RTD, 4-20mA, I2C flow, GPIO):
- Read 100 consecutive values from both C and Rust.
- Compare: values must match within floating-point tolerance (1e-6 for raw, 0.01 for engineering units).

#### 5. Long-Running Soak Test

- Deploy Rust binary to BeagleBone.
- Run for 48+ hours under normal operating conditions.
- Monitor via `top` / `ps`: RSS memory must not grow (no leaks).
- Monitor via `journalctl`: no panics, no error spikes.
- Compare 48-hour history data to C baseline.

### Regression Testing Across Phases

Maintain a "golden" test dataset:
- 50+ channel configurations with known conversions.
- 10+ `database.zinc` files representing real deployments.
- curl scripts that exercise every HTTP endpoint.

Run this suite before and after each phase merge.

---

## Risk Mitigation

### Independent Deployability

Each phase produces a deployable artifact:

| Phase | Deployed Configuration |
|-------|----------------------|
| 0 | No change (C/C++ still running) |
| 1 | Rust HTTP server + C engine (two processes) |
| 2 | Rust HTTP + Rust engine + C drivers (may still be two processes) |
| 3 | Rust HTTP + Rust engine + Rust drivers |
| 4 | Single Rust process (no IPC) |
| 5 | Single Rust process + Sedona VM (linked) |
| 6 | Single Rust process + Rust CLI tools |

### Rollback Plan

- The C/C++ codebase remains in the main git branch throughout migration.
- Each Rust phase is developed on a feature branch.
- The `.deb` package build system supports both configurations.
- Rollback: install the C/C++ `.deb` package, restart `sandstar.service`.

### Feature Flags for A/B Testing

During Phase 1, the systemd service file can be configured to start either:
```ini
# /etc/systemd/system/sandstar.service
# Option A: C++ Haystack
ExecStart=/home/eacio/sandstar/bin/haystack_server

# Option B: Rust Haystack
ExecStart=/home/eacio/sandstar/bin/sandstar-haystack
```

Both read from the same `database.zinc` and communicate with the same C engine via the same POSIX message queue key (`0x454E4731`).

### Known Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| libhaystack does not support all zinc edge cases | Phase 1 blocked | Fork and patch; zinc format is stable |
| tokio runtime overhead on 512MB ARM | Performance regression | Use single-threaded runtime; benchmark early |
| POSIX msgqueue FFI correctness | Silent data corruption | Byte-for-byte A/B testing against C |
| I2C timing sensitivity | Sensor read failures | Keep C driver code available as fallback |
| Sedona SVM thread safety | Crashes or data races | SVM runs on its own thread; FFI uses mutex |

---

## Phase Dependency Graph

```
Phase 0 (Foundation)
    |
    +---> Phase 1 (Haystack REST API)
    |         |
    |         +---> Phase 4 (IPC Unification) ---> Phase 5 (Sedona FFI)
    |         |
    +---> Phase 2 (Engine Core)
    |         |
    |         +---> Phase 3 (Hardware Drivers)
    |         |
    |         +---> Phase 6 (CLI Tools)
    |
    (Phase 1 and Phase 2 can proceed in parallel after Phase 0)
    (Phase 3 requires Phase 2)
    (Phase 4 requires Phase 1 + Phase 2 + Phase 3)
    (Phase 5 requires Phase 4)
    (Phase 6 requires Phase 2, can proceed in parallel with Phases 3-5)
```

### Parallelization Opportunities

| Work Stream A | Work Stream B | Notes |
|---------------|---------------|-------|
| Phase 1 (HTTP) | Phase 2 design | Different codebases, no overlap |
| Phase 1 steps 1e-1i | Phase 2 implementation | After 1d completes |
| Phase 3 individual drivers | Each other | adc.rs, gpio.rs, i2c.rs, pwm.rs, uart.rs are independent |
| Phase 6 (CLI tools) | Phases 3-5 | Independent work stream |

---

## Summary: Lines of Code by Phase

| Phase | C/C++ Replaced | Estimated Rust LOC | Complexity | Depends On |
|-------|---------------|--------------------|------------|------------|
| 0: Foundation | 0 | 200-400 | Low | -- |
| 1: Haystack REST | ~18,464 | 3,000-4,000 | Medium | Phase 0 |
| 2: Engine Core | ~11,584 | 4,000-5,000 | High | Phase 0 |
| 3: Hardware Drivers | ~2,767 | 1,500-2,000 | Medium | Phase 2 |
| 4: IPC Unification | ~1,156 (removed) | -500 (net) | Medium | Phases 1+2+3 |
| 5: Sedona FFI | 0 (SVM stays C) | 500-800 | Medium-High | Phase 4 |
| 6: CLI Tools | ~2,266 | 800-1,200 | Low | Phase 2 |
| **Total** | **~32,314** (fully replaced or linked) | **~10,000-13,400** | | |

The Rust codebase is expected to be roughly 30-40% the size of the C/C++ codebase, primarily due to:
1. `libhaystack` eliminating ~7,400 LOC of Haystack types and zinc parsing.
2. `axum` replacing ~10,000 LOC of POCO HTTP framework integration.
3. Rust's standard library and ecosystem crates (`csv`, `clap`, `serialport`, `i2cdev`) replacing hand-written C parsers and drivers.

---

## Project Structure

```
sandstar_rust/
+-- Cargo.toml                     # Workspace root
+-- .cargo/
|   +-- config.toml               # ARM cross-compilation config
+-- crates/
|   +-- sandstar-engine/          # Engine core (Phase 2)
|   |   +-- src/
|   |   |   +-- lib.rs
|   |   |   +-- channel.rs        # ChannelManager, Channel, ChannelType
|   |   |   +-- value.rs          # ValueConv, EngineValue, ConversionFlags
|   |   |   +-- table.rs          # TableManager, Table, interpolation
|   |   |   +-- poll.rs           # PollManager, PollItem
|   |   |   +-- watch.rs          # WatchManager, WatchEntry
|   |   |   +-- notify.rs         # NotifyManager, NotifyEntry
|   |   |   +-- smooth.rs         # SmoothState, SmoothMethod
|   |   |   +-- rate_limit.rs     # RateLimitState
|   |   +-- Cargo.toml
|   +-- sandstar-hal/             # Hardware drivers (Phase 3)
|   |   +-- src/
|   |   |   +-- lib.rs
|   |   |   +-- adc.rs            # Analog input via IIO sysfs
|   |   |   +-- gpio.rs           # Digital I/O via gpio-cdev or sysfs
|   |   |   +-- i2c.rs            # I2C sensors via i2cdev
|   |   |   +-- i2c_worker.rs     # Async I2C worker pool
|   |   |   +-- pwm.rs            # PWM output via sysfs
|   |   |   +-- uart.rs           # UART via serialport
|   |   |   +-- uart_async.rs     # Async UART via tokio
|   |   |   +-- io_cache.rs       # Sysfs FD caching
|   |   +-- Cargo.toml
|   +-- sandstar-haystack/        # Haystack REST API (Phase 1)
|   |   +-- src/
|   |   |   +-- lib.rs
|   |   |   +-- server.rs         # Axum router, shared state, startup
|   |   |   +-- ops/              # One file per Haystack operation
|   |   |   |   +-- mod.rs
|   |   |   |   +-- about.rs      # /about endpoint
|   |   |   |   +-- read.rs       # /read endpoint (filter + ID reads)
|   |   |   |   +-- write.rs      # /write endpoint
|   |   |   |   +-- nav.rs        # /nav + /root endpoints
|   |   |   |   +-- watch.rs      # /watchSub, /watchUnsub, /watchPoll
|   |   |   |   +-- his_read.rs   # /hisRead endpoint
|   |   |   |   +-- point_write.rs # /pointWrite endpoint
|   |   |   |   +-- commit.rs     # /commit (add/update/delete/optimize/zinc)
|   |   |   |   +-- formats.rs    # /formats endpoint
|   |   |   |   +-- ops_list.rs   # /ops endpoint
|   |   |   |   +-- xeto.rs       # /xeto endpoint (XETO spec generator)
|   |   |   |   +-- restart.rs    # /restart endpoint
|   |   |   +-- points.rs         # PointServer: record store, zinc loader
|   |   |   +-- ipc.rs            # POSIX msgqueue bridge (Phase 1 only)
|   |   |   +-- cors.rs           # CORS middleware
|   |   |   +-- auth.rs           # Authentication middleware
|   |   |   +-- zinc_file.rs      # database.zinc hot-reload + dirty write
|   |   +-- Cargo.toml
|   +-- sandstar-ipc/             # IPC layer (Phase 1-3, removed in Phase 4)
|   |   +-- src/
|   |   |   +-- lib.rs
|   |   |   +-- msgqueue.rs       # POSIX message queue wrapper
|   |   |   +-- messages.rs       # ENGINE_MESSAGE structs (#[repr(C)])
|   |   +-- Cargo.toml
|   +-- sandstar-ffi/             # Sedona FFI (Phase 5)
|   |   +-- src/
|   |   |   +-- lib.rs
|   |   |   +-- engineio.rs       # extern "C" FFI functions
|   |   +-- Cargo.toml
|   +-- sandstar-cli/             # CLI tools (Phase 6)
|       +-- src/
|       |   +-- main.rs           # clap app with subcommands
|       |   +-- read.rs
|       |   +-- write.rs
|       |   +-- watch.rs
|       |   +-- channels.rs
|       |   +-- tables.rs
|       |   +-- polls.rs
|       |   +-- convert.rs
|       |   +-- status.rs
|       +-- Cargo.toml
+-- src/
|   +-- main.rs                   # Binary entry point (Phase 4+: unified)
+-- build.rs                      # Sedona C compilation (Phase 5)
+-- tests/
|   +-- integration/              # Cross-phase integration tests
|   +-- golden/                   # Golden test data (zinc files, conversions)
+-- docs/
    +-- research/                 # This document and related research
```

### Cargo Workspace Configuration

```toml
# sandstar_rust/Cargo.toml
[workspace]
members = [
    "crates/sandstar-engine",
    "crates/sandstar-hal",
    "crates/sandstar-haystack",
    "crates/sandstar-ipc",
    "crates/sandstar-ffi",
    "crates/sandstar-cli",
]
resolver = "2"

[workspace.dependencies]
tokio = { version = "1", features = ["rt", "net", "time", "sync", "macros", "io-util", "signal"] }
axum = "0.7"
libhaystack = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
clap = { version = "4", features = ["derive"] }
csv = "1"
bitflags = "2"
thiserror = "1"
anyhow = "1"
```

### Cross-Compilation Configuration

```toml
# sandstar_rust/.cargo/config.toml
[target.armv7-unknown-linux-gnueabihf]
linker = "arm-linux-gnueabihf-gcc"

[build]
# Uncomment for default ARM target:
# target = "armv7-unknown-linux-gnueabihf"
```

---

## Key Design Decisions

### 1. libhaystack as the Haystack Foundation

The `libhaystack` crate provides Project Haystack 4 types, zinc encoding/decoding, and filter evaluation. This single dependency eliminates the need to port:
- 30 value type classes (Str, Num, Bool, Ref, Date, Time, DateTime, etc.)
- Grid and Dict containers
- Zinc reader, writer, and tokenizer
- Haystack filter parser and evaluator

**Risk:** If libhaystack lacks a feature needed by Sandstar, we fork and contribute upstream.

### 2. Axum Over Other HTTP Frameworks

Axum is chosen over alternatives (actix-web, warp, rocket) because:
- Built on tokio and tower, the standard Rust async ecosystem.
- Shared state via `State<Arc<AppState>>` maps naturally to `PointServer` globals.
- Middleware (tower layers) maps to CORS and auth requirements.
- Lightweight enough for 512MB ARM.

### 3. Single-Threaded Tokio Runtime

The BeagleBone has a single-core ARM Cortex-A8 CPU. A multi-threaded tokio runtime would add overhead without benefit. Use:
```rust
#[tokio::main(flavor = "current_thread")]
async fn main() { ... }
```

Exceptions: I2C worker tasks that perform blocking 50ms reads should use `tokio::task::spawn_blocking()` to avoid stalling the reactor.

### 4. HashMap Instead of Fixed-Size Arrays

The C code uses fixed-size arrays (`MAX_CHANNELS=10000`, `MAX_POLLS=64`, etc.) with O(1) index arrays. In Rust, `HashMap<u32, T>` provides the same O(1) lookup with:
- No wasted memory for sparse channel IDs.
- No maximum limit (grows dynamically).
- Built-in iteration.

### 5. POSIX IPC as Transitional Layer

During Phases 1-3, the Rust HTTP server and C engine communicate via POSIX message queues -- the same mechanism the C++ code uses. This enables incremental migration without changing the C engine. Phase 4 removes this layer entirely.

### 6. Sedona VM Stays as C

The Sedona VM is a mature, stable codebase that would take significant effort to port with minimal benefit. It is linked into the Rust binary via FFI (Phase 5) rather than rewritten.

---

## Appendix A: IPC Message Protocol Reference

Messages exchanged between the HTTP server and engine via POSIX message queues:

| Message ID | Hex | Direction | Purpose |
|-----------|-----|-----------|---------|
| `ENGINE_MESSAGE_STOP` | 0x01 | HTTP-->Engine | Stop engine |
| `ENGINE_MESSAGE_RESTART` | 0x02 | HTTP-->Engine | Restart engine |
| `ENGINE_MESSAGE_POLL` | 0x03 | HTTP-->Engine | Trigger poll cycle |
| `ENGINE_MESSAGE_STATUS` | 0x10 | HTTP-->Engine | Request status |
| `ENGINE_MESSAGE_CHANNELS` | 0x11 | HTTP-->Engine | Request channel list |
| `ENGINE_MESSAGE_TABLES` | 0x12 | HTTP-->Engine | Request table list |
| `ENGINE_MESSAGE_POLLS` | 0x13 | HTTP-->Engine | Request poll list |
| `ENGINE_MESSAGE_WATCHES` | 0x14 | HTTP-->Engine | Request watch list |
| `ENGINE_MESSAGE_NOTIFIES` | 0x15 | HTTP-->Engine | Request notify list |
| `ENGINE_MESSAGE_CONVERT` | 0x20 | HTTP-->Engine | Convert value |
| `ENGINE_MESSAGE_CONVACK` | 0x21 | Engine-->HTTP | Convert response |
| `ENGINE_MESSAGE_NOTIFY` | 0x30 | HTTP-->Engine | Register notification |
| `ENGINE_MESSAGE_CHANGE` | 0x31 | Engine-->HTTP | Value changed |
| `ENGINE_MESSAGE_UNNOTIFY` | 0x32 | HTTP-->Engine | Unregister notification |
| `ENGINE_MESSAGE_WATCH` | 0x40 | HTTP-->Engine | Add watch |
| `ENGINE_MESSAGE_UPDATE` | 0x41 | Engine-->HTTP | Watch update |
| `ENGINE_MESSAGE_UNWATCH` | 0x42 | HTTP-->Engine | Remove watch |
| `ENGINE_MESSAGE_READ` | 0x50 | HTTP-->Engine | Read channel |
| `ENGINE_MESSAGE_READACK` | 0x51 | Engine-->HTTP | Read response |
| `ENGINE_MESSAGE_WRITE` | 0x60 | HTTP-->Engine | Write channel |
| `ENGINE_MESSAGE_WRITEACK` | 0x61 | Engine-->HTTP | Write response |
| `ENGINE_MESSAGE_WRITE_VIRTUAL` | 0x62 | HTTP-->Engine | Write virtual channel |
| `ENGINE_CHANNELS_REPORT` | 0x64 | HTTP-->Engine | Full channel dump |
| `ENGINE_CHANNELS_REPORT_ACK` | 0x65 | Engine-->HTTP | Channel dump data |
| `ENGINE_STATUS_MSG` | 0x66 | HTTP-->Engine | Status request |
| `ENGINE_STATUS_MSG_ACK` | 0x67 | Engine-->HTTP | Status response |
| `ENGINE_TABLES_REPORT` | 0x68 | HTTP-->Engine | Table dump |
| `ENGINE_TABLES_REPORT_ACK` | 0x69 | Engine-->HTTP | Table dump data |
| `ENGINE_WATCHES_REPORT` | 0x6A | HTTP-->Engine | Watch dump |
| `ENGINE_WATCHES_REPORT_ACK` | 0x6B | Engine-->HTTP | Watch dump data |
| `ENGINE_NOTIFIES_REPORT` | 0x6C | HTTP-->Engine | Notify dump |
| `ENGINE_NOTIFIES_REPORT_ACK` | 0x6D | Engine-->HTTP | Notify dump data |
| `ENGINE_POLLS_REPORT` | 0x6E | HTTP-->Engine | Poll dump |
| `ENGINE_POLLS_REPORT_ACK` | 0x6F | Engine-->HTTP | Poll dump data |
| `ENGINE_MESSAGE_CHANNEL_UPDATE` | 0x72 | HTTP-->Engine | Update channel metadata |
| `ENGINE_MESSAGE_CHANNEL_UPDATE_ACK` | 0x73 | Engine-->HTTP | Update ack |

Message queue key: `0x454E4731` ("ENG1" in ASCII)

---

## Appendix B: Haystack Operations Reference

Operations registered in the C++ `StdOps` class that must be implemented in Rust:

| Operation | C++ Class | Endpoint | Priority |
|-----------|-----------|----------|----------|
| about | AboutOp | GET /about | Step 1a |
| ops | OpsOp | GET /ops | Step 1a |
| formats | FormatsOp | GET /formats | Step 1a |
| read | ReadOp | GET /read?filter=... | Step 1b |
| nav | NavOp | GET /nav | Step 1f |
| root | RootOp | GET /root | Step 1f |
| watchSub | WatchSubOp | POST /watchSub | Step 1d |
| watchUnsub | WatchUnsubOp | POST /watchUnsub | Step 1d |
| watchPoll | WatchPollOp | POST /watchPoll | Step 1d |
| watchList | WatchListOp | GET /watchList | Step 1d |
| pointWrite | PointWriteOp | POST /pointWrite | Step 1c |
| hisRead | HisReadOp | GET /hisRead | Step 1e |
| hisWrite | HisWriteOp | POST /hisWrite | Step 1e |
| invokeAction | InvokeActionOp | POST /invokeAction | Step 1c |
| commit | CommitOp | POST /commit | Step 1c |
| restart | RestartOp | POST /restart | Step 1a |
| xeto | XetoOp | GET /xeto | Step 1g |

---

## Appendix C: Crate Dependencies by Phase

### Phase 0
```
tokio, axum (validation only)
```

### Phase 1
```
axum, tokio, libhaystack, serde, serde_json, tracing, tracing-subscriber,
libc (POSIX msgqueue), reqwest (SOX HTTP client), quick-xml (Sedona XML),
notify (file watching), tower-http (CORS)
```

### Phase 2
```
bitflags, csv, thiserror, anyhow
```

### Phase 3
```
i2cdev (or linux-embedded-hal), gpio-cdev, serialport (or tokio-serial)
```

### Phase 4
```
(no new dependencies -- removes libc msgqueue usage)
```

### Phase 5
```
cbindgen (build dependency), cc (build dependency)
```

### Phase 6
```
clap
```
