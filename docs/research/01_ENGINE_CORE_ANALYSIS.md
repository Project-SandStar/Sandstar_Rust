# 01: Engine Core Analysis — C to Rust Mapping

## Overview

The engine core is the heart of Sandstar: a daemon process that manages hardware channels, polls sensors, converts values, and communicates via POSIX IPC. This document maps every C construct to its Rust equivalent.

**Source files:** `engine.c` (2,147 lines), `channel.c` (1,363 lines), `value.c` (750 lines), `table.c` (347 lines), `poll.c` (453 lines), `watch.c` (227 lines), `notify.c` (220 lines)

**Total: ~5,507 lines C → estimated ~3,000 lines Rust**

---

## 1. Core Data Types

### ENGINE_VALUE (engine.h:39-52)

```c
// C current
struct _ENGINE_VALUE {
    ENGINE_STATUS status;    // enum: OK, UNKNOWN, STALE, DISABLED, FAULT, DOWN
    ENGINE_DATA raw;         // typedef double
    ENGINE_DATA cur;         // typedef double
    int flags;               // ENGINE_DATA_RAW (0x01), ENGINE_DATA_CUR (0x02)
    int trigger;             // trigger event ID
};
```

```rust
// Rust equivalent
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChannelStatus {
    Ok,
    Unknown,
    Stale,
    Disabled,
    Fault,
    Down,
}

#[derive(Debug, Clone, Copy)]
pub struct EngineValue {
    pub status: ChannelStatus,
    pub raw: f64,
    pub cur: f64,
    pub has_raw: bool,     // replaces flags bitmask
    pub has_cur: bool,     // replaces flags bitmask
    pub trigger: Option<u32>,  // None instead of 0
}
```

**Rust improvement:** The `flags` bitmask becomes two explicit booleans. `trigger` becomes `Option<u32>` — `None` is clearer than magic value 0.

### ENGINE_MESSAGE (engine.h:54-65)

```c
// C current
struct _ENGINE_MESSAGE {
    long nMessage;           // message type ID (used as mtype for msgrcv)
    ENGINE_CHANNEL channel;  // unsigned int
    ENGINE_VALUE value;
    key_t sender;            // reply queue key
};
```

```rust
// Rust equivalent
#[derive(Debug, Clone)]
pub enum EngineCommand {
    Stop,
    Restart,
    Poll,
    Status,
    Read { channel: u32, reply_to: Option<Sender<EngineReply>> },
    Write { channel: u32, value: EngineValue, reply_to: Option<Sender<EngineReply>> },
    Watch { channel: u32, subscriber: u32 },
    Unwatch { channel: u32, subscriber: u32 },
    Notify { subscriber: u32 },
    Unnotify { subscriber: u32 },
    Convert { channel: u32, value: EngineValue },
    ChannelUpdate { channel: u32, enable: bool, conv: ValueConv, label: String },
    ReportChannels,
    ReportTables,
    ReportPolls,
    ReportWatches,
    ReportNotifies,
}

#[derive(Debug, Clone)]
pub struct EngineReply {
    pub channel: u32,
    pub value: EngineValue,
}
```

**Rust improvement:** The raw `long nMessage` dispatch becomes a typed enum. The `key_t sender` reply queue becomes a typed `Sender<EngineReply>` channel. **Impossible to send wrong message type or reply to wrong queue.**

### VALUE_CONV (value.h:43-88)

```c
// C current: 88 lines of struct definition
struct _VALUE_CONV {
    int nTable;
    double low, high, offset, scale, min, max;
    conv_func_ptr conv_func;    // double (*)(double)
    int flags;                  // 17 different flag bits
    char unit[16];
    double kFactor, deadBand, hystOn, hystOff, scaleFactor;
    double spikeThreshold;
    int startupDiscard;
    double reverseThreshold;
    int smoothWindow, smoothMethod;
    double maxRiseRate, maxFallRate;
};
```

```rust
// Rust equivalent
#[derive(Debug, Clone)]
pub struct ValueConv {
    pub table_index: Option<usize>,           // None instead of -1
    pub low: Option<f64>,                      // None instead of flag check
    pub high: Option<f64>,
    pub offset: Option<f64>,
    pub scale: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub unit: String,                          // heap-allocated, no overflow
    pub conv_func: Option<ConversionFn>,       // enum, not raw fn pointer
    pub flow_config: Option<FlowConfig>,       // grouped SDP610 params
    pub spike_filter: Option<SpikeFilterConfig>,
    pub smoothing: Option<SmoothingConfig>,
    pub rate_limit: Option<RateLimitConfig>,
    pub adc_mode: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum ConversionFn {
    Sdp610ToPa,
    Sdp610ToInH2O,
    Sdp610ToPsi,
    Sdp610ToCfm,
    Sdp610ToLps,
}

#[derive(Debug, Clone)]
pub struct FlowConfig {
    pub k_factor: f64,        // default 14000
    pub dead_band: f64,       // default 5
    pub hyst_on: f64,         // default 16
    pub hyst_off: f64,        // default 8
    pub scale_factor: f64,    // default 60
    pub allow_reverse: bool,
    pub reverse_threshold: f64,
}
```

**Rust improvement:** The 17-bit flag field becomes `Option<T>` fields — each parameter is either present or absent, enforced at compile time. The raw function pointer becomes an enum, eliminating the possibility of calling through a dangling pointer. SDP610 configuration is grouped into a struct, not scattered across 10 loose fields.

### CHANNEL_ITEM (channel.h:97-140)

```c
// C current
struct _CHANNEL_ITEM {
    int nUsed;              // 0 or 1
    int nTrigger;
    int nFailed;
    int nRetryCounter;
    int nExport;
    ENGINE_CHANNEL channel; // unsigned int
    ENGINE_CHANNEL channelIn;
    CHANNEL_ENABLE enable;
    CHANNEL_TYPE type;
    CHANNEL_DIRECTION direction;
    CHANNEL_DEVICE device;
    CHANNEL_ADDRESS address;
    VALUE_CONV conv;
    ENGINE_VALUE value;
    char label[64];         // fixed buffer
    int flowDetected;
    double lastValidValue;
    int spikeReadingCount;
    SMOOTH_STATE smoothState;
    RATE_LIMIT_STATE rateLimitState;
    int pwmDisabled;
};
```

```rust
// Rust equivalent
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChannelType {
    Analog,
    Digital,
    Pwm,
    Triac,
    I2c,
    Uart,
    VirtualAnalog,
    VirtualDigital,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChannelDirection {
    In,
    Out,
    High,
    Low,
    None,
}

#[derive(Debug)]
pub struct Channel {
    pub id: u32,
    pub enabled: bool,
    pub channel_type: ChannelType,
    pub direction: ChannelDirection,
    pub device: u32,
    pub address: u32,
    pub trigger: Option<u32>,
    pub conv: ValueConv,
    pub label: String,              // no fixed buffer overflow
    pub value: EngineValue,         // cached last reading
    pub hw_state: HardwareState,    // groups nFailed, nRetryCounter, nExport, pwmDisabled
    pub filter_state: FilterState,  // groups flow, spike, smooth, rate limit state
}

#[derive(Debug, Default)]
pub struct HardwareState {
    pub failed: bool,
    pub retry_counter: u32,
    pub exported: bool,
    pub pwm_disabled: bool,
}

#[derive(Debug, Default)]
pub struct FilterState {
    pub flow_detected: bool,
    pub last_valid_value: f64,
    pub spike_reading_count: u32,
    pub smooth: SmoothState,
    pub rate_limit: RateLimitState,
}
```

**Rust improvement:** `nUsed` field disappears — Rust uses `Option<Channel>` or a `HashMap<u32, Channel>` instead of sparse array with used/unused flags. The `label[64]` fixed buffer becomes `String`. Hardware state and filter state are grouped into sub-structs.

### CHANNEL container (channel.h:144-155)

```c
// C current: sparse array with O(1) index
struct _CHANNEL {
    int nItems;          // allocated capacity
    int nCount;          // actual count
    CHANNEL_ITEM *items; // malloc'd array
    int *index;          // channel_id → items[] index, malloc'd
};
```

```rust
// Rust equivalent: HashMap gives O(1) lookup naturally
use std::collections::HashMap;

pub struct ChannelManager {
    channels: HashMap<u32, Channel>,  // channel_id → Channel
}

impl ChannelManager {
    pub fn get(&self, id: u32) -> Option<&Channel> {
        self.channels.get(&id)
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut Channel> {
        self.channels.get_mut(&id)
    }

    pub fn add(&mut self, channel: Channel) -> Result<(), EngineError> {
        if self.channels.contains_key(&channel.id) {
            return Ok(()); // already exists
        }
        self.channels.insert(channel.id, channel);
        Ok(())
    }

    pub fn remove(&mut self, id: u32) -> Option<Channel> {
        self.channels.remove(&id)
    }

    pub fn count(&self) -> usize {
        self.channels.len()
    }
}
```

**Rust improvement:** The manual sparse array + separate index array + manual `malloc`/`free` becomes a single `HashMap<u32, Channel>`. No need for `channel_init`, `channel_exit`, `channel_alloc`, `channel_find` — those 100+ lines of C become zero lines of Rust.

---

## 2. Engine Main Loop

### C Current (engine.c:1754-2026)

```c
static int engine_loop(ARGS *args) {
    CHANNEL channels = {0};
    TABLE tables = {0};
    POLL poll = {0};

    engine_load(args, &channels, &tables, &poll);

    WATCH watch;
    watch_init(&watch, MAX_WATCHES);
    NOTIFY notify;
    notify_init(&notify, MAX_NOTIFIES);

    while(g_quit == 0) {
        ENGINE_MESSAGE msg;
        err = msgrcv(g_hEngine, &msg, ...);  // BLOCKING

        switch(msg.nMessage) {
            case ENGINE_MESSAGE_STOP: g_quit = 1; break;
            case ENGINE_MESSAGE_POLL: poll_update(&poll, &channels, ...); break;
            case ENGINE_MESSAGE_READ: channel_read(&channels, ...); engine_reply(...); break;
            case ENGINE_MESSAGE_WRITE: channel_write(&channels, ...); engine_reply(...); break;
            // ... 15+ more cases
        }
    }

    // manual cleanup
    watch_exit(&watch);
    notify_exit(&notify);
    poll_exit(&poll);
    table_exit(&tables);
    channel_exit(&channels);
}
```

### Rust Equivalent

```rust
pub struct Engine {
    channels: ChannelManager,
    tables: TableManager,
    polls: PollManager,
    watches: WatchManager,
    notifies: NotifyManager,
    watchdog: WatchdogManager,
    config: EngineConfig,
}

impl Engine {
    pub async fn run(
        mut self,
        mut cmd_rx: tokio::sync::mpsc::Receiver<EngineCommand>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), EngineError> {
        loop {
            tokio::select! {
                Some(cmd) = cmd_rx.recv() => {
                    match cmd {
                        EngineCommand::Stop => break,
                        EngineCommand::Restart => self.reload_config()?,
                        EngineCommand::Poll => self.poll_update(),
                        EngineCommand::Read { channel, reply_to } => {
                            let result = self.channel_read(channel);
                            if let Some(tx) = reply_to {
                                let _ = tx.send(result);
                            }
                        }
                        EngineCommand::Write { channel, value, reply_to } => {
                            let result = self.channel_write(channel, value);
                            if let Some(tx) = reply_to {
                                let _ = tx.send(result);
                            }
                        }
                        // ... pattern match all variants (exhaustive!)
                    }
                }
                _ = shutdown.changed() => break,
            }
        }
        Ok(())
        // All resources dropped automatically via RAII
    }
}
```

**Rust improvements:**
1. **No manual cleanup** — `RAII` drops everything when `Engine` goes out of scope
2. **Exhaustive match** — forgetting a message type is a compile error
3. **Typed channels** — can't send wrong message type or reply to wrong queue
4. **Async/await** — `tokio::select!` replaces blocking `msgrcv` + poll thread
5. **No signal handler unsafety** — tokio handles `SIGTERM`/`SIGINT` safely
6. **No `goto` labels** — structured control flow

---

## 3. Value Conversion System

### C Current (value.c:327-612, 285 lines)

The `value_convert` function is the most complex algorithmic code. It handles:
- Auto-detection of sensor type from channel number (1100-1723)
- Table lookup with binary search and linear interpolation
- Range clamping (low/high on raw, min/max on converted)
- SDP610 flow sensor conversion (Pa, inH2O, CFM, LPS, PSI)
- Hysteresis for flow detection
- Scale/offset linear transformation
- ADC mode (analog-to-digital threshold)

### Rust Equivalent

```rust
impl ValueConv {
    pub fn convert(
        &self,
        raw: f64,
        tables: &TableManager,
        channel_id: u32,
        flow_state: &mut Option<bool>,  // hysteresis state
    ) -> Result<f64, ConversionError> {
        // Step 1: Auto-detect sensor if needed (channels 1100-1723)
        // In Rust this would be done once at channel creation, not every read

        // Step 2: Conversion function path (SDP610 sensors)
        if let Some(conv_fn) = &self.conv_func {
            return self.convert_with_function(raw, conv_fn, flow_state);
        }

        // Step 3: Table lookup path
        if let Some(table_idx) = self.table_index {
            let table = tables.get(table_idx)
                .ok_or(ConversionError::TableNotFound(table_idx))?;  // explicit error

            let (low, high) = self.get_range()
                .ok_or(ConversionError::MissingRange)?;  // explicit error

            let clamped = raw.clamp(low, high);  // std::f64::clamp, no manual if/else

            let cur = table.interpolate(clamped, low, high, self.min()?, self.max()?)?;
            return Ok(self.apply_post_processing(cur));
        }

        // Step 4: Linear scaling path
        let mut cur = raw;
        if let (Some(low), Some(high)) = (self.low, self.high) {
            cur /= high - low;
        }
        if let Some(offset) = self.offset { cur += offset; }
        if let Some(scale) = self.scale { cur *= scale; }
        if let Some(min) = self.min { cur = cur.max(min); }
        if let Some(max) = self.max { cur = cur.min(max); }

        Ok(cur)
    }
}
```

**Rust improvements:**
1. **`Result<f64, ConversionError>`** — errors are explicit return values, not -1 magic
2. **`f64::clamp()`** — standard library, replaces manual if/else chains
3. **`Option<T>` instead of flags** — `if let Some(scale) = self.scale` is self-documenting
4. **Table interpolation is bounds-checked** — can't read past end of table array
5. **Binary search is `slice::binary_search_by()`** — standard library, bug-free

### Table Interpolation

```rust
impl Table {
    pub fn interpolate(
        &self,
        raw: f64,
        low: f64, high: f64,
        min: f64, max: f64,
    ) -> Result<f64, ConversionError> {
        let n = self.values.len();
        if n < 2 { return Err(ConversionError::TableTooSmall); }

        let unit = (max - min) / (n as f64 - 1.0);

        // Binary search for interval
        let idx = self.values.partition_point(|&v| {
            if self.increasing { v < raw as i32 } else { v > raw as i32 }
        });

        // Bounds check is automatic — Rust slices can't overrun
        let idx = idx.saturating_sub(1).min(n - 2);

        let r1 = self.values[idx] as f64;     // bounds-checked
        let r2 = self.values[idx + 1] as f64; // bounds-checked

        let c1 = min + (idx as f64 * unit);
        let c2 = c1 + unit;

        let t = if self.increasing {
            (raw - r1) / (r2 - r1)
        } else {
            (raw - r2) / (r1 - r2)
        };

        Ok(lerp(c1, c2, t.clamp(0.0, 1.0)))
    }
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a * (1.0 - t) + b * t
}
```

---

## 4. Poll / Watch / Notify Subsystems

These are simpler subsystems that follow the same pattern:

| C Subsystem | Lines | Rust Equivalent |
|------------|-------|-----------------|
| `poll.c` (poll_init, poll_add, poll_update, poll_report) | 453 | `PollManager` with `HashMap<u32, PollEntry>` |
| `watch.c` (watch_init, watch_add, watch_remove, check) | 227 | `WatchManager` with `HashMap<u32, Vec<u32>>` |
| `notify.c` (notify_init, notify_add, notify_remove) | 220 | `NotifyManager` with `HashSet<u32>` |

Each C subsystem follows the same pattern:
```c
struct { int nItems; int nCount; ITEM *items; }  // sparse array
xxx_init(s, max)   // malloc + memset
xxx_exit(s)        // free
xxx_add(s, ...)    // linear scan for empty slot
xxx_remove(s, ...) // linear scan + mark unused
xxx_find(s, ...)   // linear scan
```

In Rust, ALL of this becomes `HashMap` or `HashSet` operations — O(1) instead of O(n), no manual memory management, no init/exit lifecycle.

---

## 5. Engine Daemon Lifecycle

### C Current: fork() + pthread

```c
// engine.c:694 — fork daemon
pid_t pid = fork();
if(pid > 0) return 0;  // parent exits

// engine.c:752 — create poll thread
pthread_create(&pollthread, NULL, engine_poll, args);

// engine.c:761 — run main loop (blocks on msgrcv)
engine_loop(args);

// engine.c:771 — join poll thread
pthread_join(pollthread, NULL);
```

### Rust: tokio async tasks

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // No fork() needed — systemd manages the process
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(256);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Signal handling (replaces signal_handler + g_quit global)
    let shutdown = shutdown_tx.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        shutdown.send(true).ok();
    });

    // Poll task (replaces pthread + usleep loop)
    let poll_tx = cmd_tx.clone();
    let mut poll_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(1000));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let _ = poll_tx.send(EngineCommand::Poll).await;
                }
                _ = poll_shutdown.changed() => break,
            }
        }
    });

    // Engine main loop (replaces engine_loop + msgrcv)
    let engine = Engine::load(config)?;
    engine.run(cmd_rx, shutdown_rx).await?;

    Ok(())
}
```

**Rust improvements:**
1. **No `fork()`** — systemd already manages daemon lifecycle; `fork()` is fragile (C++ statics corrupt after fork, as seen in `logger_reinit_after_fork()`)
2. **No `pthread_create`** — `tokio::spawn` is lighter and composable
3. **No `usleep`** — `tokio::time::interval` is precise and cancellable
4. **No global `g_quit`** — `watch::channel` broadcasts shutdown cleanly
5. **No `msgrcv` blocking** — `mpsc::channel` is typed and async
6. **No `atexit` cleanup** — RAII handles everything

---

## 6. Configuration Loading

### C Current: zinc + csv + manual iteration (engine.c:1314-1749, 435 lines)

The `engine_load` function reads `database.zinc` and `points.csv`, cross-references them, and populates channels. It has deeply nested loops with `goto`-like `break` statements and duplicated conversion setup code (virtual vs physical channels share ~100 lines of identical conv setup).

### Rust Equivalent

```rust
pub fn load_config(config: &EngineConfig) -> Result<Engine, EngineError> {
    // Parse zinc grid (replaced by libhaystack)
    let zinc_data = std::fs::read_to_string(&config.zinc_path)?;
    let grid: Grid = zinc::decode::from_str(&zinc_data)?;

    // Parse CSV
    let mut points_reader = csv::Reader::from_path(&config.points_path)?;
    let points: Vec<PointConfig> = points_reader.deserialize().collect::<Result<_, _>>()?;

    // Load tables
    let tables = TableManager::load(&config.tables_path)?;

    // Build channels
    let mut channels = ChannelManager::new();
    for row in grid.rows() {
        let channel_id: u32 = row.get("channel")?;
        let conv = ValueConv::from_zinc_row(&row, &tables)?;  // single function, no duplication

        if row.has("virtualChannel") {
            channels.add(Channel::new_virtual(channel_id, &row, conv)?)?;
        } else if let Some(hw) = points.iter().find(|p| p.channel == channel_id) {
            channels.add(Channel::new_physical(channel_id, &row, hw, conv)?)?;
        } else {
            tracing::warn!(channel = channel_id, "Channel in zinc but not in points.csv");
        }
    }

    Ok(Engine { channels, tables, ..Default::default() })
}
```

**Rust improvements:**
1. **No duplicated conversion setup** — `ValueConv::from_zinc_row()` is called once for both virtual and physical
2. **`?` operator** — error propagation is clean, no nested `if(err<0)` chains
3. **`csv` crate with `deserialize()`** — type-safe CSV parsing, no manual column lookup
4. **libhaystack grid parsing** — replaces custom `zinc.c` parser entirely
5. **`tracing::warn!`** — structured logging, no `printf` + `syslog` split

---

## Summary: Lines of Code Comparison

| Component | C Lines | Rust Lines (est.) | Notes |
|-----------|---------|-------------------|-------|
| engine.c main/loop/daemon | 800 | 200 | tokio replaces fork/pthread/msgrcv |
| engine.c config loading | 435 | 80 | libhaystack + csv crate |
| engine.c CLI/commands | 600 | 150 | clap crate |
| engine.c misc (version, help, etc.) | 300 | 50 | trivial |
| channel.c types + container | 320 | 60 | HashMap replaces sparse array |
| channel.c read/write dispatch | 400 | 150 | match on enum, no switch fallthrough |
| channel.c hw open/close/export | 350 | 100 | HAL traits |
| channel.c filters (smooth, rate limit) | 300 | 120 | same algorithms, safer types |
| value.c conversion | 600 | 250 | Option<T>, Result<T>, clamp() |
| value.h types | 130 | 80 | enum instead of fn pointer |
| table.c interpolation | 350 | 100 | slice::partition_point() |
| poll.c | 453 | 80 | HashMap, tokio interval |
| watch.c | 227 | 50 | HashMap<u32, Vec<u32>> |
| notify.c | 220 | 40 | HashSet<u32> |
| **TOTAL** | **~5,500** | **~1,510** | **~73% reduction** |

The reduction comes from:
- HashMap/HashSet replacing manual array management (~40% of C code is init/exit/find/alloc)
- Standard library (`clamp`, `partition_point`, `Option`, `Result`)
- Crates (csv, libhaystack) replacing custom parsers
- tokio replacing fork/pthread/message queue boilerplate
- RAII eliminating cleanup code
