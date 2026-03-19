# 09 - Complete Dependency Mapping: C/C++ to Rust

## Overview

This document provides a complete mapping of every C and C++ dependency in the Sandstar codebase to its Rust equivalent. Each entry includes the current usage location, the Rust replacement, and migration notes.

---

## 1. C System Libraries

### 1.1 Threading: `pthread` -> `tokio` / `std::thread`

**Current usage:**
- `engine.c` - Main engine thread, poll thread
- `i2c_worker.c` - Dedicated I2C polling thread
- `uart_async.c` - UART async read thread
- `async_io.h` - Pthread mutex and condition variables

```c
// Current: engine.c
#include <pthread.h>
pthread_t poll_thread;
pthread_create(&poll_thread, NULL, poll_thread_func, NULL);
pthread_mutex_lock(&engine_mutex);
pthread_mutex_unlock(&engine_mutex);
```

```rust
// Rust replacement: tokio for async, std::thread for OS threads
use tokio::task;
use std::sync::Mutex;

// For CPU-bound work (I2C polling, sensor reading)
let handle = tokio::task::spawn_blocking(move || {
    poll_sensors()
});

// For async I/O
let handle = tokio::spawn(async move {
    uart_read_loop().await
});

// Mutex (when truly needed)
use std::sync::Mutex;
let engine_state = Mutex::new(EngineState::default());

// Or for async contexts:
use tokio::sync::Mutex;
let engine_state = tokio::sync::Mutex::new(EngineState::default());
```

### 1.2 IPC: `sys/msg.h` (POSIX Message Queues) -> `tokio::sync::mpsc`

**Current usage:**
- `engine.c` - Message queue for Haystack/Sedona communication
- `global.h` - `sys/types.h`, `sys/ipc.h`, `sys/msg.h` for `msgget`, `msgsnd`, `msgrcv`
- All CLI tools (`read.c`, `write.c`, `watch.c`, etc.) - Send commands to engine

```c
// Current: engine.c
#include <sys/types.h>
#include <sys/ipc.h>
#include <sys/msg.h>

int msgid = msgget(key, IPC_CREAT | 0666);
msgsnd(msgid, &msg, sizeof(msg), 0);
msgrcv(msgid, &msg, sizeof(msg), type, 0);
```

```rust
// Rust replacement: tokio channels for internal IPC
use tokio::sync::mpsc;

#[derive(Debug)]
enum EngineCommand {
    ReadChannel { channel: u16, reply: oneshot::Sender<f64> },
    WriteChannel { channel: u16, value: f64 },
    Shutdown,
}

let (tx, mut rx) = mpsc::channel::<EngineCommand>(200);

// Engine main loop
tokio::spawn(async move {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            EngineCommand::ReadChannel { channel, reply } => {
                let value = read_sensor(channel);
                let _ = reply.send(value);
            }
            EngineCommand::WriteChannel { channel, value } => {
                write_output(channel, value);
            }
            EngineCommand::Shutdown => break,
        }
    }
});
```

### 1.3 File Locking: `sys/file.h` (flock) -> `fs2` or `nix` crate

**Current usage:**
- `engine.c` - `flock()` to prevent multiple engine instances

```c
// Current: engine.c
#include <sys/file.h>
int fd = open("/var/run/sandstar.lock", O_CREAT | O_RDWR, 0666);
if (flock(fd, LOCK_EX | LOCK_NB) < 0) {
    // Another instance running
    exit(1);
}
```

```rust
// Rust replacement: fs2 crate
use fs2::FileExt;
use std::fs::File;

let lock_file = File::create("/var/run/sandstar.lock")?;
if lock_file.try_lock_exclusive().is_err() {
    eprintln!("Another sandstar instance is already running");
    std::process::exit(1);
}
// Lock released when lock_file is dropped
```

### 1.4 Signals: `signal.h` -> `tokio::signal`

**Current usage:**
- `engine.c` - SIGTERM/SIGINT handlers for graceful shutdown
- All CLI tools - SIGPIPE handling

```c
// Current: engine.c
#include <signal.h>
signal(SIGTERM, signal_handler);
signal(SIGINT, signal_handler);
```

```rust
// Rust replacement: tokio::signal
use tokio::signal;

tokio::spawn(async move {
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("Failed to register SIGTERM handler");
    let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())
        .expect("Failed to register SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => println!("Received SIGTERM"),
        _ = sigint.recv() => println!("Received SIGINT"),
    }

    // Initiate graceful shutdown
    shutdown_tx.send(()).ok();
});
```

### 1.5 Logging: `syslog.h` -> `tracing` + `tracing-subscriber`

**Current usage:**
- `engine.c`, `csv.c`, `poll.c`, `value.c`, `table.c` - `openlog()`, `syslog()`, `closelog()`
- `engine_log.h` - Logging macros (`ENGINE_LOG_DEBUG`, `ENGINE_LOG_INFO`, etc.)
- `logger.cpp` / `logger_c_wrapper.cpp` - C++ logger with C-callable wrapper

```c
// Current: engine_log.h
#include <syslog.h>
#define ENGINE_LOG_DEBUG(fmt, ...) syslog(LOG_DEBUG, fmt, ##__VA_ARGS__)
#define ENGINE_LOG_INFO(fmt, ...) syslog(LOG_INFO, fmt, ##__VA_ARGS__)
#define ENGINE_LOG_ERR(fmt, ...) syslog(LOG_ERR, fmt, ##__VA_ARGS__)
```

```rust
// Rust replacement: tracing ecosystem
use tracing::{debug, info, warn, error, instrument};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// Initialize logging with syslog support
fn init_logging() {
    let journald_layer = tracing_journald::layer()
        .expect("Failed to connect to journald");

    tracing_subscriber::registry()
        .with(journald_layer)
        .with(tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_level(true))
        .init();
}

// Usage throughout code
#[instrument(skip(grid))]
fn process_channel(channel: u16, grid: &Grid) -> f64 {
    debug!(channel, "Reading sensor");
    let value = read_sensor(channel);
    info!(channel, value, "Sensor read complete");
    value
}
```

### 1.6 Math: `math.h` -> `std::f64` methods

**Current usage:**
- `value.c` - `sqrt()`, `fabs()`, `fmin()`, `fmax()` for sensor value conversion
- `channel.c` - `fabs()` for value comparison

```c
// Current: value.c
#include <math.h>
double result = sqrt(value);
double diff = fabs(a - b);
double clamped = fmin(fmax(value, min), max);
```

```rust
// Rust: native f64 methods (no crate needed)
let result = value.sqrt();
let diff = (a - b).abs();
let clamped = value.clamp(min, max);  // Better: single call

// Additional useful methods
let is_valid = !value.is_nan() && value.is_finite();
let rounded = value.round();
```

### 1.7 Time: `time.h` / `sys/time.h` -> `std::time` / `chrono`

**Current usage:**
- `engine.c` - `gettimeofday()` for timestamps
- `channel.c` - `gettimeofday()` for poll timing

```c
// Current: channel.c
#include <sys/time.h>
struct timeval tv;
gettimeofday(&tv, NULL);
long ms = (tv.tv_sec * 1000) + (tv.tv_usec / 1000);
```

```rust
// Rust: std::time for monotonic timing
use std::time::Instant;

let start = Instant::now();
// ... do work ...
let elapsed_ms = start.elapsed().as_millis();

// For wall-clock timestamps (e.g., in Haystack data)
use chrono::{Utc, Local};
let now = Utc::now();
let timestamp = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
```

### 1.8 I/O Control: `sys/ioctl.h` -> `nix` crate

**Current usage:**
- `i2cio.c` - `ioctl()` for I2C device configuration
- `i2c_worker.c` - `ioctl()` for I2C slave address setting
- `uartio.c` - `ioctl()` for UART configuration

```c
// Current: i2cio.c
#include <sys/ioctl.h>
#include <linux/i2c-dev.h>
ioctl(fd, I2C_SLAVE, address);
```

```rust
// Rust: nix crate for ioctl
use nix::ioctl_write_int_bad;

const I2C_SLAVE: u16 = 0x0703;
ioctl_write_int_bad!(i2c_set_slave, I2C_SLAVE as nix::libc::c_ulong);

unsafe {
    i2c_set_slave(fd, address as nix::libc::c_int)?;
}

// Or use the linux-embedded-hal crate for type-safe I2C:
use linux_embedded_hal::I2cdev;
use embedded_hal::i2c::I2c;

let mut i2c = I2cdev::new("/dev/i2c-2")?;
i2c.write(address, &data)?;
i2c.read(address, &mut buffer)?;
```

### 1.9 Async I/O: `sys/epoll.h` / `sys/eventfd.h` / `sys/select.h` -> `tokio`

**Current usage:**
- `uart_async.c` - `epoll_create`, `epoll_ctl`, `epoll_wait` for async UART
- `i2c_worker.c` - `eventfd` for thread signaling, `select` for timeout

```c
// Current: uart_async.c
#include <sys/epoll.h>
int epoll_fd = epoll_create1(0);
epoll_ctl(epoll_fd, EPOLL_CTL_ADD, uart_fd, &ev);
epoll_wait(epoll_fd, events, MAX_EVENTS, timeout_ms);
```

```rust
// Rust: tokio handles epoll/io_uring internally
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::fs::File;
use tokio::time::timeout;
use std::time::Duration;

async fn uart_read_loop(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut uart = File::open(path).await?;
    let mut buf = [0u8; 256];

    loop {
        match timeout(Duration::from_millis(1000), uart.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {
                process_uart_data(&buf[..n]);
            }
            Ok(Ok(_)) => break, // EOF
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => continue, // Timeout, retry
        }
    }
    Ok(())
}
```

---

## 2. C++ Libraries: POCO Framework

POCO is the largest dependency (~500K lines vendored). Only a small subset is used.

### 2.1 HTTP Server: `Poco::Net::HTTPServer` -> `axum`

**Current usage:**
- `server.cpp` / `server.hpp` - HTTP server for Haystack REST API
- `op.cpp` / `op.hpp` - Request handler dispatch

```cpp
// Current: Uses POCO HTTPServer, HTTPRequestHandler, HTTPServerRequest/Response
Poco::Net::HTTPServer server(
    new RequestHandlerFactory(points),
    Poco::Net::ServerSocket(8085),
    new Poco::Net::HTTPServerParams
);
server.start();
```

```rust
// Rust: axum
use axum::{Router, routing::get};

let app = Router::new()
    .route("/about", get(about_handler))
    .route("/ops", get(ops_handler))
    .route("/read", get(read_handler))
    .route("/nav", get(nav_handler))
    .route("/hisRead", get(his_read_handler))
    .route("/pointWrite", axum::routing::post(point_write_handler))
    .route("/watchSub", axum::routing::post(watch_sub_handler))
    .route("/watchPoll", axum::routing::post(watch_poll_handler))
    .route("/watchUnsub", axum::routing::post(watch_unsub_handler))
    .route("/invokeAction", axum::routing::post(invoke_action_handler))
    .with_state(app_state);

let listener = tokio::net::TcpListener::bind("0.0.0.0:8085").await?;
axum::serve(listener, app).await?;
```

### 2.2 HTTP Client: `Poco::Net::HTTPClientSession` -> `reqwest`

**Current usage:**
- `client.cpp` - HTTP client for SkySpark sync, inter-device communication

```cpp
// Current
Poco::URI uri("http://skyspark.local/api/proj/read");
Poco::Net::HTTPClientSession session(uri.getHost(), uri.getPort());
Poco::Net::HTTPRequest request(Poco::Net::HTTPRequest::HTTP_GET, uri.getPath());
session.sendRequest(request);
Poco::Net::HTTPResponse response;
std::istream& rs = session.receiveResponse(response);
```

```rust
// Rust: reqwest
let client = reqwest::Client::new();
let response = client
    .get("http://skyspark.local/api/proj/read")
    .header("Accept", "text/zinc")
    .send()
    .await?;
let body = response.text().await?;
```

### 2.3 HTTP Request/Response: `Poco::Net::HTTPServerRequest/Response` -> `axum` extractors

**Current usage:**
- `op.cpp` - Extracting form parameters, query strings, writing responses

```cpp
// Current: op.cpp
#include "Poco/Net/HTMLForm.h"
Poco::Net::HTMLForm form(request, request.stream());
std::string filter = form.get("filter", "");
response.setContentType("text/zinc; charset=utf-8");
response.send() << zinc_body;
```

```rust
// Rust: axum extractors and responses
use axum::extract::Query;
use axum::response::IntoResponse;

#[derive(serde::Deserialize)]
struct ReadParams {
    filter: Option<String>,
}

async fn read_handler(
    Query(params): Query<ReadParams>,
) -> impl IntoResponse {
    let filter = params.filter.unwrap_or_default();
    // ... process
    (
        [(axum::http::header::CONTENT_TYPE, "text/zinc; charset=utf-8")],
        zinc_body,
    )
}
```

### 2.4 Threading: `Poco::Thread` / `Poco::ThreadPool` -> `tokio` runtime

**Current usage:**
- POCO HTTP server uses internal thread pool for request handling

```rust
// Rust: tokio runtime handles all async task scheduling
#[tokio::main]
async fn main() {
    // tokio::main creates a multi-threaded runtime by default
    // For BeagleBone (single core), use:
    // #[tokio::main(flavor = "current_thread")]
}
```

### 2.5 URI Parsing: `Poco::URI` -> `url` crate

**Current usage:**
- `client.cpp` - Parse URIs for HTTP client requests
- `auth/clientcontext.cpp` - Parse authentication URIs

```cpp
Poco::URI uri("http://host:8085/api/read?filter=point");
std::string host = uri.getHost();
int port = uri.getPort();
std::string path = uri.getPath();
```

```rust
use url::Url;
let url = Url::parse("http://host:8085/api/read?filter=point")?;
let host = url.host_str().unwrap();
let port = url.port().unwrap_or(80);
let path = url.path();
```

### 2.6 Concurrency Primitives: `Poco::AtomicCounter` / `Poco::RWLock` / `Poco::Timer`

**Current usage:**
- `server.hpp` - `Poco::AtomicCounter` for reference counting
- `server.hpp`, `points.hpp` - `Poco::RWLock` for concurrent point access
- `server.hpp`, `points.hpp` - `Poco::Timer` for periodic operations

```rust
// AtomicCounter -> std::sync::atomic
use std::sync::atomic::{AtomicUsize, Ordering};
let counter = AtomicUsize::new(0);
counter.fetch_add(1, Ordering::SeqCst);

// RWLock -> std::sync::RwLock or tokio::sync::RwLock
use tokio::sync::RwLock;
let points = RwLock::new(PointDatabase::new());
let reader = points.read().await;  // Multiple concurrent readers
let writer = points.write().await; // Exclusive writer

// Timer -> tokio::time::interval
use tokio::time::{interval, Duration};
let mut timer = interval(Duration::from_secs(10));
loop {
    timer.tick().await;
    poll_sensors().await;
}
```

### 2.7 File Watching: `Poco::DirectoryWatcher` -> `notify` crate

**Current usage:**
- `points.hpp` - Watch for configuration file changes to auto-reload

```rust
use notify::{Watcher, RecursiveMode, watcher};
use std::sync::mpsc::channel;
use std::time::Duration;

let (tx, rx) = channel();
let mut watcher = notify::recommended_watcher(move |res| {
    tx.send(res).ok();
})?;
watcher.watch("/home/eacio/sandstar/etc/config", RecursiveMode::Recursive)?;

// Async version with tokio
tokio::spawn(async move {
    while let Ok(event) = rx.recv() {
        match event {
            Ok(event) => reload_config(&event.paths).await,
            Err(e) => error!("Watch error: {}", e),
        }
    }
});
```

### 2.8 String Manipulation: `Poco::StringTokenizer` -> `str::split()` / `regex`

**Current usage:**
- Various locations for parsing comma-separated values, header parsing

```cpp
Poco::StringTokenizer tok(header, ",");
for (auto& token : tok) { /* ... */ }
```

```rust
// Rust: built-in str methods
let tokens: Vec<&str> = header.split(',').map(str::trim).collect();

// Or for more complex patterns:
let tokens: Vec<&str> = header.split(&[',', ';'][..]).collect();
```

### 2.9 Dynamic Values: `Poco::Dynamic::Var` -> `serde_json::Value`

```rust
use serde_json::Value;
let val = Value::Number(serde_json::Number::from_f64(72.5).unwrap());
let val = Value::String("hello".to_string());
let val = Value::Bool(true);
```

### 2.10 Cryptography: `Poco::Crypto` -> `ring` / `hmac` / `sha2`

**Current usage:**
- `auth/scramscheme.cpp` - SCRAM authentication (HMAC-SHA256, PBKDF2)

```cpp
// Current: SCRAM authentication
#include "Poco/Crypto/DigestEngine.h"
#include "Poco/HMACEngine.h"
#include "Poco/PBKDF2Engine.h"
#include "Poco/Base64Encoder.h"
#include "Poco/Base64Decoder.h"
```

```rust
// Rust: dedicated crypto crates
use hmac::{Hmac, Mac};
use sha2::Sha256;
use pbkdf2::pbkdf2_hmac;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

type HmacSha256 = Hmac<Sha256>;

fn compute_hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key)
        .expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn derive_key(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password, salt, iterations, &mut key);
    key
}

let encoded = BASE64.encode(&data);
let decoded = BASE64.decode(&encoded)?;
```

### 2.11 XML Parsing: `Poco::DOM` / `Poco::SAX` -> `quick-xml`

**Current usage:**
- `op.cpp` - Parsing XML responses from SkySpark

```rust
use quick_xml::Reader;
use quick_xml::events::Event;

let mut reader = Reader::from_str(xml_str);
loop {
    match reader.read_event()? {
        Event::Start(e) if e.name().as_ref() == b"row" => {
            // process row element
        }
        Event::Eof => break,
        _ => {}
    }
}
```

### 2.12 Other POCO Components

| POCO Component | Rust Replacement | Notes |
|---|---|---|
| `Poco::Net::NameValueCollection` | `HashMap<String, String>` | Standard library |
| `Poco::Net::HTTPAuthenticationParams` | Custom struct or `http::HeaderMap` | Part of axum/http |
| `Poco::NumberParser` | `str::parse::<T>()` | Standard library |
| `Poco::Path` | `std::path::Path` / `PathBuf` | Standard library |
| `Poco::Exception` | `thiserror` / `anyhow` | Rust error handling |
| `Poco::StreamCopier` | `tokio::io::copy()` | Standard async I/O |
| `Poco::RandomStream` | `rand` crate | See below |
| `Poco::DateTimeFormatter` | `chrono::format` | See below |
| `Poco::Task` / `Poco::TaskManager` | `tokio::task` | Async task management |
| `Poco::Util::ServerApplication` | `clap` + `tokio::main` | CLI + async runtime |
| `Poco::Util::Option/OptionSet` | `clap` crate | Command-line parsing |
| `Poco::Util::HelpFormatter` | `clap` (automatic) | Built into clap |

---

## 3. C++ Libraries: Boost

### 3.1 String Algorithms: `boost::algorithm` -> `itertools` or `std`

**Current usage:**
- `server.cpp`, `client.cpp`, `haystack.hpp` - `boost::algorithm::string` (split, trim, predicate)
- `coord.cpp`, `datetime.cpp` - `boost::algorithm::string::predicate` (starts_with, ends_with)
- `datetimerange.cpp` - `boost::algorithm::string::split`

```cpp
#include <boost/algorithm/string.hpp>
std::vector<std::string> parts;
boost::split(parts, input, boost::is_any_of(","));
bool result = boost::starts_with(s, "ver:");
bool result = boost::ends_with(s, "Z");
boost::trim(s);
```

```rust
// Rust: built-in str methods (no crate needed)
let parts: Vec<&str> = input.split(',').collect();
let result = s.starts_with("ver:");
let result = s.ends_with("Z");
let trimmed = s.trim();

// For more complex iteration patterns:
use itertools::Itertools;
let joined = parts.iter().join(", ");
```

### 3.2 UUID: `boost::uuid` -> `uuid` crate

**Current usage:**
- `points.cpp` - Generate UUIDs for watch subscriptions and point IDs

```cpp
#include <boost/uuid/uuid.hpp>
#include <boost/uuid/uuid_generators.hpp>
#include <boost/uuid/uuid_io.hpp>
boost::uuids::random_generator gen;
boost::uuids::uuid id = gen();
std::string id_str = boost::lexical_cast<std::string>(id);
```

```rust
use uuid::Uuid;
let id = Uuid::new_v4();
let id_str = id.to_string();
```

### 3.3 Lexical Cast: `boost::lexical_cast` -> `str::parse()` / `FromStr`

**Current usage:**
- `zincreader.cpp` - Parse numbers from strings
- `points.cpp`, `date.cpp`, `time.cpp`, `datetime.cpp` - Type conversions

```cpp
#include <boost/lexical_cast.hpp>
double val = boost::lexical_cast<double>(s.str());
int year = boost::lexical_cast<int>(s.str());
std::string s = boost::lexical_cast<std::string>(id);
```

```rust
// Rust: built-in (no crate needed)
let val: f64 = s.parse()?;
let year: i32 = s.parse()?;
let s = id.to_string();

// With default on error:
let val: f64 = s.parse().unwrap_or(0.0);
```

### 3.4 Random: `boost::random` -> `rand` crate

**Current usage:**
- `points.cpp` - Mersenne twister for generating random values (test/demo data)

```cpp
#include <boost/random/mersenne_twister.hpp>
#include <boost/random/uniform_real.hpp>
#include <boost/random/variate_generator.hpp>
boost::mt19937 rng;
boost::uniform_real<> range(0.0, 100.0);
boost::variate_generator<boost::mt19937&, boost::uniform_real<>> gen(rng, range);
double value = gen();
```

```rust
use rand::Rng;
let mut rng = rand::thread_rng();
let value: f64 = rng.gen_range(0.0..100.0);
```

### 3.5 Smart Pointers: `boost::scoped_ptr` -> `Box<T>`

**Current usage:**
- `zincreader.cpp` - `boost::scoped_ptr` and `boost::scoped_array` for temporary values
- `points.cpp`, `server.cpp`, `grid.cpp` - `boost::scoped_ptr` for owned objects

```cpp
#include <boost/scoped_ptr.hpp>
boost::scoped_ptr<TimeZone> tz(new TimeZone(name));
boost::scoped_array<Val*> cells(new Val*[numCols]);
```

```rust
// Rust: Box<T> (or just owned values on the stack)
let tz = Box::new(TimeZone::new(name));

// For arrays, use Vec<T>
let cells: Vec<Option<Value>> = vec![None; num_cols];
// Or for fixed-size:
let cells = Box::new([Value::Null; NUM_COLS]);
```

### 3.6 Shared Pointers: `boost::shared_ptr` -> `Arc<T>` / owned values

**Current usage:**
- `filter.hpp` - `boost::shared_ptr<Filter>` with `enable_shared_from_this`
- `watch.hpp` - `boost::shared_ptr<Watch>`

```cpp
#include <boost/shared_ptr.hpp>
#include <boost/enable_shared_from_this.hpp>
class Filter : public boost::enable_shared_from_this<Filter> {
    typedef boost::shared_ptr<Filter> shared_ptr_t;
};
Filter::shared_ptr_t filter = Filter::make("point");
```

```rust
// Rust: Arc for shared ownership, or Box for unique ownership
use std::sync::Arc;
let filter: Arc<Filter> = Arc::new(Filter::try_from("point")?);

// In most cases with libhaystack, owned values suffice:
let filter = Filter::try_from("point")?;
```

### 3.7 Container Pointers: `boost::ptr_vector` / `boost::ptr_map`

**Current usage:**
- `grid.hpp` - `boost::ptr_vector` for grid rows and columns
- `row.hpp` - `boost::ptr_vector` for row values
- `dict.hpp` - `boost::ptr_map` for dictionary key-value pairs
- `list.cpp`, `dict_type.cpp` - `boost::ptr_vector` for list items

```cpp
#include <boost/ptr_container/ptr_vector.hpp>
#include <boost/ptr_container/ptr_map.hpp>
boost::ptr_vector<Row> rows;
rows.push_back(new Row(cells, numCols));
boost::ptr_map<std::string, Val> entries;
entries.insert("tag", new Num(42));
```

```rust
// Rust: Vec<T> and HashMap<K, V> with owned values (no indirection needed)
let mut rows: Vec<Row> = Vec::new();
rows.push(Row::new(cells));

let mut entries: HashMap<String, Value> = HashMap::new();
entries.insert("tag".to_string(), Value::from(42));

// If dynamic dispatch is needed:
let mut items: Vec<Box<dyn Val>> = Vec::new();
```

### 3.8 Other Boost Components

| Boost Component | Rust Replacement | Notes |
|---|---|---|
| `boost::noncopyable` | (default in Rust) | Types are non-copyable by default |
| `boost::format` | `format!()` macro | Standard library |
| `boost::iterator_facade` | `Iterator` trait | Standard library |
| `boost::make_shared` | `Arc::new()` | Standard library |
| `boost::foreach` | `for x in collection` | Language built-in |

---

## 4. Custom C Parsers

### 4.1 Zinc Parser: `zinc.c` (684 lines) -> `libhaystack` zinc

See [05_ZINC_IO_ENCODING.md](05_ZINC_IO_ENCODING.md) for detailed mapping.

```c
// Current: zinc.c
ZINC zinc;
zinc_init(&zinc);
zinc_load(&zinc, "database.zinc");
int channel = zinc_integer(&zinc, row, "channel", -1);
zinc_exit(&zinc);
```

```rust
// Rust: libhaystack
let grid = libhaystack::zinc::decode::from_str(&fs::read_to_string("database.zinc")?)?;
let channel = grid.rows().nth(row)
    .and_then(|r| r.get("channel"))
    .and_then(|v| v.as_num())
    .map(|n| n.value as i32)
    .unwrap_or(-1);
```

### 4.2 CSV Parser: `csv.c` (569 lines) -> `csv` crate (BurntSushi)

**Current usage:**
- Engine startup - Load `tables.csv`, sensor configuration files

```c
// Current: csv.c
CSV csv;
csv_init(&csv);
csv_load(&csv, "tables.csv");
char *path = csv_string(&csv, row, "path");
int index = csv_integer(&csv, row, "nTable");
csv_exit(&csv);
```

```rust
// Rust: csv crate (BurntSushi)
use csv::ReaderBuilder;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct TableConfig {
    tag: String,
    unit: String,
    path: String,
}

let mut reader = ReaderBuilder::new()
    .has_headers(true)
    .from_path("tables.csv")?;

let configs: Vec<TableConfig> = reader
    .deserialize()
    .collect::<Result<_, _>>()?;

for config in &configs {
    println!("Tag: {}, Path: {}", config.tag, config.path);
}
```

The `csv` crate by BurntSushi is the standard Rust CSV parser. It provides:
- Serde integration for automatic deserialization into structs
- Streaming iteration (no need to load entire file into memory)
- Proper handling of quoted fields, escapes, and multi-line values
- Configurable delimiters, quote characters, and comment handling

---

## 5. Complete Cargo.toml

```toml
[package]
name = "sandstar"
version = "0.1.0"
edition = "2021"
description = "Sandstar IoT control system for BeagleBone"
license = "Proprietary"

[dependencies]
# ── Haystack Protocol ────────────────────────────────────────────
libhaystack = { version = "1", features = ["zinc", "json", "filter"] }

# ── HTTP Server (replaces POCO HTTPServer) ───────────────────────
axum = { version = "0.7", features = ["macros"] }
tower = "0.4"
tower-http = { version = "0.5", features = ["cors", "trace", "compression-gzip"] }
hyper = { version = "1", features = ["full"] }

# ── HTTP Client (replaces POCO HTTPClientSession) ────────────────
reqwest = { version = "0.12", features = ["json"], default-features = false, optional = true }

# ── Async Runtime (replaces pthread, POCO Thread/ThreadPool) ─────
tokio = { version = "1", features = [
    "full",        # rt-multi-thread, io-util, net, time, signal, sync, macros, fs
] }

# ── Serialization ────────────────────────────────────────────────
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# ── CSV Parsing (replaces csv.c, 569 lines) ─────────────────────
csv = "1"

# ── Logging (replaces syslog.h, engine_log.h, logger.cpp) ───────
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
tracing-journald = "0.3"

# ── Time & Dates (replaces time.h, gettimeofday) ────────────────
chrono = { version = "0.4", features = ["serde"] }

# ── UUID (replaces boost::uuid) ─────────────────────────────────
uuid = { version = "1", features = ["v4", "serde"] }

# ── URL Parsing (replaces Poco::URI) ────────────────────────────
url = "2"

# ── File Watching (replaces Poco::DirectoryWatcher) ──────────────
notify = "6"

# ── File Locking (replaces flock/sys/file.h) ────────────────────
fs2 = "0.4"

# ── Linux I/O (replaces sys/ioctl.h, Linux-specific syscalls) ───
nix = { version = "0.28", features = ["ioctl", "signal", "fs"] }

# ── Embedded HAL (replaces raw I2C/SPI/GPIO ioctl) ──────────────
embedded-hal = "1.0"
linux-embedded-hal = "0.4"

# ── Cryptography (replaces Poco::Crypto, SCRAM auth) ────────────
hmac = "0.12"
sha2 = "0.10"
pbkdf2 = "0.12"
base64 = "0.22"

# ── XML Parsing (replaces Poco::DOM/SAX, used in op.cpp) ────────
quick-xml = "0.31"

# ── Error Handling ───────────────────────────────────────────────
thiserror = "1"
anyhow = "1"

# ── Random (replaces boost::random) ─────────────────────────────
rand = "0.8"

# ── CLI tools (replaces Poco::Util::Option) ─────────────────────
clap = { version = "4", features = ["derive"] }

# ── Iteration utilities (replaces boost::algorithm) ─────────────
itertools = "0.12"

[features]
default = ["http-client"]
http-client = ["dep:reqwest"]

[build-dependencies]
# For linking Sedona VM (C code) into Rust binary
cc = "1"

[profile.release]
opt-level = "z"        # Optimize for binary size (BeagleBone: 512MB storage)
lto = true             # Link-Time Optimization
strip = true           # Strip debug symbols
codegen-units = 1      # Single codegen unit for max optimization
panic = "abort"        # Smaller binary, no unwinding

[profile.dev]
opt-level = 0
debug = true           # Full debug symbols for GDB
```

---

## 6. Dependency Comparison Summary

### Lines of Code Eliminated

| Current Dependency | Lines (Custom Code) | Rust Replacement | Lines (New) |
|---|---|---|---|
| POCO (vendored) | ~500,000 | axum + reqwest + tower | 0 (crate) |
| Boost headers (vendored) | ~50,000 | std + uuid + rand | 0 (crate) |
| zinc.c | 684 | libhaystack | 0 (crate) |
| csv.c | 569 | csv crate | 0 (crate) |
| zincreader.cpp/.hpp | 998 | libhaystack | 0 (crate) |
| zincwriter.cpp/.hpp | 187 | libhaystack | 0 (crate) |
| tokenizer.cpp/.hpp | 498 | libhaystack | 0 (crate) |
| filter.cpp/.hpp | 700 | libhaystack | 0 (crate) |
| logger.cpp + wrapper | ~200 | tracing | 0 (crate) |
| **Total custom code eliminated** | **~3,836** | | **~200 wrapper** |
| **Total vendored code eliminated** | **~550,000** | | **0** |

### Crate Count

The Rust binary uses **25 direct crate dependencies** (plus transitive deps managed by Cargo). This replaces:
- 1 massive vendored C++ framework (POCO, ~500K lines)
- 5+ Boost library headers
- 7 custom parser/serializer files
- 15+ POSIX system headers with manual error handling

### Safety Improvements

| Category | C/C++ | Rust |
|---|---|---|
| Null pointer derefs | Runtime crash | `Option<T>` at compile time |
| Buffer overflows | Manual bounds checks | Bounds-checked by default |
| Memory leaks | Manual free() | RAII (Drop trait) |
| Data races | pthread mutex (manual) | `Send`/`Sync` traits (compile-time) |
| Use-after-free | Runtime crash | Borrow checker prevents |
| Uninitialized memory | cppcheck catches some | Compiler error |
| String encoding | Manual UTF-8 function | Native UTF-8 strings |
| Error handling | Return codes / exceptions | `Result<T, E>` |
