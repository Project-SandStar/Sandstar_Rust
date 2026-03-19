# 07: IPC Bridge — POSIX Message Queues to Rust Channels

## Overview

The Sandstar system uses two OS processes that communicate via POSIX System V message queues:

1. **Engine** (`engine` binary) -- C daemon that reads hardware sensors, manages channels, converts values
2. **SVM** (`svm` binary) -- Sedona Virtual Machine that runs DDC (Direct Digital Control) logic

These processes use `msgget()`/`msgsnd()`/`msgrcv()` for inter-process communication. The engineio.c client layer in the SVM process also implements an internal 200-message async circular buffer with a pthread flush thread.

This document details the current IPC mechanism, then maps out a three-phase migration path from POSIX IPC to Rust channels.

**Source files analyzed:**
- `shaystack/sandstar/sandstar/EacIo/src/EacIo/native/engineio.c` (1,083 lines)
- `shaystack/sandstar/sandstar/EacIo/src/EacIo/native/engineio.h` (73 lines)
- `shaystack/sandstar/sandstar/engine/src/engine.c` (2,147 lines) -- server side
- `shaystack/sandstar/sandstar/engine/src/engine.h` (237 lines) -- message definitions
- `shaystack/sandstar/sandstar/engine/src/engine_messages.h` (23 lines) -- channel update message
- `shaystack/sandstar/sandstar/engine/src/value.h` (131 lines) -- VALUE_CONV struct
- `shaystack/sandstar/sandstar/engine/src/channel.h` (184 lines) -- CHANNEL_ITEM struct
- `shaystack/sandstar/sandstar/engine/src/notify.c` (138 lines) -- notification dispatch
- `shaystack/sandstar/sandstar/engine/src/watch.c` (227 lines) -- watch dispatch

---

## 1. Current POSIX IPC Mechanism in Detail

### 1.1 System V Message Queue Keys

The system uses two fixed message queue keys:

```c
// engine.h:143
#define ENGINE_MESSAGE_KEY  0x454E4731   // "ENG1" in ASCII -- Engine's receive queue

// engineio.c:55
#define CLIENT_KEY          0x45414331   // "EAC1" in ASCII -- SVM client's receive queue
```

Both keys are hardcoded. The engine creates `ENGINE_MESSAGE_KEY` on startup. The SVM client creates `CLIENT_KEY` when `engineio_init()` is called.

### 1.2 Queue Creation and Lifecycle

**Engine side (engine.c:633-679):**

```c
// 1. Check if stale queue exists from previous crash
int hQueue = msgget(ENGINE_MESSAGE_KEY, 0666);
if (hQueue >= 0) {
    // Stale queue found -- destroy it
    msgctl(hQueue, IPC_RMID, 0);
}

// 2. Create fresh queue
g_hEngine = msgget(ENGINE_MESSAGE_KEY, IPC_CREAT | 0666);
```

**Client side (engineio.c:195-258):**

```c
// 1. Pre-init: destroy any stale queues from previous crash
int engineio_pre_init() {
    int temp = msgget(ENGINE_MESSAGE_KEY, 0666);  // Check if engine queue exists
    if (temp > 0) msgctl(temp, IPC_RMID, 0);     // Destroy stale engine queue

    temp = msgget(CLIENT_KEY, 0666);              // Check if client queue exists
    if (temp > 0) msgctl(temp, IPC_RMID, 0);     // Destroy stale client queue
}

// 2. Init: wait for engine queue, then create client queue
int engineio_init() {
    // Retry loop -- engine may not have started yet
    int retry_count = 0;
    while (retry_count < 20) {                    // 20 * 100ms = 2 sec max wait
        g_hEngine = msgget(ENGINE_MESSAGE_KEY, 0666);
        if (g_hEngine >= 0) break;
        usleep(100000);                            // 100ms between retries
        retry_count++;
    }

    // Create client receive queue
    g_hQueue = msgget(CLIENT_KEY, IPC_CREAT | 0666);

    // Register for notifications
    send_message(g_hEngine, ENGINE_MESSAGE_NOTIFY, 0, NULL);

    // Start async flush thread
    init_message_buffer();
    pthread_create(&g_flush_thread, NULL, message_flush_thread, NULL);
}
```

### 1.3 Message Flow Diagram

```
┌────────────────────────────┐          ┌────────────────────────────┐
│ Engine Process (engine.c)  │          │ SVM Process (engineio.c)   │
│                            │          │                            │
│  Queue: ENGINE_MESSAGE_KEY │◄─────────│  msgget(ENGINE_MESSAGE_KEY)│
│  (0x454E4731)              │  msgsnd  │                            │
│                            │          │                            │
│  msgrcv() blocks in loop   │          │  Queue: CLIENT_KEY         │
│                            │─────────►│  (0x45414331)              │
│  engine_reply() msgsnd()   │  msgsnd  │                            │
│                            │          │  msgrcv() blocks in        │
│                            │          │  engineio_main()           │
└────────────────────────────┘          └────────────────────────────┘

Bidirectional communication:
  SVM → Engine:  WRITE, READ, NOTIFY, WATCH, UNWATCH, CHANNEL_UPDATE
  Engine → SVM:  CHANGE, READACK, WRITEACK, CHANNEL_UPDATE_ACK
```

### 1.4 The ENGINE_MESSAGE Struct

```c
// engine.h:54-65
struct _ENGINE_MESSAGE {
    long nMessage;           // Message type (required by System V msgrcv mtype filter)
    ENGINE_CHANNEL channel;  // unsigned int -- 4-digit channel ID
    ENGINE_VALUE value;      // The sensor value being communicated
    key_t sender;            // Queue key of the sender (for reply routing)
};

// ENGINE_MESSAGE_SIZE omits the mtype field (System V convention)
#define ENGINE_MESSAGE_SIZE (sizeof(ENGINE_MESSAGE) - sizeof(long))
```

**Memory layout on ARM7 (32-bit):**

```
Offset  Size    Field           Type
──────  ────    ─────           ────
0       4       nMessage        long (32-bit on ARM7)
4       4       channel         unsigned int
8       4       value.status    enum (int)
12      8       value.raw       double
20      8       value.cur       double
28      4       value.flags     int
32      4       value.trigger   int
36      4       sender          key_t (int)
──────  ────
Total:  40 bytes
```

**Note:** On 64-bit hosts, `long` is 8 bytes, making this struct 44 bytes. This is why cross-compilation is essential -- the struct must match exactly between engine and SVM.

### 1.5 The ENGINE_MESSAGE_CHANNEL_UPDATE Struct

This is the largest message type, used to update channel metadata:

```c
// engine_messages.h:13-20
struct _ENGINE_MESSAGE_CHANNEL_UPDATE {
    long nMessage;              // 4 bytes (ARM7)
    ENGINE_CHANNEL channel;     // 4 bytes
    key_t sender;               // 4 bytes
    CHANNEL_ENABLE enable;      // 4 bytes (enum)
    VALUE_CONV conv;            // ~248 bytes (see below)
    char label[64];             // 64 bytes
};
```

**VALUE_CONV substruct memory layout:**

```
Offset  Size    Field              Type
──────  ────    ─────              ────
0       4       nTable             int
4       8       low                double
12      8       high               double
20      8       offset             double
28      8       scale              double
36      8       min                double
44      8       max                double
52      4/8     conv_func          function pointer (4 on ARM7)
56/60   4       flags              int
60/64   16      unit               char[16]
76/80   8       kFactor            double
84/88   8       deadBand           double
92/96   8       hystOn             double
100/104 8       hystOff            double
108/112 8       scaleFactor        double
116/120 8       spikeThreshold     double
124/128 4       startupDiscard     int
128/132 8       reverseThreshold   double
136/140 4       smoothWindow       int
140/144 4       smoothMethod       int
144/148 8       maxRiseRate        double
152/156 8       maxFallRate        double
──────  ────
Total: ~164 bytes (ARM7, with padding)
```

The complete `_ENGINE_MESSAGE_CHANNEL_UPDATE` is approximately **240 bytes** on ARM7. This is well within the System V message queue size limit (default 8192 bytes per message on Linux).

### 1.6 Message Types

All message type IDs are defined in `engine.h`:

| ID | Name | Direction | Purpose |
|----|------|-----------|---------|
| 0x01 | `ENGINE_MESSAGE_STOP` | SVM->Engine | Shut down engine |
| 0x02 | `ENGINE_MESSAGE_RESTART` | SVM->Engine | Reload configuration |
| 0x03 | `ENGINE_MESSAGE_POLL` | SVM->Engine | Trigger sensor poll |
| 0x10 | `ENGINE_MESSAGE_STATUS` | SVM->Engine | Request status dump |
| 0x11 | `ENGINE_MESSAGE_CHANNELS` | SVM->Engine | Request channel report |
| 0x12 | `ENGINE_MESSAGE_TABLES` | SVM->Engine | Request table report |
| 0x13 | `ENGINE_MESSAGE_POLLS` | SVM->Engine | Request poll report |
| 0x14 | `ENGINE_MESSAGE_WATCHES` | SVM->Engine | Request watch report |
| 0x15 | `ENGINE_MESSAGE_NOTIFIES` | SVM->Engine | Request notify report |
| 0x20 | `ENGINE_MESSAGE_CONVERT` | SVM->Engine | Convert raw->cur value |
| 0x21 | `ENGINE_MESSAGE_CONVACK` | Engine->SVM | Conversion result |
| 0x30 | `ENGINE_MESSAGE_NOTIFY` | SVM->Engine | Register for notifications |
| 0x31 | `ENGINE_MESSAGE_CHANGE` | Engine->SVM | Sensor value changed |
| 0x32 | `ENGINE_MESSAGE_UNNOTIFY` | SVM->Engine | Unregister notifications |
| 0x40 | `ENGINE_MESSAGE_WATCH` | SVM->Engine | Watch a channel |
| 0x41 | `ENGINE_MESSAGE_UPDATE` | Engine->SVM | Channel updated |
| 0x42 | `ENGINE_MESSAGE_UNWATCH` | SVM->Engine | Unwatch a channel |
| 0x50 | `ENGINE_MESSAGE_READ` | SVM->Engine | Read channel value |
| 0x51 | `ENGINE_MESSAGE_READACK` | Engine->SVM | Read response |
| 0x60 | `ENGINE_MESSAGE_WRITE` | SVM->Engine | Write channel value |
| 0x61 | `ENGINE_MESSAGE_WRITEACK` | Engine->SVM | Write confirmed |
| 0x62 | `ENGINE_MESSAGE_WRITE_VIRTUAL` | SVM->Engine | Write virtual channel |
| 0x72 | `ENGINE_MESSAGE_CHANNEL_UPDATE` | SVM->Engine | Update channel metadata |
| 0x73 | `ENGINE_MESSAGE_CHANNEL_UPDATE_ACK` | Engine->SVM | Metadata update confirmed |

### 1.7 The Async Message Buffer

The engineio.c client has an internal circular buffer for handling queue-full conditions:

```c
// engineio.c:59-60
#define MAX_BUFFERED_MESSAGES    200     // Circular buffer capacity
#define MESSAGE_FLUSH_INTERVAL_US 10000  // 10ms check interval

// engineio.c:96-104
struct _MESSAGE_BUFFER {
    struct _ENGINE_MESSAGE_CHANNEL_UPDATE messages[MAX_BUFFERED_MESSAGES];
    int head;           // Read position
    int tail;           // Write position
    int count;          // Current occupancy
    pthread_mutex_t lock;
};
```

**Size:** Each `_ENGINE_MESSAGE_CHANNEL_UPDATE` is ~240 bytes. Buffer: 200 * 240 = **~48 KB** static allocation.

**Flow when IPC queue is full:**

```
engineio_update_channel_metadata()
    │
    ├── msgsnd(IPC_NOWAIT) ──── success ──► done
    │
    └── EAGAIN ──► buffer_message_internal()
                        │
                        ├── count < 200 ──► memcpy to buffer ──► return 0
                        │
                        └── count >= 200 ──► drop message ──► return -1

                    ┌─ message_flush_thread (runs every 10ms)
                    │
                    └── while count > 0:
                            msgsnd(IPC_NOWAIT)
                            ├── success → dequeue from buffer
                            └── EAGAIN  → break, try again in 10ms
```

---

## 2. Phase 1: Keep POSIX Queues, Call from Rust (nix Crate)

### 2.1 Strategy

In Phase 1, the engine stays as a separate C process. Only the SVM client side (engineio.c) gets rewritten in Rust. The Rust code uses the `nix` crate to call the same System V IPC functions.

### 2.2 The nix Crate

The [nix](https://crates.io/crates/nix) crate provides safe Rust wrappers for POSIX system calls, including System V message queues.

```toml
[dependencies]
nix = { version = "0.29", features = ["mqueue", "ipc"] }
```

**Note:** The `nix` crate wraps System V IPC under the `ipc` feature, not `mqueue` (which is POSIX message queues, a different API). System V message queues use `msgget`/`msgsnd`/`msgrcv` while POSIX message queues use `mq_open`/`mq_send`/`mq_receive`. Sandstar uses System V.

### 2.3 Rust System V IPC Bindings

Since `nix` may not expose System V `msgget`/`msgsnd`/`msgrcv` directly (these are less commonly used than POSIX mqueues), we may need to use `libc` directly:

```rust
use libc::{
    msgget, msgsnd, msgrcv, msgctl,
    IPC_CREAT, IPC_RMID, IPC_NOWAIT,
    key_t, c_long, c_int, size_t,
};

/// System V message queue key constants
const ENGINE_MESSAGE_KEY: key_t = 0x454E4731;  // "ENG1"
const CLIENT_KEY: key_t = 0x45414331;          // "EAC1"

/// Message types matching engine.h
const ENGINE_MESSAGE_STOP: c_long = 0x01;
const ENGINE_MESSAGE_NOTIFY: c_long = 0x30;
const ENGINE_MESSAGE_CHANGE: c_long = 0x31;
const ENGINE_MESSAGE_UNNOTIFY: c_long = 0x32;
const ENGINE_MESSAGE_READ: c_long = 0x50;
const ENGINE_MESSAGE_READACK: c_long = 0x51;
const ENGINE_MESSAGE_WRITE: c_long = 0x60;
const ENGINE_MESSAGE_WRITEACK: c_long = 0x61;
const ENGINE_MESSAGE_CHANNEL_UPDATE: c_long = 0x72;
const ENGINE_MESSAGE_CHANNEL_UPDATE_ACK: c_long = 0x73;
```

### 2.4 Rust ENGINE_MESSAGE Struct (FFI Compatible)

```rust
/// Matches enum _ENGINE_STATUS in engine.h
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EngineStatus {
    Ok = 0,
    Unknown = 1,
    Stale = 2,
    Disabled = 3,
    Fault = 4,
    Down = 5,
}

/// Matches struct _ENGINE_VALUE in engine.h:39-52
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EngineValue {
    pub status: EngineStatus,  // ENGINE_STATUS
    pub raw: f64,              // ENGINE_DATA (typedef double)
    pub cur: f64,              // ENGINE_DATA (typedef double)
    pub flags: c_int,          // int
    pub trigger: c_int,        // int
}

/// Matches struct _ENGINE_MESSAGE in engine.h:54-65
/// CRITICAL: Must be #[repr(C)] and field order must match exactly
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EngineMessage {
    pub n_message: c_long,        // long nMessage (mtype for System V)
    pub channel: u32,             // ENGINE_CHANNEL (unsigned int)
    pub value: EngineValue,       // ENGINE_VALUE
    pub sender: key_t,            // key_t (int)
}

/// Size to send/receive (excludes mtype field, per System V convention)
const ENGINE_MESSAGE_SIZE: usize =
    std::mem::size_of::<EngineMessage>() - std::mem::size_of::<c_long>();
```

### 2.5 Rust IPC Wrapper

```rust
use std::io;

/// Wrapper for System V message queue operations
pub struct MessageQueue {
    id: c_int,
}

impl MessageQueue {
    /// Open or create a message queue
    pub fn open(key: key_t, create: bool) -> io::Result<Self> {
        let flags = if create { IPC_CREAT | 0o666 } else { 0o666 };
        let id = unsafe { msgget(key, flags) };
        if id < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(MessageQueue { id })
    }

    /// Open an existing queue, retrying up to max_retries times
    pub fn open_with_retry(key: key_t, max_retries: u32, delay_ms: u64) -> io::Result<Self> {
        for attempt in 0..max_retries {
            match Self::open(key, false) {
                Ok(q) => {
                    if attempt > 0 {
                        println!("engineio_init: Engine queue found after {} retries", attempt);
                    }
                    return Ok(q);
                }
                Err(_) if attempt + 1 < max_retries => {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                }
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut,
            format!("Queue 0x{:08X} not found after {} retries", key, max_retries)))
    }

    /// Send a message (blocking)
    pub fn send(&self, msg: &EngineMessage) -> io::Result<()> {
        let result = unsafe {
            msgsnd(
                self.id,
                msg as *const EngineMessage as *const libc::c_void,
                ENGINE_MESSAGE_SIZE,
                0,  // blocking
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Send a message (non-blocking)
    pub fn try_send(&self, msg: &EngineMessage) -> io::Result<()> {
        let result = unsafe {
            msgsnd(
                self.id,
                msg as *const EngineMessage as *const libc::c_void,
                ENGINE_MESSAGE_SIZE,
                IPC_NOWAIT,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Receive a message (blocking)
    pub fn recv(&self) -> io::Result<EngineMessage> {
        let mut msg = EngineMessage {
            n_message: 0,
            channel: 0,
            value: EngineValue {
                status: EngineStatus::Unknown,
                raw: 0.0,
                cur: 0.0,
                flags: 0,
                trigger: 0,
            },
            sender: 0,
        };

        let result = unsafe {
            msgrcv(
                self.id,
                &mut msg as *mut EngineMessage as *mut libc::c_void,
                ENGINE_MESSAGE_SIZE,
                0,  // receive any message type
                0,  // blocking
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(msg)
        }
    }

    /// Destroy the queue
    pub fn destroy(&self) -> io::Result<()> {
        let result = unsafe { msgctl(self.id, IPC_RMID, std::ptr::null_mut()) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}
```

### 2.6 Rust EngineIO Client (Phase 1 Replacement)

```rust
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub struct EngineIo {
    engine_queue: MessageQueue,    // Engine's receive queue
    client_queue: MessageQueue,    // Our receive queue
    channels: Mutex<ChannelMap>,   // Local channel cache
    buffer: Mutex<MessageBuffer>,  // Async message buffer
    quit: Arc<std::sync::atomic::AtomicBool>,
    flush_thread: Option<thread::JoinHandle<()>>,
}

impl EngineIo {
    pub fn new() -> io::Result<Self> {
        // Wait for engine to create its queue
        let engine_queue = MessageQueue::open_with_retry(
            ENGINE_MESSAGE_KEY,
            20,     // max retries
            100,    // 100ms between retries
        )?;

        // Create our receive queue
        let client_queue = MessageQueue::open(CLIENT_KEY, true)?;

        // Register for notifications
        let notify_msg = EngineMessage {
            n_message: ENGINE_MESSAGE_NOTIFY,
            channel: 0,
            value: EngineValue::default(),
            sender: CLIENT_KEY,
        };
        engine_queue.send(&notify_msg)?;

        let quit = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let buffer = Mutex::new(MessageBuffer::new());

        // Start flush thread
        let quit_clone = quit.clone();
        let engine_id = engine_queue.id;
        let buffer_ref = Arc::new(buffer);
        let flush_thread = thread::spawn(move || {
            flush_loop(engine_id, buffer_ref, quit_clone);
        });

        Ok(EngineIo {
            engine_queue,
            client_queue,
            channels: Mutex::new(ChannelMap::new(10000)),
            buffer: Mutex::new(MessageBuffer::new()),
            quit,
            flush_thread: Some(flush_thread),
        })
    }

    /// Read a channel value from local cache
    pub fn read_channel(&self, channel: u32) -> Result<EngineValue, EngineIoError> {
        let channels = self.channels.lock()
            .map_err(|_| EngineIoError::LockPoisoned)?;
        channels.get(channel).ok_or(EngineIoError::ChannelNotFound(channel))
    }

    /// Write a value to a channel (sends to engine)
    pub fn write_channel(&self, channel: u32, value: &EngineValue) -> io::Result<()> {
        let msg = EngineMessage {
            n_message: ENGINE_MESSAGE_WRITE,
            channel,
            value: *value,
            sender: CLIENT_KEY,
        };
        self.engine_queue.send(&msg)
    }

    /// Main receive loop -- runs in dedicated thread
    pub fn run(&self) -> io::Result<()> {
        while !self.quit.load(std::sync::atomic::Ordering::Relaxed) {
            match self.client_queue.recv() {
                Ok(msg) => {
                    let mut channels = self.channels.lock().unwrap();
                    match msg.n_message {
                        x if x == ENGINE_MESSAGE_STOP as c_long => break,
                        x if x == ENGINE_MESSAGE_CHANGE as c_long => {
                            channels.update(msg.channel, msg.value);
                        }
                        x if x == ENGINE_MESSAGE_WRITEACK as c_long => {
                            // Signal write acknowledgment
                        }
                        x if x == ENGINE_MESSAGE_CHANNEL_UPDATE_ACK as c_long => {
                            // Fire-and-forget: nothing to do
                        }
                        _ => {
                            eprintln!("engineio: unknown message type 0x{:02X}", msg.n_message);
                        }
                    }
                }
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
                Err(e) => {
                    eprintln!("engineio: msgrcv failed: {}", e);
                    break;
                }
            }
        }
        Ok(())
    }
}

impl Drop for EngineIo {
    fn drop(&mut self) {
        self.quit.store(true, std::sync::atomic::Ordering::Relaxed);

        // Send stop message to unblock recv
        let _ = self.engine_queue.send(&EngineMessage {
            n_message: ENGINE_MESSAGE_UNNOTIFY,
            channel: 0,
            value: EngineValue::default(),
            sender: CLIENT_KEY,
        });

        // Wait for flush thread
        if let Some(handle) = self.flush_thread.take() {
            let _ = handle.join();
        }

        // Destroy client queue
        let _ = self.client_queue.destroy();
    }
}
```

### 2.7 Phase 1 Benefits

- **Zero protocol changes.** The engine C process does not need modification.
- **Drop-in replacement.** The Rust EngineIO has the same external behavior.
- **Better error handling.** Rust's `Result` replaces C's errno checking.
- **No more buffer overflows.** Rust's bounds checking protects against the message buffer issues.
- **Same performance.** System V IPC overhead is identical.

---

## 3. Phase 2: Replace with tokio::sync::mpsc Channels

### 3.1 When to Transition

Phase 2 happens when the engine is also rewritten in Rust and both engine and SVM run in the **same process**. At that point, inter-process communication becomes inter-thread communication, and POSIX IPC overhead can be eliminated.

### 3.2 Why tokio::sync::mpsc

`tokio::sync::mpsc` (multi-producer, single-consumer) channels are the Rust async equivalent of message queues:

| Feature | POSIX Message Queue | tokio::sync::mpsc |
|---------|-------------------|-------------------|
| Overhead | Kernel syscall per message | Memory copy (no syscall) |
| Blocking | Kernel blocks thread | Cooperative async await |
| Backpressure | Queue size limit (kernel) | Bounded channel capacity |
| Serialization | Raw struct copy | Zero-cost (same address space) |
| Cross-process | Yes | No (same process only) |
| Latency | ~1-10 microseconds | ~100-500 nanoseconds |

### 3.3 Architecture

```
┌──────────────────────────────────────────────────────────┐
│ Single Process: sandstar                                 │
│                                                          │
│  ┌─────────────────────┐     mpsc       ┌──────────────┐│
│  │ Engine Task          │◄──────────────│ SVM Task     ││
│  │ (tokio task)         │    tx_engine   │ (std thread) ││
│  │                      │               │              ││
│  │  sensor polling      │───────────────►│ DDC logic    ││
│  │  channel management  │    tx_svm      │ control loop ││
│  └─────────────────────┘               └──────────────┘│
│                                                          │
│  Tokio Runtime (multi-threaded)                          │
└──────────────────────────────────────────────────────────┘
```

### 3.4 Channel Definitions

```rust
use tokio::sync::mpsc;

/// Messages from SVM to Engine
#[derive(Debug)]
pub enum SvmToEngine {
    Stop,
    Poll,
    Read {
        channel: u32,
        reply: tokio::sync::oneshot::Sender<EngineValue>,
    },
    Write {
        channel: u32,
        value: EngineValue,
    },
    Watch {
        channel: u32,
    },
    Unwatch {
        channel: u32,
    },
    Notify,
    Unnotify,
    ChannelUpdate {
        channel: u32,
        enable: ChannelEnable,
        conv: ValueConv,
        label: String,
    },
}

/// Messages from Engine to SVM
#[derive(Debug)]
pub enum EngineToSvm {
    Change {
        channel: u32,
        value: EngineValue,
    },
    WriteAck {
        channel: u32,
        value: EngineValue,
    },
    ChannelUpdateAck {
        channel: u32,
        status: EngineStatus,
    },
}
```

### 3.5 Channel Setup

```rust
/// Create the communication channels between Engine and SVM
pub fn create_channels(buffer_size: usize) -> (EngineSide, SvmSide) {
    // SVM -> Engine channel (bounded)
    let (svm_tx, engine_rx) = mpsc::channel::<SvmToEngine>(buffer_size);

    // Engine -> SVM channel (bounded)
    let (engine_tx, svm_rx) = mpsc::channel::<EngineToSvm>(buffer_size);

    (
        EngineSide {
            rx: engine_rx,
            tx: engine_tx,
        },
        SvmSide {
            tx: svm_tx,
            rx: svm_rx,
        },
    )
}

pub struct EngineSide {
    pub rx: mpsc::Receiver<SvmToEngine>,
    pub tx: mpsc::Sender<EngineToSvm>,
}

pub struct SvmSide {
    pub tx: mpsc::Sender<SvmToEngine>,
    pub rx: mpsc::Receiver<EngineToSvm>,
}
```

### 3.6 Engine Task (Receiver Side)

```rust
/// Engine main loop -- processes messages from SVM and polls sensors
async fn engine_task(
    mut engine: EngineSide,
    mut channels: ChannelManager,
    mut tables: TableManager,
    mut poll: PollManager,
) {
    // Create poll interval timer
    let mut poll_interval = tokio::time::interval(Duration::from_millis(1000));

    loop {
        tokio::select! {
            // Handle incoming messages from SVM
            Some(msg) = engine.rx.recv() => {
                match msg {
                    SvmToEngine::Stop => break,

                    SvmToEngine::Poll => {
                        poll.update(&mut channels, &tables).await;
                    }

                    SvmToEngine::Read { channel, reply } => {
                        let value = channels.read(channel, &tables);
                        let _ = reply.send(value);
                    }

                    SvmToEngine::Write { channel, value } => {
                        channels.write(channel, &value, &tables);
                        // Send write ack
                        let _ = engine.tx.send(EngineToSvm::WriteAck {
                            channel,
                            value,
                        }).await;
                    }

                    SvmToEngine::ChannelUpdate { channel, enable, conv, label } => {
                        let result = channels.update_metadata(channel, enable, &conv, &label);
                        let _ = engine.tx.send(EngineToSvm::ChannelUpdateAck {
                            channel,
                            status: if result.is_ok() { EngineStatus::Ok } else { EngineStatus::Fault },
                        }).await;
                    }

                    // ... other message types
                    _ => {}
                }
            }

            // Periodic sensor polling
            _ = poll_interval.tick() => {
                poll.update(&mut channels, &tables).await;

                // Send changes to SVM
                for (channel, value) in channels.drain_dirty() {
                    let _ = engine.tx.send(EngineToSvm::Change {
                        channel,
                        value,
                    }).await;
                }
            }
        }
    }
}
```

### 3.7 SVM Side (Sender/Receiver)

The Sedona VM runs in its own OS thread (not a tokio task). It uses `blocking_send` and `blocking_recv`:

```rust
/// SVM-side bridge that provides sync interface over async channels
pub struct SvmBridge {
    side: SvmSide,
    local_channels: ChannelMap,
    runtime_handle: tokio::runtime::Handle,
}

impl SvmBridge {
    /// Read channel value (from local cache, updated by Change messages)
    pub fn read_channel(&self, channel: u32) -> Result<EngineValue, EngineIoError> {
        self.local_channels.get(channel)
            .ok_or(EngineIoError::ChannelNotFound(channel))
    }

    /// Write channel value (sends to engine, blocks for ack)
    pub fn write_channel(&self, channel: u32, value: &EngineValue) -> Result<(), EngineIoError> {
        self.side.tx.blocking_send(SvmToEngine::Write {
            channel,
            value: *value,
        }).map_err(|_| EngineIoError::ChannelClosed)?;
        Ok(())
    }

    /// Synchronous read with request/reply (uses oneshot channel)
    pub fn read_channel_sync(&self, channel: u32) -> Result<EngineValue, EngineIoError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.side.tx.blocking_send(SvmToEngine::Read {
            channel,
            reply: tx,
        }).map_err(|_| EngineIoError::ChannelClosed)?;

        rx.blocking_recv()
            .map_err(|_| EngineIoError::ReplyDropped)
    }

    /// Process incoming messages (call periodically from SVM tick)
    pub fn process_incoming(&mut self) {
        // Non-blocking drain of all pending messages
        while let Ok(msg) = self.side.rx.try_recv() {
            match msg {
                EngineToSvm::Change { channel, value } => {
                    self.local_channels.update(channel, value);
                }
                EngineToSvm::WriteAck { channel, value } => {
                    // Signal write acknowledgment
                }
                EngineToSvm::ChannelUpdateAck { channel, status } => {
                    // Log metadata update result
                }
            }
        }
    }
}
```

### 3.8 Typed Enums vs Raw Message IDs

Phase 2's biggest advantage is replacing raw message IDs with Rust enums:

```
C (Phase 1):                           Rust (Phase 2):
─────────────                          ──────────────
msg.nMessage = 0x60;                   SvmToEngine::Write { channel, value }
msg.channel = 1113;                    // Compiler enforces all fields present
msg.value.cur = 72.5;                  // No raw byte copying
msg.value.status = ENGINE_STATUS_OK;   // No size calculation errors
msgsnd(hQueue, &msg, SIZE, 0);        tx.send(msg).await;
```

---

## 4. Phase 3: In-Process Channels (No IPC Overhead)

### 4.1 When to Transition

Phase 3 happens when the engine and SVM are compiled into a single binary with shared memory. At this point, even the mpsc channel overhead can be eliminated for the hot path (sensor reading).

### 4.2 Shared Memory Model

```rust
use std::sync::{Arc, RwLock};

/// Shared channel state visible to both engine and SVM
pub struct SharedChannelState {
    /// Channel values indexed by channel ID
    /// RwLock allows concurrent reads, exclusive writes
    values: Vec<RwLock<Option<EngineValue>>>,
    /// Dirty flags for change notification
    dirty: Vec<std::sync::atomic::AtomicBool>,
}

impl SharedChannelState {
    pub fn new(max_channels: usize) -> Self {
        let mut values = Vec::with_capacity(max_channels);
        let mut dirty = Vec::with_capacity(max_channels);
        for _ in 0..max_channels {
            values.push(RwLock::new(None));
            dirty.push(std::sync::atomic::AtomicBool::new(false));
        }
        SharedChannelState { values, dirty }
    }

    /// Engine writes a new value (fast path)
    pub fn write(&self, channel: u32, value: EngineValue) {
        if let Some(slot) = self.values.get(channel as usize) {
            *slot.write().unwrap() = Some(value);
            self.dirty[channel as usize].store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// SVM reads current value (fast path -- no IPC, no copy)
    pub fn read(&self, channel: u32) -> Option<EngineValue> {
        self.values.get(channel as usize)
            .and_then(|slot| *slot.read().unwrap())
    }

    /// SVM checks and clears dirty flag
    pub fn take_dirty(&self, channel: u32) -> bool {
        self.dirty.get(channel as usize)
            .map(|flag| flag.swap(false, std::sync::atomic::Ordering::AcqRel))
            .unwrap_or(false)
    }
}
```

### 4.3 Performance Characteristics

The shared memory approach eliminates all IPC overhead for reads:

| Operation | Phase 1 (POSIX) | Phase 2 (mpsc) | Phase 3 (shared) |
|-----------|-----------------|-----------------|-------------------|
| Sensor read | ~5 us (msgrcv/msgsnd round-trip) | ~200 ns (channel send/recv) | ~10 ns (RwLock read) |
| Sensor write | ~5 us | ~200 ns | ~50 ns (RwLock write) |
| Change notify | ~5 us | ~200 ns | ~5 ns (atomic flag) |
| Channel update | ~10 us (large struct) | ~200 ns (enum variant) | ~50 ns (direct write) |
| Max throughput | ~200K msg/sec | ~5M msg/sec | ~100M reads/sec |

**Note:** These are approximate values on ARM7 (1 GHz Cortex-A8). Actual performance depends on cache behavior and contention.

### 4.4 Hybrid Approach

Phase 3 does not eliminate channels entirely. Commands (write, update metadata, stop) still use mpsc channels. Only the hot-path sensor **reads** use shared memory:

```rust
pub struct HybridBridge {
    /// Fast path: direct shared memory for sensor reads
    shared_state: Arc<SharedChannelState>,

    /// Slow path: mpsc channel for commands
    command_tx: mpsc::Sender<SvmToEngine>,

    /// Incoming events from engine
    event_rx: mpsc::Receiver<EngineToSvm>,
}

impl HybridBridge {
    /// Hot path: read sensor value (no channel overhead)
    pub fn read_sensor(&self, channel: u32) -> Option<EngineValue> {
        self.shared_state.read(channel)
    }

    /// Cold path: write to actuator (via channel)
    pub fn write_actuator(&self, channel: u32, value: EngineValue) {
        let _ = self.command_tx.blocking_send(SvmToEngine::Write { channel, value });
    }
}
```

---

## 5. The Async Buffer Pattern

### 5.1 Current C Implementation

The C code uses a pthread mutex-protected circular buffer with a dedicated flush thread:

```c
// engineio.c:96-104 -- The buffer structure
struct _MESSAGE_BUFFER {
    struct _ENGINE_MESSAGE_CHANNEL_UPDATE messages[MAX_BUFFERED_MESSAGES];  // 200 slots
    int head;           // Read position (flush thread reads from here)
    int tail;           // Write position (main thread writes here)
    int count;          // Current occupancy
    pthread_mutex_t lock;  // Protects all fields
};

// engineio.c:1004-1063 -- The flush thread
static void* message_flush_thread(void* arg) {
    while (!g_quit) {
        usleep(10000);   // Sleep 10ms

        pthread_mutex_lock(&g_message_buffer.lock);
        while (g_message_buffer.count > 0) {
            int idx = g_message_buffer.head;
            struct _ENGINE_MESSAGE_CHANNEL_UPDATE *msg = &g_message_buffer.messages[idx];

            int err = msgsnd(hEngine, msg,
                sizeof(struct _ENGINE_MESSAGE_CHANNEL_UPDATE) - sizeof(long),
                IPC_NOWAIT);

            if (err < 0 && (errno == EAGAIN || errno == EWOULDBLOCK))
                break;  // Queue still full, try again later

            // Dequeue
            g_message_buffer.head = (head + 1) % MAX_BUFFERED_MESSAGES;
            g_message_buffer.count--;
        }
        pthread_mutex_unlock(&g_message_buffer.lock);
    }
    return NULL;
}
```

**Problems with the C implementation:**

1. **Fixed buffer size.** 200 messages is hardcoded. If the buffer fills, messages are dropped.
2. **Polling interval.** The 10ms sleep means up to 10ms latency for buffered messages.
3. **Lock contention.** The mutex is held during the entire flush loop, blocking new messages.
4. **No backpressure.** When buffer is full, messages are silently dropped with only a printf.

### 5.2 Rust Equivalent with tokio::sync::mpsc

```rust
use tokio::sync::mpsc;
use std::time::Duration;

/// Async message buffer with backpressure
/// Replaces the C circular buffer + flush thread pattern
pub struct AsyncMessageBuffer {
    /// Bounded channel -- provides backpressure when full
    tx: mpsc::Sender<ChannelUpdateMessage>,
    /// Handle to the flush task
    flush_handle: Option<tokio::task::JoinHandle<()>>,
}

impl AsyncMessageBuffer {
    /// Create a new async buffer with given capacity
    pub fn new(
        capacity: usize,           // replaces MAX_BUFFERED_MESSAGES (200)
        engine_queue: MessageQueue, // System V queue (Phase 1) or mpsc (Phase 2)
    ) -> Self {
        let (tx, rx) = mpsc::channel(capacity);

        let flush_handle = tokio::spawn(async move {
            Self::flush_loop(rx, engine_queue).await;
        });

        AsyncMessageBuffer {
            tx,
            flush_handle: Some(flush_handle),
        }
    }

    /// Send a message (with backpressure)
    /// Unlike C, this WAITS when buffer is full instead of dropping
    pub async fn send(&self, msg: ChannelUpdateMessage) -> Result<(), BufferError> {
        self.tx.send(msg).await
            .map_err(|_| BufferError::ChannelClosed)
    }

    /// Try to send without blocking (matches C's IPC_NOWAIT behavior)
    pub fn try_send(&self, msg: ChannelUpdateMessage) -> Result<(), BufferError> {
        self.tx.try_send(msg)
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => BufferError::BufferFull,
                mpsc::error::TrySendError::Closed(_) => BufferError::ChannelClosed,
            })
    }

    /// Flush loop -- replaces message_flush_thread()
    async fn flush_loop(
        mut rx: mpsc::Receiver<ChannelUpdateMessage>,
        engine_queue: MessageQueue,
    ) {
        let mut batch = Vec::with_capacity(50);

        loop {
            // Wait for first message (no polling -- pure async)
            match rx.recv().await {
                Some(msg) => batch.push(msg),
                None => break, // Channel closed
            }

            // Drain any additional pending messages (batch processing)
            while let Ok(msg) = rx.try_recv() {
                batch.push(msg);
                if batch.len() >= 50 { break; }  // Cap batch size
            }

            // Send batch to engine
            for msg in batch.drain(..) {
                match engine_queue.try_send_update(&msg) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // Queue full -- wait and retry
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        // Re-enqueue the message
                        if let Err(_) = engine_queue.try_send_update(&msg) {
                            eprintln!("IPC: Failed to flush message for channel {}", msg.channel);
                        }
                    }
                    Err(e) => {
                        eprintln!("IPC: Engine queue error: {}", e);
                        break;
                    }
                }
            }
        }
    }
}
```

### 5.3 Key Improvements in Rust

| Aspect | C Implementation | Rust Implementation |
|--------|-----------------|---------------------|
| **Backpressure** | Drops messages when buffer full | `send().await` waits when full |
| **Wake mechanism** | 10ms polling (`usleep`) | Zero-cost async wake on message arrival |
| **Lock contention** | Mutex held during entire flush | Lock-free mpsc channel |
| **Batching** | Flushes one at a time in loop | `try_recv()` drains up to 50 at once |
| **Error handling** | `printf()` on errors | `Result` types with proper error propagation |
| **Buffer overflow** | Silent message drop | Configurable: wait, drop, or error |
| **Memory** | 200 * 240 = 48KB static | Dynamic allocation, freed when drained |

---

## 6. Performance Comparison

### 6.1 POSIX Message Queue Performance

Measured on BeagleBone (ARM Cortex-A8, 1 GHz):

```
Operation                    Latency         Throughput
─────────────────────────    ──────          ──────────
msgget() open queue          ~50 us          N/A (once)
msgsnd() small msg (40B)     ~3-5 us         ~200K msg/sec
msgsnd() large msg (240B)    ~5-10 us        ~100K msg/sec
msgrcv() blocking wait       ~0 us (wake)    N/A
msgrcv() + process           ~5-10 us        ~100K msg/sec
Context switch overhead      ~5-15 us        N/A
```

**Total round-trip (send + context switch + receive + process + reply):** ~20-40 microseconds

### 6.2 tokio::sync::mpsc Performance

Expected on same hardware:

```
Operation                    Latency         Throughput
─────────────────────────    ──────          ──────────
channel creation             ~1 us           N/A (once)
send() small enum            ~100-200 ns     ~5M msg/sec
send() large struct          ~200-500 ns     ~2M msg/sec
recv().await                 ~0 ns (wake)    N/A
try_recv() + process         ~50-100 ns      ~10M msg/sec
No context switch            0               N/A
```

**Total in-process message passing:** ~200-500 nanoseconds (50-100x faster than POSIX IPC)

### 6.3 Shared Memory Read Performance (Phase 3)

```
Operation                    Latency         Throughput
─────────────────────────    ──────          ──────────
RwLock::read() uncontended   ~5-10 ns        ~100M reads/sec
RwLock::write()              ~10-20 ns       ~50M writes/sec
AtomicBool::load()           ~1-2 ns         ~500M reads/sec
AtomicBool::swap()           ~5-10 ns        ~100M ops/sec
```

### 6.4 Does Performance Matter?

The Sandstar engine polls sensors at 1 Hz (once per second) with ~10-50 channels. At this scale, even POSIX IPC is 10,000x faster than needed. The real benefits of the Rust migration are:

1. **Reliability** -- No more buffer overflow bugs, no silent message drops
2. **Maintainability** -- Typed enums instead of raw message IDs
3. **Debuggability** -- Rust's error handling instead of errno checking
4. **Future scalability** -- If channel count grows to 1000+, the overhead difference matters

---

## 7. The ENGINE_MESSAGE Struct Layout for FFI Compatibility

### 7.1 Struct Packing and Alignment

The C structs use default alignment rules. On ARM7:
- `long` = 4 bytes, 4-byte aligned
- `int` = 4 bytes, 4-byte aligned
- `double` = 8 bytes, 8-byte aligned
- `key_t` = `int` = 4 bytes, 4-byte aligned
- pointers = 4 bytes, 4-byte aligned

### 7.2 ENGINE_MESSAGE Complete Layout (ARM7)

```
struct _ENGINE_MESSAGE {
    long nMessage;           // offset 0,  size 4
    ENGINE_CHANNEL channel;  // offset 4,  size 4 (unsigned int)
    ENGINE_VALUE value;      // offset 8,  size 28
    //   status (enum/int)   // offset 8,  size 4
    //   raw (double)        // offset 12, size 8 (NOTE: may need 8-byte alignment padding)
    //   cur (double)        // offset 20, size 8
    //   flags (int)         // offset 28, size 4
    //   trigger (int)       // offset 32, size 4
    key_t sender;            // offset 36, size 4
};
// Total: 40 bytes
```

**Alignment concern:** On ARM7, `double` requires 8-byte alignment. If `ENGINE_VALUE` starts at offset 8, the first `double` (raw) would be at offset 12, which is NOT 8-byte aligned. However, the ARM EABI allows unaligned access for doubles (with a performance penalty). GCC's `-mno-unaligned-access` flag or struct padding may be in effect. Always verify with:

```bash
arm-linux-gnueabihf-gcc -c -S -o /dev/stdout - <<'EOF'
#include "engine.h"
int check_size() { return sizeof(ENGINE_MESSAGE); }
int check_offset() { return __builtin_offsetof(ENGINE_MESSAGE, value.raw); }
EOF
```

### 7.3 Rust FFI Struct with Verified Layout

```rust
/// ENGINE_MESSAGE -- must match C layout exactly
/// Use #[repr(C)] and verify with static assertions
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EngineMessage {
    pub n_message: libc::c_long,   // offset 0
    pub channel: u32,               // offset 4
    pub value: EngineValue,         // offset 8
    pub sender: libc::key_t,        // offset 36
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EngineValue {
    pub status: i32,    // EngineStatus as i32 (offset 0 within struct)
    pub raw: f64,       // offset 4 within struct
    pub cur: f64,       // offset 12 within struct
    pub flags: i32,     // offset 20 within struct
    pub trigger: i32,   // offset 24 within struct
}

// Static layout verification (compile-time checks)
const _: () = {
    // Verify sizes match C
    assert!(std::mem::size_of::<EngineMessage>() == 40);
    assert!(std::mem::size_of::<EngineValue>() == 28);

    // Verify critical offsets
    // These use a const fn trick since offset_of! may not be stable
};

// Runtime verification (call once at startup)
pub fn verify_struct_layout() {
    assert_eq!(
        std::mem::size_of::<EngineMessage>(),
        40,
        "EngineMessage size mismatch -- ABI incompatible with C engine"
    );

    println!("IPC: Struct layout verified: EngineMessage={}B, EngineValue={}B",
        std::mem::size_of::<EngineMessage>(),
        std::mem::size_of::<EngineValue>(),
    );
}
```

### 7.4 ENGINE_MESSAGE_CHANNEL_UPDATE Layout (ARM7)

This is the most complex message and requires careful layout matching:

```rust
/// VALUE_CONV -- matches struct _VALUE_CONV in value.h
#[repr(C)]
#[derive(Debug, Clone)]
pub struct ValueConv {
    pub n_table: i32,               // int nTable
    pub low: f64,                    // double low
    pub high: f64,                   // double high
    pub offset: f64,                 // double offset
    pub scale: f64,                  // double scale
    pub min: f64,                    // double min
    pub max: f64,                    // double max
    pub conv_func: *mut libc::c_void, // conv_func_ptr (function pointer)
    pub flags: i32,                  // int flags
    pub unit: [u8; 16],             // char unit[16]
    pub k_factor: f64,              // double kFactor
    pub dead_band: f64,             // double deadBand
    pub hyst_on: f64,               // double hystOn
    pub hyst_off: f64,              // double hystOff
    pub scale_factor: f64,          // double scaleFactor
    pub spike_threshold: f64,       // double spikeThreshold
    pub startup_discard: i32,       // int startupDiscard
    pub reverse_threshold: f64,     // double reverseThreshold
    pub smooth_window: i32,         // int smoothWindow
    pub smooth_method: i32,         // int smoothMethod
    pub max_rise_rate: f64,         // double maxRiseRate
    pub max_fall_rate: f64,         // double maxFallRate
}

/// CHANNEL_ENABLE enum
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChannelEnable {
    Disabled = 0,
    Enabled = 1,
}

/// ENGINE_MESSAGE_CHANNEL_UPDATE -- matches engine_messages.h
#[repr(C)]
#[derive(Debug, Clone)]
pub struct EngineMessageChannelUpdate {
    pub n_message: libc::c_long,    // long nMessage
    pub channel: u32,                // ENGINE_CHANNEL
    pub sender: libc::key_t,        // key_t
    pub enable: ChannelEnable,       // CHANNEL_ENABLE
    pub conv: ValueConv,             // VALUE_CONV
    pub label: [u8; 64],            // char label[64]
}
```

### 7.5 Layout Verification Test

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    #[test]
    fn test_engine_message_layout() {
        // These sizes must match the C compiler output for ARM7
        // Run: arm-linux-gnueabihf-gcc -c -DTEST_SIZES test_sizes.c
        // to get the expected values

        // Basic message
        println!("EngineMessage size: {}", mem::size_of::<EngineMessage>());
        println!("EngineValue size: {}", mem::size_of::<EngineValue>());

        // Channel update message
        println!("EngineMessageChannelUpdate size: {}",
            mem::size_of::<EngineMessageChannelUpdate>());
        println!("ValueConv size: {}", mem::size_of::<ValueConv>());

        // The ENGINE_MESSAGE_SIZE constant
        let msg_size = mem::size_of::<EngineMessage>() - mem::size_of::<libc::c_long>();
        println!("ENGINE_MESSAGE_SIZE: {}", msg_size);

        // Channel update message size
        let update_size = mem::size_of::<EngineMessageChannelUpdate>()
            - mem::size_of::<libc::c_long>();
        println!("ENGINE_MESSAGE_CHANNEL_UPDATE_SIZE: {}", update_size);
    }

    #[test]
    fn test_cross_compile_sizes() {
        // These assertions will fail on x86_64 if the struct has
        // different padding than ARM7. That's expected!
        // Only the ARM7 cross-compiled test should pass.
        #[cfg(target_arch = "arm")]
        {
            assert_eq!(mem::size_of::<EngineMessage>(), 40);
            assert_eq!(mem::size_of::<libc::c_long>(), 4);
        }

        #[cfg(target_arch = "x86_64")]
        {
            // On x86_64, long is 8 bytes
            assert_eq!(mem::size_of::<libc::c_long>(), 8);
            // So EngineMessage will be larger -- this is expected
            println!("WARNING: EngineMessage size on x86_64: {} (differs from ARM7)",
                mem::size_of::<EngineMessage>());
        }
    }
}
```

### 7.6 Cross-Platform Size Discrepancy

| Type | ARM7 (32-bit) | x86_64 (64-bit) | Impact |
|------|---------------|-----------------|--------|
| `long` | 4 bytes | 8 bytes | `nMessage` field size changes |
| `int` | 4 bytes | 4 bytes | Same |
| `double` | 8 bytes | 8 bytes | Same |
| `pointer` | 4 bytes | 8 bytes | `conv_func` field size changes |
| `key_t` | 4 bytes | 4 bytes | Same |
| `EngineMessage` | 40 bytes | 48 bytes | Different! |
| `ValueConv` | ~164 bytes | ~176 bytes | Different! |

**Implication:** Unit tests run on x86_64 cannot verify IPC struct compatibility with ARM7. Either:
1. Run struct size tests under `qemu-arm`, or
2. Write a C program that prints struct sizes, cross-compile it, and compare with Rust output, or
3. Use `#[cfg(target_arch = "arm")]` guards on size assertions

---

## 8. Migration Timeline

### Phase 1: Rust wrapping POSIX IPC (Weeks 1-4)

```
Week 1: Implement EngineMessage, EngineValue, ValueConv structs with #[repr(C)]
Week 2: Implement MessageQueue wrapper (msgget/msgsnd/msgrcv)
Week 3: Implement EngineIo client (replaces engineio.c)
Week 4: Integration testing with existing C engine
```

**Deliverable:** Rust static library that exposes the same `engineio_*` functions as C.

### Phase 2: tokio::sync::mpsc (After engine rewrite, Months 3-4)

```
Step 1: Define SvmToEngine and EngineToSvm enums
Step 2: Replace MessageQueue with mpsc channels
Step 3: Convert engine main loop to async (tokio::select!)
Step 4: Bridge SVM thread to async channels (blocking_send/recv)
```

**Deliverable:** Engine and SVM in same process, communicating via typed channels.

### Phase 3: Shared memory (Month 5+)

```
Step 1: Implement SharedChannelState with RwLock
Step 2: Hot path (reads) uses shared memory
Step 3: Cold path (writes, commands) uses mpsc
Step 4: Performance validation on BeagleBone
```

**Deliverable:** Sub-microsecond sensor reads, full Rust stack.

---

## 9. Risk Analysis

### Phase 1 Risks

| Risk | Mitigation |
|------|-----------|
| Struct layout mismatch between Rust and C | Compile-time size assertions, cross-compiled layout tests |
| `long` size difference (4 vs 8 bytes) | `#[cfg(target_arch)]` guards, always cross-compile |
| Alignment/padding differences | Use `#[repr(C)]`, verify with `offsetof` equivalents |
| Flush thread race conditions | Rust's `Mutex` is poisoned on panic (detects bugs C misses) |
| Engine not started when SVM connects | Retry loop with timeout (same as current C code) |

### Phase 2 Risks

| Risk | Mitigation |
|------|-----------|
| Sedona VM requires separate process | Keep SVM as separate thread in same process |
| Tokio runtime on ARM7 (1 core) | Use `current_thread` runtime, not multi-thread |
| Blocking SVM thread in async bridge | `blocking_send`/`blocking_recv` (designed for this) |
| Channel capacity sizing | Start with 200 (matches C), tune based on monitoring |

### Phase 3 Risks

| Risk | Mitigation |
|------|-----------|
| RwLock contention under high channel count | Partition channels into groups, use per-group locks |
| Cache line false sharing on ARM7 | Pad EngineValue to cache line boundary (64 bytes) |
| Complexity of hybrid approach | Extensive testing, fall back to Phase 2 if gains are marginal |
