# 04 -- REST API / Axum Migration

## Overview

This document details the migration of Sandstar's Haystack REST API from POCO C++ to Axum (Rust). The current implementation relies on POCO (~500,000 lines vendored) for HTTP server functionality, XML parsing, URI handling, directory watching, timers, and threading primitives. The Axum migration replaces this entire dependency stack with axum (~5K lines in binary) plus a handful of focused crates (tower-http, tokio, notify, serde), resulting in a dramatically smaller, safer, and faster HTTP layer.

### Source Files Under Migration

| File | Lines | Responsibility |
|------|-------|----------------|
| `op.cpp` | 1,675 | HTTP operation handlers (12 ops) |
| `op.hpp` | 151 | Op base class, StdOps registry |
| `points.cpp` | 3,555 | PointServer: database, timer, hot-reload, CRUD, priority array |
| `points.hpp` | 312 | PointServer/PointWatch/PointHistory class declarations |
| `server.cpp` | 261 | Abstract Server base class impl |
| `server.hpp` | 249 | Server interface: about, nav, watches, point writes, history |
| `filter.cpp` | 355 | Haystack filter parser (has/missing/eq/ne/lt/le/gt/ge/and/or) |
| `filter.hpp` | 344 | Filter AST types |
| **Total** | **~6,900** | Custom Haystack HTTP layer |
| POCO vendored | ~500,000 | HTTP server, XML, URI, timers, threading, directory watcher |

### POCO Classes Used (and Their Rust Replacements)

| POCO Class | Used For | Rust Replacement |
|------------|----------|------------------|
| `Poco::Net::HTTPServer` | Accept connections, dispatch | `axum::serve()` + `tokio::net::TcpListener` |
| `Poco::Net::HTTPServerRequest` | Request parsing | `axum::extract::Request` |
| `Poco::Net::HTTPServerResponse` | Response building | `axum::response::Response` |
| `Poco::Net::HTMLForm` | Query parameter parsing | `axum::extract::Query<T>` |
| `Poco::Net::HTTPClientSession` | Outbound HTTP to Sedona SOX | `reqwest::Client` |
| `Poco::XML::DOMParser` | Parse Sedona backup XML | `quick-xml` crate |
| `Poco::URI` | URI parsing | `http::Uri` (from http crate, re-exported by axum) |
| `Poco::RWLock` | Thread-safe record access | `tokio::sync::RwLock` |
| `Poco::Timer` | Periodic callbacks (1s tick) | `tokio::time::interval()` |
| `Poco::DirectoryWatcher` | Hot-reload database.zinc | `notify` crate + `tokio::sync::watch` |
| `Poco::AtomicCounter` | Watch lease counter | `std::sync::atomic::AtomicI32` |
| `Poco::FastMutex` | Metadata sync lock | `tokio::sync::Mutex` |
| `Poco::Util::Application` | Config file access | Custom config struct or `config` crate |

**Net dependency reduction: ~500,000 lines of vendored C++ replaced by ~15 focused Rust crates totaling ~20K lines.**

---

## 1. Current C++ Architecture

### 1.1 Request Dispatch Flow

```
HTTP Request (port 8085)
    |
    v
POCO HTTPServer (thread pool)
    |
    v
Op::on_service(Server& db, HTTPServerRequest& req, HTTPServerResponse& res)
    |
    +-- Set status 200, content-type "text/zinc; charset=utf-8"
    +-- Set CORS headers (Access-Control-Allow-Origin)
    +-- Parse request:
    |       GET  -> get_to_grid(req)   // HTMLForm -> Dict -> Grid
    |       POST -> post_to_grid(req)  // text/zinc body -> ZincReader -> Grid
    |
    +-- Route to virtual: Op::on_service(Server& db, const Grid& req)
    |       |
    |       +-- Each Op subclass implements this
    |
    +-- Write response: ZincWriter(ostr).write_grid(*result)
    +-- On error: write_grid(Grid::make_err(exception))
```

### 1.2 Operation Registry

Operations are registered as global singletons in `op.cpp`:

```cpp
const Op &StdOps::about      = AboutOp();
const Op &StdOps::ops         = *new OpsOp();
const Op &StdOps::formats     = FormatsOp();
const Op &StdOps::read        = ReadOp();
const Op &StdOps::nav         = NavOp();
const Op &StdOps::watch_sub   = WatchSubOp();
const Op &StdOps::watch_unsub = WatchUnsubOp();
const Op &StdOps::watch_poll  = WatchPollOp();
const Op &StdOps::watch_list  = WatchListOp();
const Op &StdOps::point_write = PointWriteOp();
const Op &StdOps::his_read    = HisReadOp();
const Op &StdOps::his_write   = HisWriteOp();
const Op &StdOps::invoke_action = InvokeActionOp();
const Op &StdOps::commit      = CommitOp();
const Op &StdOps::restart     = RestartOp();
const Op &StdOps::root        = RootOp();
const Op &StdOps::xeto        = XetoOp();
```

The `ops_map()` function lazily builds a `std::map<std::string, const Op* const>` used for dispatch.

### 1.3 CORS Handling (Current)

CORS is handled manually in `Op::on_service()`:

```cpp
// Check if wildcard is in allowed list
if (std::find(db.m_allowIPs.begin(), db.m_allowIPs.end(), "*") != db.m_allowIPs.end()) {
    res.set("Access-Control-Allow-Origin", "*");
} else {
    std::string origin = req.get("Origin");
    for (auto it = db.m_allowIPs.begin(); it != db.m_allowIPs.end(); it++) {
        if (it->find(origin) != std::string::npos) {
            res.set("Access-Control-Allow-Origin", *it);
            break;
        }
    }
}
res.set("Access-Control-Allow-Method", req.getMethod());
```

The allowed IPs are loaded from `/home/eacio/sandstar/etc/config/AllowedCorsUrl.config` at startup. If the file is missing, `"*"` is used.

### 1.4 PointServer State

`PointServer` holds all mutable state behind `Poco::RWLock`:

- `m_recs` -- `boost::ptr_map<std::string, Dict>` -- All point records
- `m_channelIndex` -- `std::unordered_map<ENGINE_CHANNEL, std::string>` -- O(1) channel-to-record lookup
- `m_allowIPs` -- `std::vector<std::string>` -- CORS whitelist
- `m_watches` -- `std::map<std::string, Watch::shared_ptr>` -- Active watch subscriptions
- `m_history` -- `std::map<std::string, PointHistory::shared_ptr>` -- In-memory history rings
- `m_dirty` -- `std::atomic<bool>` -- Dirty flag for debounced commit
- `m_reloadPending` -- `std::atomic<bool>` -- Hot-reload trigger from DirectoryWatcher
- `m_dirWatcher` -- `Poco::DirectoryWatcher*` -- Watches config directory for file changes

### 1.5 Timer Architecture

A `Poco::Timer` fires every 1 second (`TIMER_GRANULARITY = 1000ms`):

```cpp
void PointServer::on_timer(Poco::Timer &timer) {
    if (willTerminate()) return;
    flush_if_dirty();                    // Debounced zinc commit (500ms)
    if (m_reloadPending.exchange(false)) // Hot-reload database.zinc
        reload_zinc();
    timer_update();                      // Sync engine values -> records (every tick)
    if (count % m_timerWatches == 0)     // Watch lease expiry (every 60s)
        timer_watches();
    if (count % m_timerHistory == 0)     // History sampling (every 60s)
        timer_history();
    count++;
}
```

---

## 2. Axum Architecture

### 2.1 Cargo Dependencies

```toml
[dependencies]
# HTTP framework
axum = "0.7"
tokio = { version = "1", features = ["full"] }
tower-http = { version = "0.5", features = ["cors"] }

# Serialization / Haystack types
serde = { version = "1", features = ["derive"] }

# File watching for hot-reload
notify = "6"

# Outbound HTTP (for Sedona SOX spy/backup)
reqwest = { version = "0.12", features = ["json"] }

# XML parsing (for Sedona component list)
quick-xml = "0.31"

# Logging
tracing = "0.1"
tracing-subscriber = "0.3"
```

### 2.2 Complete Router Setup

```rust
use axum::{
    Router,
    routing::{get, post, any},
    extract::{State, Query, Form},
    response::{IntoResponse, Response},
    http::{StatusCode, header},
    middleware,
};
use tower_http::cors::{CorsLayer, Any};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Shared application state passed to every handler via Arc.
pub struct AppState {
    pub point_server: RwLock<PointServer>,
    pub config: AppConfig,
}

pub fn build_router(state: Arc<AppState>) -> Router {
    // CORS layer replaces manual header setting in Op::on_service()
    let cors = build_cors_layer(&state.config);

    Router::new()
        // --- Standard Haystack Ops ---
        .route("/about",      get(about_handler).post(about_handler))
        .route("/ops",        get(ops_handler).post(ops_handler))
        .route("/formats",    get(formats_handler).post(formats_handler))
        .route("/read",       get(read_handler).post(read_handler))
        .route("/nav",        get(nav_handler).post(nav_handler))
        .route("/watchSub",   get(watch_sub_handler).post(watch_sub_handler))
        .route("/watchUnsub", get(watch_unsub_handler).post(watch_unsub_handler))
        .route("/watchPoll",  get(watch_poll_handler).post(watch_poll_handler))
        .route("/watchList",  get(watch_list_handler).post(watch_list_handler))
        .route("/pointWrite", get(point_write_handler).post(point_write_handler))
        .route("/hisRead",    get(his_read_handler).post(his_read_handler))
        .route("/hisWrite",   get(his_write_handler).post(his_write_handler))
        // --- Extended Sandstar Ops ---
        .route("/commit",     get(commit_handler).post(commit_handler))
        .route("/restart",    get(restart_handler).post(restart_handler))
        .route("/root/*path", get(root_handler).post(root_handler))
        .route("/xeto",       get(xeto_handler).post(xeto_handler))
        .route("/invokeAction", get(invoke_action_handler).post(invoke_action_handler))
        // --- Middleware ---
        .layer(cors)
        // --- State ---
        .with_state(state)
}
```

### 2.3 Server Startup with Graceful Shutdown

```rust
use tokio::signal;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    tracing_subscriber::init();

    // Load configuration
    let config = AppConfig::load("/home/eacio/sandstar/etc/config");

    // Initialize point server (loads database.zinc)
    let point_server = PointServer::new(&config).expect("Failed to initialize PointServer");

    let state = Arc::new(AppState {
        point_server: RwLock::new(point_server),
        config,
    });

    // Start background tasks (timer_update, timer_watches, timer_history, hot-reload)
    let bg_state = Arc::clone(&state);
    tokio::spawn(background_tasks(bg_state));

    // Start file watcher for hot-reload
    let watch_state = Arc::clone(&state);
    tokio::spawn(file_watcher_task(watch_state));

    // Build router
    let app = build_router(Arc::clone(&state));

    // Bind to port 8085
    let listener = TcpListener::bind("0.0.0.0:8085").await.unwrap();
    tracing::info!("Haystack server listening on port 8085");

    // Serve with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("Shutdown signal received, starting graceful shutdown");
}
```

---

## 3. Operation-by-Operation Migration

### 3.1 Common Pattern: Op Base Class to Axum Handler

**C++ Pattern (every operation follows this):**

```cpp
class ReadOp : public Op {
public:
    const std::string name() const { return "read"; }
    const std::string summary() const { return "Read entity records in database"; }

    Grid::auto_ptr_t on_service(Server &db, const Grid &req) {
        // ... operation logic ...
        return db.read_all(filter, limit);
    }
};
```

The base class `Op::on_service(Server&, HTTPServerRequest&, HTTPServerResponse&)` handles:
1. Setting response headers (200 OK, text/zinc, CORS)
2. Parsing GET query params or POST zinc body into a `Grid`
3. Calling the virtual `on_service(Server&, const Grid&)`
4. Writing the result `Grid` as zinc to the response stream
5. Catching exceptions and returning `Grid::make_err()`

**Axum Pattern (each operation becomes a standalone async function):**

```rust
/// Common request parsing: GET query params or POST zinc body -> HaystackGrid
async fn parse_haystack_request(
    method: &axum::http::Method,
    query: &Option<HashMap<String, String>>,
    body: &str,
    content_type: &str,
) -> Result<Grid, HaystackError> {
    match method.as_str() {
        "GET" => {
            // Convert query params to single-row grid (mirrors get_to_grid())
            let mut dict = Dict::new();
            if let Some(params) = query {
                for (key, val) in params {
                    let parsed = zinc::parse_scalar(val).unwrap_or(Val::Str(val.clone()));
                    dict.insert(key.clone(), parsed);
                }
            }
            Ok(Grid::from_dict(dict))
        }
        "POST" => {
            // Parse zinc body (mirrors post_to_grid())
            if !content_type.contains("text/zinc") && !content_type.contains("text/plain") {
                return Err(HaystackError::NotAcceptable(content_type.to_string()));
            }
            zinc::read_grid(body).map_err(HaystackError::ZincParse)
        }
        _ => Err(HaystackError::MethodNotAllowed),
    }
}

/// Common response formatting: Grid -> text/zinc HTTP response
fn zinc_response(grid: &Grid) -> Response {
    let body = zinc::write_grid(grid);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/zinc; charset=utf-8")
        .body(body.into())
        .unwrap()
}

fn zinc_error_response(err: &HaystackError) -> Response {
    let grid = Grid::make_err(&err.to_string());
    zinc_response(&grid)
}
```

The CORS headers that were manually set in `Op::on_service()` are now handled by the `tower-http` CorsLayer middleware (see Section 5), so individual handlers never touch CORS.

---

### 3.2 AboutOp

**C++ (op.cpp lines 215-222):**

```cpp
class AboutOp : public Op {
    const std::string name() const { return "about"; }
    const std::string summary() const { return "Summary information for server"; }
    Grid::auto_ptr_t on_service(Server &db, const Grid &req) {
        return Grid::make(*db.about());
    }
};
```

Where `Server::about()` returns:

```cpp
Dict::auto_ptr_t Server::about() const {
    Dict::auto_ptr_t d(new Dict());
    d->add(on_about())
        .add("haystackVersion", "3.0")
        .add("serverTime", DateTime::now())
        .add("serverBootTime", boot_time())
        .add("tz", TimeZone::DEFAULT.name);
    return d;
}
```

And `PointServer::on_about()` adds:

```cpp
m_about->add("serverName", Poco::Net::DNS::hostName())
    .add("vendorName", "AnkaLabs")
    .add("vendorUri", Uri("https://ankalabs.com/"))
    .add("productName", "Project Sandstar")
    .add("productVersion", PROJECT_VERSION)
    .add("productUri", Uri("http://project-sandstar.org/"));
```

**Axum:**

```rust
async fn about_handler(
    State(state): State<Arc<AppState>>,
) -> Response {
    let server = state.point_server.read().await;

    let mut dict = Dict::new();
    dict.insert("haystackVersion", Val::Str("3.0".into()));
    dict.insert("serverTime",     Val::DateTime(Utc::now()));
    dict.insert("serverBootTime", Val::DateTime(server.boot_time));
    dict.insert("tz",             Val::Str(server.timezone.clone()));
    dict.insert("serverName",     Val::Str(hostname::get().unwrap_or_default()));
    dict.insert("vendorName",     Val::Str("AnkaLabs".into()));
    dict.insert("vendorUri",      Val::Uri("https://ankalabs.com/".into()));
    dict.insert("productName",    Val::Str("Project Sandstar".into()));
    dict.insert("productVersion", Val::Str(env!("CARGO_PKG_VERSION").into()));
    dict.insert("productUri",     Val::Uri("http://project-sandstar.org/".into()));

    zinc_response(&Grid::from_dict(dict))
}
```

**Key Differences:**
- No virtual dispatch; `about_handler` is a plain async function
- `CARGO_PKG_VERSION` replaces the C++ `PROJECT_VERSION` macro
- `hostname::get()` replaces `Poco::Net::DNS::hostName()`
- `Utc::now()` replaces `DateTime::now()` (via `chrono` crate)

---

### 3.3 OpsOp

**C++ (op.cpp lines 228-262):**

```cpp
class OpsOp : public Op {
    Grid::auto_ptr_t on_service(Server &db, const Grid &req) {
        // Lazily build and cache a grid of all registered operations
        if (ops_grid.get() != NULL)
            return Grid::auto_ptr_t(new GridView(gv));
        Grid::auto_ptr_t g(new Grid);
        g->add_col("name");
        g->add_col("summary");
        for (auto it = StdOps::ops_map().begin(); it != end; ++it) {
            Val *vals[2] = {new Str(it->second->name()), new Str(it->second->summary())};
            g->add_row(vals, 2);
        }
        ops_grid = g;
        return Grid::auto_ptr_t(new GridView(*ops_grid));
    }
};
```

**Axum:**

```rust
/// Static operation metadata -- built at compile time, no runtime caching needed.
const OPS: &[(&str, &str)] = &[
    ("about",       "Summary information for server"),
    ("ops",         "Operations supported by this server"),
    ("formats",     "Grid data formats supported by this server"),
    ("read",        "Read entity records in database"),
    ("nav",         "Navigate record tree"),
    ("watchSub",    "Watch subscription"),
    ("watchUnsub",  "Watch unsubscription"),
    ("watchPoll",   "Watch poll cov or refresh"),
    ("watchList",   "List all watches registered"),
    ("pointWrite",  "Read/write writable point priority array"),
    ("hisRead",     "Read time series from historian"),
    ("hisWrite",    "Write time series data to historian"),
    ("invokeAction","Invoke action on target entity"),
    ("commit",      "Commit changes to database"),
    ("restart",     "Restart Sandstar"),
    ("root",        "Root Sandstar component tree"),
    ("xeto",        "XETO specs from channel configuration"),
];

async fn ops_handler() -> Response {
    let mut grid = Grid::new();
    grid.add_col("name");
    grid.add_col("summary");
    for (name, summary) in OPS {
        grid.add_row(vec![Val::Str(name.to_string()), Val::Str(summary.to_string())]);
    }
    zinc_response(&grid)
}
```

**Key Difference:** The C++ version uses lazy initialization with a cached `GridView` and a global singleton. In Rust, the operation list is a compile-time constant. No mutex, no lazy init, no heap allocation.

---

### 3.4 FormatsOp

**C++ (op.cpp lines 268-299):**

```cpp
class FormatsOp : public Op {
    Grid::auto_ptr_t on_service(Server &db, const Grid &req) {
        static GridView gv(*fmt_grid);
        return Grid::auto_ptr_t(new GridView(gv));
    }
    static const Grid::auto_ptr_t fmt_grid;
};
// Static init: one row: ("text/zinc", Marker, Marker)
```

**Axum:**

```rust
async fn formats_handler() -> Response {
    let mut grid = Grid::new();
    grid.add_col("mime");
    grid.add_col("read");
    grid.add_col("write");
    grid.add_row(vec![
        Val::Str("text/zinc".into()),
        Val::Marker,
        Val::Marker,
    ]);
    zinc_response(&grid)
}
```

---

### 3.5 ReadOp

**C++ (op.cpp lines 305-338):**

```cpp
class ReadOp : public Op {
    Grid::auto_ptr_t on_service(Server &db, const Grid &req) {
        if (req.is_empty()) throw std::runtime_error("Request has no rows");
        const Row &row = req.row(0);
        if (row.has("filter")) {
            const std::string &filter = row.get_string("filter");
            size_t limit = row.has("limit") ? row.get_double("limit") : (size_t)-1;
            return db.read_all(filter, limit);
        } else if (row.has("id")) {
            boost::ptr_vector<Ref> v;
            for (auto it = req.begin(); it != req.end(); ++it) {
                if (it->has("id"))
                    v.push_back(new_clone((Ref &)it->get("id")));
            }
            return db.read_by_ids(v);
        }
        return Grid::auto_ptr_t();
    }
};
```

The `read_all` path goes through `Server::on_read_all()` which acquires a read lock and evaluates the filter against every record:

```cpp
Grid::auto_ptr_t Server::on_read_all(const std::string &filter, size_t limit) const {
    Poco::ScopedReadRWLock l(m_lock);
    Filter::shared_ptr_t f = Filter::make(filter);
    PathImpl pather(*this);
    std::vector<const Dict *> v;
    for (auto it = begin(); it != end(); ++it) {
        if (f->include(*it, pather)) {
            v.push_back(&*it);
            if (v.size() > limit) break;
        }
    }
    return Grid::make(v);
}
```

**Axum:**

```rust
#[derive(Deserialize)]
struct ReadParams {
    filter: Option<String>,
    id: Option<String>,
    limit: Option<usize>,
}

async fn read_handler(
    method: axum::http::Method,
    State(state): State<Arc<AppState>>,
    Query(params): Query<ReadParams>,
    body: String,
    headers: axum::http::HeaderMap,
) -> Response {
    // Parse request into grid (GET query params or POST zinc body)
    let req_grid = match parse_request(&method, &params, &body, &headers).await {
        Ok(g) => g,
        Err(e) => return zinc_error_response(&e),
    };

    let server = state.point_server.read().await;

    // Filter-based read
    if let Some(filter_str) = req_grid.get_str("filter") {
        let limit = req_grid.get_num("limit").unwrap_or(usize::MAX);
        match server.read_all(&filter_str, limit) {
            Ok(grid) => return zinc_response(&grid),
            Err(e) => return zinc_error_response(&e),
        }
    }

    // ID-based read
    if let Some(ids) = req_grid.get_refs("id") {
        match server.read_by_ids(&ids) {
            Ok(grid) => return zinc_response(&grid),
            Err(e) => return zinc_error_response(&e),
        }
    }

    zinc_error_response(&HaystackError::BadRequest("Request has no rows".into()))
}
```

**Key Differences:**
- `Query(params)` extracts GET params automatically via serde deserialization
- The `RwLock` is `tokio::sync::RwLock`, acquired with `.read().await` (async, non-blocking)
- Filter evaluation is identical in logic but the Rust filter parser returns `Result<Filter, Error>` instead of throwing exceptions
- No manual memory management (`boost::ptr_vector<Ref>` becomes `Vec<Ref>`)

---

### 3.6 NavOp

**C++ (op.cpp lines 343-445):**

The NavOp has two paths:
1. Standard navigation: `db.nav(navId)` which walks the site/equip/device/point hierarchy
2. Component list: Makes an HTTP GET to `http://127.0.0.1:8080/spy/backup`, parses the Sedona XML backup, and extracts components

The component list path uses `Poco::Net::HTTPClientSession` for the outbound request and `Poco::XML::DOMParser` for XML parsing.

**Axum:**

```rust
async fn nav_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let server = state.point_server.read().await;

    // Check for disType=component (Sedona component list request)
    if params.get("disType").map(|s| s.as_str()) == Some("component") {
        return match fetch_sedona_components().await {
            Ok(grid) => zinc_response(&grid),
            Err(e) => zinc_error_response(&e),
        };
    }

    // Standard nav path
    let nav_id = params.get("navId").cloned().unwrap_or_default();
    match server.nav(&nav_id) {
        Ok(grid) => zinc_response(&grid),
        Err(e) => zinc_error_response(&e),
    }
}

/// Fetch Sedona components via HTTP + XML parsing.
/// Replaces Poco::Net::HTTPClientSession + Poco::XML::DOMParser.
async fn fetch_sedona_components() -> Result<Grid, HaystackError> {
    let client = reqwest::Client::new();
    let resp = client
        .get("http://127.0.0.1:8080/spy/backup")
        .send()
        .await
        .map_err(|e| HaystackError::Internal(format!("SOX request failed: {}", e)))?;

    let xml_body = resp.text().await
        .map_err(|e| HaystackError::Internal(format!("SOX response read failed: {}", e)))?;

    // Parse XML with quick-xml
    let mut reader = quick_xml::Reader::from_str(&xml_body);
    let mut grid = Grid::new();
    grid.add_col("name");
    grid.add_col("id");
    grid.add_col("type");

    // Walk the DOM: sedonaApp > app > comp (skip "service" components)
    let mut in_app = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(ref e)) => {
                match e.name().as_ref() {
                    b"app" => in_app = true,
                    b"comp" if in_app => {
                        let attrs = parse_attributes(e);
                        if attrs.get("name").map(|n| n != "service").unwrap_or(false) {
                            grid.add_row(vec![
                                Val::Str(attrs.get("name").cloned().unwrap_or_default()),
                                Val::Ref(attrs.get("id").cloned().unwrap_or_default()),
                                Val::Str(attrs.get("type").cloned().unwrap_or_default()),
                            ]);
                        }
                    }
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(e) => return Err(HaystackError::Internal(format!("XML parse error: {}", e))),
            _ => {}
        }
        buf.clear();
    }

    Ok(grid)
}
```

**Key Differences:**
- `reqwest::Client` replaces `Poco::Net::HTTPClientSession` (async, connection-pooled)
- `quick-xml` replaces `Poco::XML::DOMParser` (streaming SAX-like parser, no DOM allocation)
- Error handling uses `Result` instead of `try/catch` around POCO exceptions

---

### 3.7 WatchSubOp / WatchUnsubOp / WatchPollOp

**C++ (op.cpp lines 451-536):**

```cpp
class WatchSubOp : public Op {
    Grid::auto_ptr_t on_service(Server &db, const Grid &req) {
        std::string watchId, watchDis;
        if (req.meta().has("watchId"))
            watchId = req.meta().get_str("watchId");
        else
            watchDis = req.meta().get_str("watchDis");
        Watch::shared_ptr watch = watchId.empty() ? db.watch_open(watchDis) : db.watch(watchId);
        if (watch.get() == NULL)
            return Grid::make_err(std::runtime_error("Watch not found."));
        const Op::refs_t &ids = grid_to_ids(db, req);
        return watch->sub(ids);
    }
};

class WatchUnsubOp : public Op {
    Grid::auto_ptr_t on_service(Server &db, const Grid &req) {
        const std::string &watchId = req.meta().get_str("watchId");
        Watch::shared_ptr watch = db.watch(watchId, false);
        if (watch.get() != NULL) {
            if (req.meta().has("close"))
                watch->close();
            else
                watch->unsub(grid_to_ids(db, req));
        }
        return Grid::auto_ptr_t();
    }
};

class WatchPollOp : public Op {
    Grid::auto_ptr_t on_service(Server &db, const Grid &req) {
        const std::string &watchId = req.meta().get_str("watchId");
        Watch::shared_ptr watch = db.watch(watchId);
        if (watch.get() != NULL) {
            if (req.meta().has("refresh"))
                return watch->poll_refresh();
            else
                return watch->poll_changes();
        }
        return Grid::make_err(std::runtime_error("Watch not found."));
    }
};
```

**Axum (combined for brevity):**

```rust
async fn watch_sub_handler(
    State(state): State<Arc<AppState>>,
    req: HaystackRequest,
) -> Response {
    let grid = req.into_grid();
    let mut server = state.point_server.write().await;

    // Open or lookup watch
    let watch_id = grid.meta().get_str("watchId");
    let watch_dis = grid.meta().get_str("watchDis");

    let watch = match (watch_id, watch_dis) {
        (Some(id), _) => server.watch(&id),
        (_, Some(dis)) => Ok(server.watch_open(&dis)),
        _ => Err(HaystackError::BadRequest("Missing watchId or watchDis".into())),
    };

    match watch {
        Ok(w) => {
            let ids = grid.get_ref_column("id");
            match w.sub(&ids) {
                Ok(result) => zinc_response(&result),
                Err(e) => zinc_error_response(&e),
            }
        }
        Err(e) => zinc_error_response(&e),
    }
}

async fn watch_unsub_handler(
    State(state): State<Arc<AppState>>,
    req: HaystackRequest,
) -> Response {
    let grid = req.into_grid();
    let mut server = state.point_server.write().await;

    let watch_id = grid.meta().get_str("watchId")
        .unwrap_or_default();

    if let Some(watch) = server.watch_opt(&watch_id) {
        if grid.meta().has("close") {
            watch.close();
        } else {
            let ids = grid.get_ref_column("id");
            watch.unsub(&ids);
        }
    }

    zinc_response(&Grid::empty())
}

async fn watch_poll_handler(
    State(state): State<Arc<AppState>>,
    req: HaystackRequest,
) -> Response {
    let grid = req.into_grid();
    let server = state.point_server.read().await;

    let watch_id = match grid.meta().get_str("watchId") {
        Some(id) => id,
        None => return zinc_error_response(
            &HaystackError::BadRequest("Missing watchId".into())
        ),
    };

    match server.watch(&watch_id) {
        Ok(watch) => {
            let result = if grid.meta().has("refresh") {
                watch.poll_refresh()
            } else {
                watch.poll_changes()
            };
            zinc_response(&result)
        }
        Err(_) => {
            let mut err_grid = Grid::make_err("Watch not found.");
            err_grid.meta_mut().insert("watchId", Val::Str(watch_id));
            zinc_response(&err_grid)
        }
    }
}
```

**Key Differences:**
- `boost::shared_ptr<Watch>` becomes `Arc<RwLock<Watch>>` or simply a reference behind the server's `RwLock`
- `NULL` checks become `Option<T>` pattern matching
- Watch lease management (currently in `timer_watches()`) moves to a background tokio task

---

### 3.8 PointWriteOp

**C++ (op.cpp lines 572-596):**

```cpp
class PointWriteOp : public Op {
    Grid::auto_ptr_t on_service(Server &db, const Grid &req) {
        if (req.is_empty()) throw std::runtime_error("Request has no rows");
        const Row &row = req.row(0);
        Val::auto_ptr_t id = val_to_id(db, row.get("id"));
        if (row.has("level")) {
            int level = static_cast<int>(row.get_int("level"));
            const std::string &who = row.get_str("who");
            const Val &val = row.get("val", false);
            const Num &dur = (Num &)row.get("duration", false);
            db.point_write(id->as<Ref>(), level, val, who, dur);
        }
        return db.point_write_array(id->as<Ref>());
    }
};
```

The `Server::point_write()` validates level 1-17, checks `writable` tag, then calls `PointServer::on_point_write()` which communicates with the C engine via `engineio_setwritelevel()` and `engineio_write_channel()`.

**Axum:**

```rust
#[derive(Deserialize)]
struct PointWriteParams {
    id: Option<String>,
    level: Option<i32>,
    val: Option<String>,
    who: Option<String>,
    duration: Option<f64>,
}

async fn point_write_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<PointWriteParams>,
    body: String,
    headers: axum::http::HeaderMap,
    method: axum::http::Method,
) -> Response {
    let req_grid = match parse_request_from_any(&method, &params, &body, &headers).await {
        Ok(g) => g,
        Err(e) => return zinc_error_response(&e),
    };

    let row = match req_grid.row(0) {
        Some(r) => r,
        None => return zinc_error_response(
            &HaystackError::BadRequest("Request has no rows".into())
        ),
    };

    let id = match row.get_ref("id") {
        Some(r) => r.clone(),
        None => return zinc_error_response(
            &HaystackError::BadRequest("Missing id".into())
        ),
    };

    let mut server = state.point_server.write().await;

    // Write if level is specified
    if let Some(level) = row.get_int("level") {
        let level = level as i32;
        if !(1..=17).contains(&level) {
            return zinc_error_response(
                &HaystackError::BadRequest(format!("Invalid level 1-17: {}", level))
            );
        }
        let who = row.get_str("who").unwrap_or_default();
        let val = row.get("val"); // Option<Val> -- None means "auto/null"
        let dur = row.get_num("duration").unwrap_or(0.0);

        if let Err(e) = server.point_write(&id, level, val, &who, dur) {
            return zinc_error_response(&e);
        }
    }

    // Always return the priority array
    match server.point_write_array(&id) {
        Ok(grid) => zinc_response(&grid),
        Err(e) => zinc_error_response(&e),
    }
}
```

**Key Differences:**
- The 17-level priority array is a `[Option<PriorityLevel>; 17]` in Rust instead of a C struct array
- `engineio_setwritelevel()` and `engineio_write_channel()` calls happen through the FFI bridge (see doc 06 and 07)
- Level validation uses `(1..=17).contains()` instead of manual `if (level < 1 || level > 17)`

---

### 3.9 HisReadOp

**C++ (op.cpp lines 601-617):**

```cpp
class HisReadOp : public Op {
    Grid::auto_ptr_t on_service(Server &db, const Grid &req) {
        if (req.is_empty()) throw std::runtime_error("Request has no rows");
        const Row &row = req.row(0);
        Val::auto_ptr_t id = val_to_id(db, row.get("id"));
        const std::string &r = row.get_str("range");
        return db.his_read((Ref &)*id, r);
    }
};
```

**Axum:**

```rust
async fn his_read_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let server = state.point_server.read().await;

    let id = match params.get("id") {
        Some(id_str) => Ref::from_str(id_str),
        None => return zinc_error_response(
            &HaystackError::BadRequest("Missing id".into())
        ),
    };

    let range = match params.get("range") {
        Some(r) => r.clone(),
        None => return zinc_error_response(
            &HaystackError::BadRequest("Missing range".into())
        ),
    };

    match server.his_read(&id, &range) {
        Ok(grid) => zinc_response(&grid),
        Err(e) => zinc_error_response(&e),
    }
}
```

---

### 3.10 CommitOp

**C++ (op.cpp lines 666-734):**

The CommitOp handles five sub-operations: `add`, `update`, `delete`, `optimize`, `zinc`.

```cpp
class CommitOp : public Op {
    Grid::auto_ptr_t on_service(Server &db, const Grid &req) {
        const Val &val = req.meta().get("commit");
        if (val == Str(UPDATE))      { updatePoint(req); }
        else if (val == Str(ADD))    { addPoint(req); }
        else if (val == Str(DELETE)) { deletePoint(req); }
        else if (val == Str(OPTIMIZE)) { optimizeGrid(); }
        else if (val == Str(ZINC))   { PointServer::mark_dirty(); }
        // return status grid
    }
};
```

Each sub-operation (updatePoint, addPoint, deletePoint) acquires a write lock and modifies `m_recs`, then calls `PointServer::mark_dirty()` which triggers debounced flush to disk (500ms).

**Axum:**

```rust
async fn commit_handler(
    State(state): State<Arc<AppState>>,
    req: HaystackRequest,
) -> Response {
    let grid = req.into_grid();

    let commit_op = match grid.meta().get_str("commit") {
        Some(op) => op,
        None => return zinc_error_response(
            &HaystackError::BadRequest("Missing commit meta tag".into())
        ),
    };

    let mut server = state.point_server.write().await;

    let status = match commit_op.as_str() {
        "update" => {
            server.update_points(&grid);
            server.mark_dirty();
            "ok"
        }
        "add" => {
            server.add_points(&grid);
            server.mark_dirty();
            "ok"
        }
        "delete" => {
            server.delete_points(&grid);
            server.mark_dirty();
            "ok"
        }
        "optimize" => {
            match server.optimize_grid() {
                Ok(()) => "ok",
                Err(_) => "fail",
            }
        }
        "zinc" => {
            server.mark_dirty(); // Async: queue for background write
            "ok"
        }
        _ => return zinc_error_response(
            &HaystackError::BadRequest(
                "Invalid commit operation. Allowed: add, update, delete, optimize, zinc".into()
            )
        ),
    };

    let mut result = Dict::new();
    result.insert("op", Val::Str("commit".into()));
    result.insert("subOp", Val::Str(commit_op));
    result.insert("status", Val::Str(status.into()));
    zinc_response(&Grid::from_dict(result))
}
```

---

### 3.11 XetoOp

**C++ (op.cpp lines 1060-1610):**

XetoOp is the largest operation (~550 lines). It:
1. Parses `points.csv` to map channels to sensor types
2. Discovers all tags grouped by sensor type from live `m_recs`
3. Optionally writes `.xeto` spec files to disk
4. Returns either a type list or detailed tag grid

**Axum:**

```rust
async fn xeto_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let server = state.point_server.read().await;

    let query_channel = params.get("channel").cloned();
    let query_type = params.get("type").cloned();
    let write_files = params.get("writeFiles").map(|v| v == "true").unwrap_or(false);

    // The xeto logic is extracted into a dedicated module:
    // sandstar_rust/src/haystack/xeto.rs
    match xeto::generate_specs(&server, query_channel, query_type, write_files) {
        Ok(grid) => zinc_response(&grid),
        Err(e) => zinc_error_response(&e),
    }
}
```

The 550-line XetoOp C++ class maps to a dedicated Rust module `xeto.rs` with the same logic but cleaner separation. The static mutable globals (`s_pointsMap`, `s_pointsParsed`) become either `OnceCell<HashMap<i32, PointsCsvEntry>>` or a field on `AppState`.

---

### 3.12 RestartOp

**C++ (op.cpp lines 740-769):**

```cpp
class RestartOp : public Op {
    Grid::auto_ptr_t on_service(Server &db, const Grid &req) {
        exit(0);  // systemd restarts the service
        // unreachable
    }
};
```

**Axum:**

```rust
async fn restart_handler() -> Response {
    tracing::info!("Restart requested via API");
    // Schedule shutdown after response is sent
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        std::process::exit(0); // systemd will restart
    });

    let mut result = Dict::new();
    result.insert("status", Val::Str("restarting".into()));
    zinc_response(&Grid::from_dict(result))
}
```

**Key Difference:** The Rust version sends a response before exiting, which is more graceful. The C++ version calls `exit(0)` immediately, never sending a response.

---

## 4. State Management

### 4.1 C++ State (Global Statics + Poco Locks)

```cpp
// points.hpp / points.cpp -- all static members on PointServer
static recs_t m_recs;                                    // All records
static std::unordered_map<ENGINE_CHANNEL, std::string> m_channelIndex;  // O(1) lookup
static std::vector<std::string> m_allowIPs;              // CORS whitelist
static std::atomic<bool> m_dirty;                        // Dirty flag
static std::atomic<bool> m_reloadPending;                // Hot-reload trigger
static std::string m_zincFilePath;                       // Path to database.zinc
static Poco::RWLock m_lock;                              // Global reader-writer lock
```

Problems:
- Static globals with manual lifetime management
- The `Poco::RWLock` is a single global lock for ALL state (records + watches + history)
- `std::auto_ptr` used throughout (deprecated in C++11, removed in C++17)

### 4.2 Rust State (Arc<AppState> + Fine-Grained Locks)

```rust
/// Application state shared across all handlers and background tasks.
pub struct AppState {
    /// Point database with fine-grained async locking
    pub records: RwLock<RecordStore>,
    /// Watch subscriptions
    pub watches: RwLock<WatchStore>,
    /// In-memory history ring buffers
    pub history: RwLock<HistoryStore>,
    /// Channel-to-record index for O(1) lookup
    pub channel_index: RwLock<HashMap<u32, String>>,
    /// Configuration (immutable after init)
    pub config: AppConfig,
    /// Boot time (immutable after init)
    pub boot_time: chrono::DateTime<chrono::Utc>,
    /// Dirty flag for debounced zinc commit
    pub dirty: AtomicBool,
    /// Last modification time
    pub last_modified: Mutex<Instant>,
    /// Shutdown signal sender
    pub shutdown_tx: watch::Sender<bool>,
}

pub struct RecordStore {
    /// All point records, keyed by record ID
    pub recs: HashMap<String, Dict>,
}

pub struct WatchStore {
    pub watches: HashMap<String, Watch>,
}

pub struct HistoryStore {
    pub history: HashMap<String, VecDeque<HisItem>>,
}

pub struct AppConfig {
    pub config_dir: PathBuf,
    pub zinc_file_path: PathBuf,
    pub allowed_ips: Vec<String>,
    pub timer_period_secs: u64,
    pub timer_watches_secs: u64,
    pub timer_history_secs: u64,
}
```

**Advantages over C++:**
- Fine-grained locks: reads do not block watches, history does not block reads
- `RwLock` is async (`tokio::sync::RwLock`), so waiting for a lock does not block the thread
- No static globals; all state flows through `Arc<AppState>`
- No manual memory management; `HashMap<String, Dict>` owns its values

---

## 5. CORS Middleware (tower-http)

### C++ (Manual, in Op::on_service):

```cpp
void Op::on_service(Server &db, HTTPServerRequest &req, HTTPServerResponse &res) {
    res.set("Access-Control-Allow-Origin", "http://localhost");
    if (std::find(db.m_allowIPs.begin(), db.m_allowIPs.end(), "*") != db.m_allowIPs.end()) {
        res.set("Access-Control-Allow-Origin", "*");
    } else {
        std::string origin = req.get("Origin");
        for (auto it = db.m_allowIPs.begin(); it != db.m_allowIPs.end(); it++) {
            if (it->find(origin) != std::string::npos) {
                res.set("Access-Control-Allow-Origin", *it);
                break;
            }
        }
    }
    res.set("Access-Control-Allow-Method", req.getMethod());
}
```

This is duplicated in EVERY request because it runs in the base `on_service()` method.

### Rust (tower-http CorsLayer, configured once):

```rust
use tower_http::cors::{CorsLayer, AllowOrigin, AllowMethods};
use axum::http::{HeaderValue, Method};

fn build_cors_layer(config: &AppConfig) -> CorsLayer {
    if config.allowed_ips.contains(&"*".to_string()) {
        // Wildcard: allow all origins
        CorsLayer::new()
            .allow_origin(AllowOrigin::any())
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            .allow_headers(tower_http::cors::Any)
    } else {
        // Specific origins from AllowedCorsUrl.config
        let origins: Vec<HeaderValue> = config.allowed_ips.iter()
            .filter_map(|ip| HeaderValue::from_str(ip).ok())
            .collect();

        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            .allow_headers(tower_http::cors::Any)
    }
}
```

**Key Advantage:** The CORS layer is applied once as middleware in the router. Individual handlers never touch CORS headers. The `tower-http` implementation also correctly handles preflight `OPTIONS` requests, which the C++ version does not.

---

## 6. Request Parsing (Extractors)

### 6.1 GET Request Parsing

**C++ (`Op::get_to_grid()`):**

```cpp
Grid::auto_ptr_t Op::get_to_grid(HTTPServerRequest &req) {
    HTMLForm form(req);
    Dict d;
    for (auto it = form.begin(); it != form.end(); ++it) {
        const std::string &name = it->first;
        const std::string &val_str = it->second;
        Val::auto_ptr_t val;
        try {
            val = ZincReader::make(val_str)->read_scalar();
        } catch (std::exception &) {
            val = Str(val_str).clone();
        }
        d.add(name, val);
    }
    d.add("path", Str(Poco::URI(req.getURI()).getPath()).clone());
    return Grid::make(d);
}
```

**Rust (Axum `Query` extractor):**

```rust
use axum::extract::Query;
use std::collections::HashMap;

/// Axum's Query extractor automatically deserializes URL query parameters.
/// For typed extraction:
#[derive(Deserialize)]
struct ReadParams {
    filter: Option<String>,
    id: Option<String>,
    limit: Option<usize>,
}

async fn read_handler(
    Query(params): Query<ReadParams>,
) -> Response {
    // params.filter is already Option<String>
    // params.limit is already Option<usize>
}

/// For dynamic extraction (when params vary by operation):
async fn generic_handler(
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // Convert to Haystack Dict, parsing zinc scalars
    let dict = params_to_dict(&params);
}

fn params_to_dict(params: &HashMap<String, String>) -> Dict {
    let mut dict = Dict::new();
    for (key, val_str) in params {
        let val = zinc::parse_scalar(val_str)
            .unwrap_or(Val::Str(val_str.clone()));
        dict.insert(key.clone(), val);
    }
    dict
}
```

### 6.2 POST Request Parsing

**C++ (`Op::post_to_grid()`):**

```cpp
Grid::auto_ptr_t Op::post_to_grid(HTTPServerRequest &req, HTTPServerResponse &res) {
    const std::string &mime = req.getContentType();
    if (mime.find("text/zinc") == mime.npos && mime.find("text/plain") == mime.npos) {
        res.setStatusAndReason(HTTPResponse::HTTP_NOT_ACCEPTABLE, mime);
        res.send();
        return Grid::auto_ptr_t();
    }
    return ZincReader(req.stream()).read_grid();
}
```

**Rust (custom extractor for zinc body):**

```rust
use axum::{
    async_trait,
    extract::FromRequest,
    http::{Request, StatusCode, header},
    body::Bytes,
};

/// Custom Axum extractor that reads POST body as a Haystack Grid.
pub struct ZincBody(pub Grid);

#[async_trait]
impl<S: Send + Sync> FromRequest<S> for ZincBody {
    type Rejection = (StatusCode, String);

    async fn from_request(req: Request<axum::body::Body>, state: &S) -> Result<Self, Self::Rejection> {
        let content_type = req.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !content_type.contains("text/zinc") && !content_type.contains("text/plain") {
            return Err((
                StatusCode::NOT_ACCEPTABLE,
                format!("Unsupported content type: {}", content_type),
            ));
        }

        let bytes = Bytes::from_request(req, state).await
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

        let body = String::from_utf8(bytes.to_vec())
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

        let grid = zinc::read_grid(&body)
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

        Ok(ZincBody(grid))
    }
}
```

### 6.3 Unified GET/POST Handler Pattern

Since every Haystack op accepts both GET and POST, we use a unified extractor:

```rust
/// Extracts a Haystack Grid from either GET query params or POST zinc body.
pub struct HaystackRequest {
    pub grid: Grid,
}

#[async_trait]
impl<S: Send + Sync> FromRequest<S> for HaystackRequest {
    type Rejection = Response;

    async fn from_request(req: Request<axum::body::Body>, state: &S) -> Result<Self, Self::Rejection> {
        let method = req.method().clone();
        let uri = req.uri().clone();

        match method {
            axum::http::Method::GET => {
                let query_str = uri.query().unwrap_or("");
                let params: HashMap<String, String> = serde_urlencoded::from_str(query_str)
                    .unwrap_or_default();
                let dict = params_to_dict(&params);
                Ok(HaystackRequest { grid: Grid::from_dict(dict) })
            }
            axum::http::Method::POST => {
                let body = ZincBody::from_request(req, state).await
                    .map_err(|(status, msg)| {
                        zinc_error_response(&HaystackError::from_status(status, &msg))
                    })?;
                Ok(HaystackRequest { grid: body.0 })
            }
            _ => Err(zinc_error_response(&HaystackError::MethodNotAllowed)),
        }
    }
}
```

---

## 7. Response Formatting (Zinc Content Type)

### C++ Pattern:

```cpp
void Op::on_service(Server &db, HTTPServerRequest &req, HTTPServerResponse &res) {
    res.setStatus(HTTPResponse::HTTP_OK);
    res.setContentType("text/zinc; charset=utf-8");
    std::ostream &ostr = res.send();
    ZincWriter w(ostr);
    try {
        Grid::auto_ptr_t g = on_service(db, *reqGrid);
        w.write_grid(*g);
    } catch (std::runtime_error &e) {
        w.write_grid(*Grid::make_err(e));
    }
}
```

### Rust Pattern:

```rust
/// Convert a Grid to an HTTP response with text/zinc content type.
pub fn zinc_response(grid: &Grid) -> Response<axum::body::Body> {
    let body = zinc::write_grid(grid);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/zinc; charset=utf-8")
        .body(axum::body::Body::from(body))
        .unwrap()
}

/// Convert a HaystackError to a zinc error grid response.
pub fn zinc_error_response(err: &HaystackError) -> Response<axum::body::Body> {
    let grid = Grid::make_err(&err.to_string());
    zinc_response(&grid)
}

/// Alternatively, implement IntoResponse for Grid directly:
impl IntoResponse for ZincGrid {
    fn into_response(self) -> Response {
        let body = zinc::write_grid(&self.0);
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/zinc; charset=utf-8")],
            body,
        ).into_response()
    }
}
```

---

## 8. Error Handling

### C++ Pattern (Exceptions):

```cpp
// In Op subclass:
throw std::runtime_error("Request has no rows");

// Caught in Op::on_service():
catch (std::runtime_error &e) {
    w.write_grid(*Grid::make_err(e));
}
```

All errors are caught and converted to zinc error grids. This means clients always get a 200 OK with an error grid, never an HTTP error status.

### Rust Pattern (Result + Custom Error Type):

```rust
use axum::http::StatusCode;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HaystackError {
    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Not acceptable: {0}")]
    NotAcceptable(String),

    #[error("Method not allowed")]
    MethodNotAllowed,

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Zinc parse error: {0}")]
    ZincParse(String),

    #[error("Engine FFI error: {0}")]
    EngineFfi(String),

    #[error("Watch not found: {0}")]
    WatchNotFound(String),

    #[error("Record missing tag: {record} missing '{tag}'")]
    MissingTag { record: String, tag: String },
}

impl HaystackError {
    /// Convert to HTTP status code.
    /// NOTE: For backward compatibility, most errors return 200 with a zinc error grid.
    /// Only content-type negotiation failures return non-200.
    pub fn status_code(&self) -> StatusCode {
        match self {
            HaystackError::NotAcceptable(_) => StatusCode::NOT_ACCEPTABLE,
            HaystackError::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            _ => StatusCode::OK, // Haystack convention: errors in zinc grid body
        }
    }
}

/// Convert HaystackError to a zinc error grid response (preserving Haystack convention).
impl IntoResponse for HaystackError {
    fn into_response(self) -> Response {
        let grid = Grid::make_err(&self.to_string());
        let body = zinc::write_grid(&grid);
        (
            self.status_code(),
            [(header::CONTENT_TYPE, "text/zinc; charset=utf-8")],
            body,
        ).into_response()
    }
}
```

**Key Advantage:** Rust's `Result<T, HaystackError>` makes error handling explicit at every call site. The `?` operator propagates errors cleanly. No invisible exception paths.

---

## 9. Hot-Reload of database.zinc

### C++ (Poco::DirectoryWatcher):

```cpp
// In PointServer constructor:
m_dirWatcher = new Poco::DirectoryWatcher(configDir,
    Poco::DirectoryWatcher::DW_ITEM_MODIFIED | Poco::DirectoryWatcher::DW_ITEM_ADDED);
m_dirWatcher->itemModified += Poco::delegate(this, &PointServer::on_zinc_changed);

// Handler sets atomic flag:
void PointServer::on_zinc_changed(const Poco::DirectoryWatcher::DirectoryEvent& event) {
    if (event.item.path().find("database.zinc") == std::string::npos) return;
    m_reloadPending = true;
}

// Timer checks the flag every 1 second:
void PointServer::on_timer(Poco::Timer &timer) {
    if (m_reloadPending.exchange(false)) {
        Poco::Thread::sleep(100);  // wait for write completion
        reload_zinc();
    }
}
```

### Rust (notify crate + tokio channel):

```rust
use notify::{Watcher, RecursiveMode, Event, EventKind};
use tokio::sync::mpsc;

async fn file_watcher_task(state: Arc<AppState>) {
    let (tx, mut rx) = mpsc::channel(10);

    // Create a synchronous watcher that sends events into our async channel
    let mut watcher = notify::recommended_watcher(move |res: Result<Event, _>| {
        if let Ok(event) = res {
            // Filter for database.zinc modifications
            let dominated_by_zinc = event.paths.iter().any(|p|
                p.file_name().map(|f| f == "database.zinc").unwrap_or(false)
            );
            if dominated_by_zinc {
                match event.kind {
                    EventKind::Modify(_) | EventKind::Create(_) => {
                        let _ = tx.blocking_send(());
                    }
                    _ => {}
                }
            }
        }
    }).expect("Failed to create file watcher");

    watcher.watch(
        state.config.config_dir.as_path(),
        RecursiveMode::NonRecursive,
    ).expect("Failed to watch config directory");

    tracing::info!("Watching {} for database.zinc changes", state.config.config_dir.display());

    // Debounce: wait 200ms after last event before reloading
    loop {
        if rx.recv().await.is_some() {
            // Drain any rapid-fire events (debounce)
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            while rx.try_recv().is_ok() {} // drain queue

            tracing::info!("database.zinc changed, reloading...");
            let mut server = state.point_server.write().await;
            if let Err(e) = server.reload_zinc(&state.config.zinc_file_path) {
                tracing::error!("Failed to reload database.zinc: {}", e);
            }
        }
    }
}
```

**Key Differences:**
- `notify` crate is cross-platform (uses inotify on Linux, the same backend as POCO)
- Debouncing is explicit with `tokio::time::sleep` instead of `Poco::Thread::sleep`
- No need for `std::atomic<bool>` flag; the mpsc channel itself is the signaling mechanism
- The watcher task is a proper async task, not a timer callback

---

## 10. Background Tasks (Timer Replacement)

### C++ Timer Architecture:

```cpp
Poco::Timer m_timer(1000, 1000);  // 1s granularity, 1s period
Poco::TimerCallback<PointServer> callback(*this, &PointServer::on_timer);
m_timer.start(callback);

void PointServer::on_timer(Poco::Timer &timer) {
    flush_if_dirty();
    timer_update();                          // every 1s
    if (count % m_timerWatches == 0)         // every 60s
        timer_watches();
    if (count % m_timerHistory == 0)         // every 60s
        timer_history();
}
```

### Rust (Separate Tokio Tasks):

```rust
async fn background_tasks(state: Arc<AppState>) {
    let update_state = Arc::clone(&state);
    let watches_state = Arc::clone(&state);
    let history_state = Arc::clone(&state);
    let flush_state = Arc::clone(&state);

    // Task 1: Update point values from engine (every 1 second)
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(update_state.config.timer_period_secs)
        );
        loop {
            interval.tick().await;
            timer_update(&update_state).await;
        }
    });

    // Task 2: Watch lease management (every 60 seconds)
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(watches_state.config.timer_watches_secs)
        );
        loop {
            interval.tick().await;
            timer_watches(&watches_state).await;
        }
    });

    // Task 3: History sampling (every 60 seconds)
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(history_state.config.timer_history_secs)
        );
        loop {
            interval.tick().await;
            timer_history(&history_state).await;
        }
    });

    // Task 4: Debounced zinc flush (check every 100ms)
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            interval.tick().await;
            flush_if_dirty(&flush_state).await;
        }
    });
}

async fn timer_update(state: &Arc<AppState>) {
    // Enumerate engine channels via FFI
    let engine_values = engine_ffi::enum_channels();

    // Acquire write lock only for the update phase
    let channel_index = state.channel_index.read().await;
    let mut records = state.records.write().await;

    for (channel, value) in &engine_values {
        if let Some(rec_id) = channel_index.get(channel) {
            if let Some(rec) = records.recs.get_mut(rec_id) {
                update_record_from_engine(rec, value);
            }
        }
    }
}

async fn timer_watches(state: &Arc<AppState>) {
    let mut watches = state.watches.write().await;
    let expired: Vec<String> = watches.watches.iter()
        .filter(|(_, w)| w.is_expired())
        .map(|(id, _)| id.clone())
        .collect();

    for id in expired {
        tracing::info!("Watch expired: {}", id);
        watches.watches.remove(&id);
    }
}

async fn timer_history(state: &Arc<AppState>) {
    let records = state.records.read().await;
    let mut history = state.history.write().await;

    for (id, rec) in &records.recs {
        if rec.has_marker("his") {
            let interpolate = rec.get_str("hisInterpolate").unwrap_or("linear");
            if interpolate == "linear" {
                if let Some(cur_val) = rec.get("curVal") {
                    let cur_status = rec.get_str("curStatus").unwrap_or("unknown");
                    let entry = history.history.entry(id.clone())
                        .or_insert_with(|| VecDeque::with_capacity(120));
                    entry.push_back(HisItem::new(Utc::now(), cur_val.clone()));
                    if entry.len() > 120 {
                        entry.pop_front();
                    }
                }
            }
        }
    }
}

async fn flush_if_dirty(state: &Arc<AppState>) {
    if state.dirty.load(Ordering::Relaxed) {
        let last_mod = *state.last_modified.lock().await;
        if last_mod.elapsed() >= std::time::Duration::from_millis(500) {
            tracing::debug!("Flushing database.zinc after 500ms debounce");
            let records = state.records.read().await;
            if let Err(e) = commit_zinc(&records, &state.config.zinc_file_path) {
                tracing::error!("Failed to commit zinc: {}", e);
            }
            state.dirty.store(false, Ordering::Relaxed);
        }
    }
}
```

**Advantages:**
- Each task is independently scheduled; no single monolithic timer callback
- Async `RwLock` means the update task does not block HTTP handlers
- Fine-grained locks: the flush task only reads records, while the watch task only writes watches
- Configurable intervals without a global counter

---

## 11. Haystack Filter System

### C++ Architecture (filter.cpp/hpp, 700 lines):

```
Filter (abstract)
├── PathFilter (abstract) -- evaluates tag paths
│   ├── Has           -- tag exists
│   ├── Missing       -- tag does not exist
│   └── CmpFilter     -- comparison with value
│       ├── Eq (==)
│       ├── Ne (!=)
│       ├── Lt (<)
│       ├── Le (<=)
│       ├── Gt (>)
│       └── Ge (>=)
└── CompoundFilter (abstract) -- combines two filters
    ├── And
    └── Or
```

Parsing: `Filter::make(string)` calls `ZincReader::read_filter()` which builds the AST.

Evaluation: `filter->include(dict, pather)` recursively evaluates against a `Dict`.

### Rust Architecture:

```rust
/// Haystack filter AST (replaces the C++ class hierarchy).
#[derive(Debug, Clone)]
pub enum Filter {
    Has(Path),
    Missing(Path),
    Eq(Path, Val),
    Ne(Path, Val),
    Lt(Path, Val),
    Le(Path, Val),
    Gt(Path, Val),
    Ge(Path, Val),
    And(Box<Filter>, Box<Filter>),
    Or(Box<Filter>, Box<Filter>),
}

#[derive(Debug, Clone)]
pub enum Path {
    Simple(String),
    Nested(Vec<String>),  // "equipRef->siteRef" = ["equipRef", "siteRef"]
}

impl Filter {
    /// Parse a filter string. Returns Result instead of throwing.
    pub fn parse(s: &str) -> Result<Filter, FilterParseError> {
        filter_parser::parse(s)
    }

    /// Evaluate filter against a record.
    pub fn matches(&self, dict: &Dict, pather: &dyn Pather) -> bool {
        match self {
            Filter::Has(path) => resolve_path(path, dict, pather)
                .map(|v| !v.is_empty())
                .unwrap_or(false),

            Filter::Missing(path) => resolve_path(path, dict, pather)
                .map(|v| v.is_empty())
                .unwrap_or(true),

            Filter::Eq(path, expected) => resolve_path(path, dict, pather)
                .map(|v| v == *expected)
                .unwrap_or(false),

            Filter::Ne(path, expected) => resolve_path(path, dict, pather)
                .map(|v| !v.is_empty() && v != *expected)
                .unwrap_or(false),

            Filter::Lt(path, expected) => resolve_path(path, dict, pather)
                .map(|v| v.same_type(expected) && v < *expected)
                .unwrap_or(false),

            Filter::Le(path, expected) => resolve_path(path, dict, pather)
                .map(|v| v.same_type(expected) && v <= *expected)
                .unwrap_or(false),

            Filter::Gt(path, expected) => resolve_path(path, dict, pather)
                .map(|v| v.same_type(expected) && v > *expected)
                .unwrap_or(false),

            Filter::Ge(path, expected) => resolve_path(path, dict, pather)
                .map(|v| v.same_type(expected) && v >= *expected)
                .unwrap_or(false),

            Filter::And(a, b) => a.matches(dict, pather) && b.matches(dict, pather),
            Filter::Or(a, b) => a.matches(dict, pather) || b.matches(dict, pather),
        }
    }
}

fn resolve_path(path: &Path, dict: &Dict, pather: &dyn Pather) -> Option<Val> {
    match path {
        Path::Simple(name) => dict.get(name).cloned(),
        Path::Nested(names) => {
            let mut val = dict.get(&names[0])?.clone();
            for name in &names[1..] {
                match &val {
                    Val::Ref(ref_str) => {
                        let target = pather.find(ref_str)?;
                        val = target.get(name)?.clone();
                    }
                    _ => return None,
                }
            }
            Some(val)
        }
    }
}

pub trait Pather {
    fn find(&self, ref_id: &str) -> Option<&Dict>;
}
```

**Key Differences:**
- Rust `enum` replaces C++ class hierarchy (12 classes -> 1 enum with 10 variants)
- Pattern matching replaces virtual dispatch (`match self` instead of `virtual bool do_include()`)
- `Option<Val>` replaces the `EmptyVal::DEF` sentinel
- `Box<Filter>` for recursive variants replaces `boost::shared_ptr<Filter>`
- Parsing returns `Result` instead of throwing exceptions

---

## 12. Debounced Zinc Commit (Atomic Write)

### C++ Pattern:

```cpp
void PointServer::commit_zinc(void) {
    Poco::ScopedWriteRWLock l(m_lock);
    Grid::auto_ptr_t recordGrid = Grid::make(m_recs);

    std::string tempPath = configDir + "/database.zinc.tmp";
    std::string backupPath = configDir + "/database.zinc.bak";
    std::string finalPath = configDir + "/database.zinc";

    // Write to temp -> verify -> rename backup -> atomic rename temp to final
    std::ofstream fZinc(tempPath.c_str(), std::ios::out | std::ios::trunc);
    ZincWriter w(fZinc);
    w.write_grid(*recordGrid);
    fZinc.flush();
    if (!fZinc.good()) { /* rollback */ }
    std::rename(finalPath, backupPath);
    std::rename(tempPath, finalPath);
    m_dirty = false;
}
```

### Rust Pattern:

```rust
/// Atomic write of database.zinc using temp file + rename.
async fn commit_zinc(records: &RecordStore, zinc_path: &Path) -> Result<(), std::io::Error> {
    let temp_path = zinc_path.with_extension("zinc.tmp");
    let backup_path = zinc_path.with_extension("zinc.bak");

    // Step 1: Write to temp file
    let grid = records.to_grid();
    let zinc_str = zinc::write_grid(&grid);
    tokio::fs::write(&temp_path, zinc_str.as_bytes()).await?;

    // Step 2: Create backup (ignore if original doesn't exist)
    let _ = tokio::fs::rename(zinc_path, &backup_path).await;

    // Step 3: Atomic rename (POSIX guarantee)
    match tokio::fs::rename(&temp_path, zinc_path).await {
        Ok(()) => {
            tracing::debug!("Successfully wrote database.zinc");
            Ok(())
        }
        Err(e) => {
            // Restore backup on failure
            let _ = tokio::fs::rename(&backup_path, zinc_path).await;
            Err(e)
        }
    }
}
```

---

## 13. Authentication

The current C++ implementation has **no authentication**. All endpoints are open. The CORS configuration (`AllowedCorsUrl.config`) provides origin-based access control but not authentication.

For the Rust migration, the authentication layer can be added as tower middleware without changing any handler code:

```rust
use axum::middleware;
use tower::ServiceBuilder;

// If/when authentication is needed:
let app = Router::new()
    // ... routes ...
    .layer(
        ServiceBuilder::new()
            .layer(cors)
            // Optional: Add auth middleware when ready
            // .layer(middleware::from_fn(auth_middleware))
    )
    .with_state(state);

// Example bearer token middleware (for future use):
async fn auth_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // Check Authorization header
    if let Some(auth) = req.headers().get(header::AUTHORIZATION) {
        if validate_token(auth).is_ok() {
            return next.run(req).await;
        }
    }
    (StatusCode::UNAUTHORIZED, "Invalid or missing auth token").into_response()
}
```

---

## 14. Graceful Shutdown

### C++ (No Graceful Shutdown):

The current system relies on `willTerminate()` returning true in the timer callback, and the RestartOp calls `exit(0)`. POCO's HTTP server does not have built-in graceful shutdown.

### Rust (Built-in to Axum + Tokio):

```rust
axum::serve(listener, app)
    .with_graceful_shutdown(shutdown_signal())
    .await
    .unwrap();

async fn shutdown_signal() {
    let ctrl_c = signal::ctrl_c();
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate()).unwrap();

    tokio::select! {
        _ = ctrl_c => tracing::info!("Received Ctrl+C"),
        _ = sigterm.recv() => tracing::info!("Received SIGTERM"),
    }

    // Perform cleanup:
    // 1. Stop accepting new connections
    // 2. Wait for in-flight requests to complete (with timeout)
    // 3. Flush dirty zinc to disk
    // 4. Close file watchers
}
```

**Advantage:** Axum's graceful shutdown waits for in-flight requests to complete before exiting. No data loss, no interrupted writes.

---

## 15. Dependency Comparison

### Lines of Code

| Component | C++ (POCO) | Rust (Axum) |
|-----------|-----------|-------------|
| HTTP server framework | ~500,000 (POCO vendored) | ~5,000 (axum) |
| CORS handling | ~30 lines manual | ~10 lines (tower-http CorsLayer) |
| URI / query parsing | POCO::URI + HTMLForm | axum::extract::Query (serde) |
| XML parsing | POCO::XML (~50K lines) | quick-xml (~3K lines) |
| HTTP client | POCO::Net::HTTPClientSession | reqwest (~10K lines) |
| Directory watching | POCO::DirectoryWatcher | notify (~5K lines) |
| Threading / timer | POCO::Timer + RWLock + Thread | tokio (async runtime) |
| **Total framework** | **~500,000 lines vendored** | **~25,000 lines total deps** |
| Custom Haystack code | ~6,900 lines C++ | ~3,000 lines Rust (estimated) |

### Binary Size Impact (ARM7)

| Component | C++ | Rust |
|-----------|-----|------|
| POCO shared libs | ~8MB (.so files) | N/A |
| HTTP server in binary | Linked dynamically | ~1.2MB statically linked |
| Total binary delta | | -6.8MB (estimated) |

### Compile Time

| Metric | C++ (POCO) | Rust (Axum) |
|--------|-----------|-------------|
| Full rebuild | ~15 minutes (Docker ARM cross) | ~5 minutes (cargo cross) |
| Incremental | ~3 minutes | ~10 seconds |

---

## 16. Migration Strategy

### Phase 1: Scaffold Axum Server (Week 1)

1. Create `sandstar_rust/src/haystack/server.rs` with the full router
2. Implement `AppState` with `RwLock<RecordStore>`
3. Port `ZincBody` extractor and `zinc_response()` helper
4. Implement `/about`, `/ops`, `/formats` (stateless, easy to verify)
5. Set up CORS middleware with `tower-http`

### Phase 2: Port Core Operations (Weeks 2-3)

1. Port `ReadOp` with full filter evaluation (`filter.rs`)
2. Port `PointWriteOp` with engine FFI bridge
3. Port `CommitOp` (add/update/delete/optimize/zinc)
4. Port `WatchSub/Unsub/Poll` with `WatchStore`

### Phase 3: Background Tasks (Week 4)

1. Port `timer_update()` with engine channel enumeration
2. Port `timer_watches()` for lease expiry
3. Port `timer_history()` for history sampling
4. Port hot-reload with `notify` crate
5. Port debounced `flush_if_dirty()` with `commit_zinc()`

### Phase 4: Extended Operations (Week 5)

1. Port `NavOp` including Sedona component list (reqwest + quick-xml)
2. Port `RootOp` with full Sedona component tree navigation
3. Port `XetoOp` with points.csv parsing and spec generation
4. Port `HisReadOp` / `HisWriteOp`

### Phase 5: Integration Testing (Week 6)

1. Run both C++ and Rust servers on different ports
2. Compare responses for every operation
3. Load test with concurrent clients
4. Verify hot-reload behavior
5. Test graceful shutdown under load

### Verification Criteria

Each operation is considered migrated when:
- Same zinc output for identical requests (byte-level comparison)
- Same error grids for invalid inputs
- CORS headers match for all origins
- Watch subscriptions survive across poll intervals
- Priority array writes propagate to engine and back

---

## 17. Summary

The migration from POCO C++ to Axum Rust transforms the Haystack REST API from a 500K-line vendored dependency into a focused, type-safe HTTP layer. The key architectural changes are:

1. **Dispatch:** Virtual method dispatch through `Op` class hierarchy becomes direct function routing via `Router::new().route()`
2. **State:** Global static variables with `Poco::RWLock` become `Arc<AppState>` with fine-grained `tokio::sync::RwLock`
3. **Concurrency:** Thread pool + blocking locks become async tasks + non-blocking locks
4. **CORS:** Manual header setting in every request becomes a single `CorsLayer` middleware
5. **Error handling:** Exception-based with `try/catch` becomes `Result<T, HaystackError>` with `?` operator
6. **File watching:** `Poco::DirectoryWatcher` becomes `notify` crate with async channel
7. **Timers:** Monolithic `Poco::Timer` callback becomes independent `tokio::time::interval` tasks
8. **XML parsing:** `Poco::XML::DOMParser` (DOM allocation) becomes `quick-xml` (streaming SAX)
9. **HTTP client:** `Poco::Net::HTTPClientSession` becomes `reqwest::Client` (async, connection-pooled)
10. **Memory safety:** Manual `new`/`delete` with `auto_ptr` and `ptr_vector` becomes automatic ownership

The result is a server that is smaller (20x fewer dependency lines), faster (async I/O), safer (no null dereferences, no buffer overflows, no use-after-free), and more maintainable (explicit error handling, no hidden global state).
