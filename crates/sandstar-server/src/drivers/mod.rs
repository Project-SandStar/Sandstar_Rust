//! Driver Framework v2 — core traits, polling, watch subscriptions, and status inheritance.
//!
//! Provides a unified abstraction for hardware and protocol drivers. Each
//! driver implements the [`Driver`] or [`AsyncDriver`] trait and is managed
//! by the actor-based [`DriverHandle`] (see [`spawn_driver_actor`]).
//!
//! ## Architecture (Haxall-inspired)
//!
//! - **Lifecycle callbacks**: `open`, `close`, `ping` for driver lifecycle
//! - **Poll buckets**: [`PollScheduler`] batches point reads with stagger offsets
//! - **Watch/COV**: [`DriverWatchManager`] tracks point subscriptions
//! - **Status inheritance**: [`PointStatus`] inherits from parent driver by default
//! - **Typed errors**: Distinguish config faults, comm errors, timeouts, etc.
//!
//! ## Available Drivers
//!
//! | Module | Driver | Status |
//! |--------|--------|--------|
//! | [`local_io`] | `LocalIoDriver` — BeagleBone GPIO/ADC/I2C/PWM via HAL | Active |
//! | [`modbus`] | `ModbusDriver` — Modbus TCP (frame-level I/O) | Active |
//! | [`bacnet`] | `BacnetDriver` — BACnet/IP | Stub |
//! | [`mqtt`] | `MqttDriver` — MQTT pub/sub | Stub |

pub mod actor;
pub mod async_driver;
pub mod bacnet;
pub mod loader;
pub mod local_io;
pub mod modbus;
pub mod mqtt;
pub mod poll_scheduler;
pub mod watch_manager;

#[cfg(test)]
mod mqtt_e2e_test;

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

pub use actor::{spawn_driver_actor, DriverCmd, DriverHandle, DEFAULT_COV_CAPACITY};
pub use async_driver::{AnyDriver, AsyncDriver};
pub use poll_scheduler::PollScheduler;
pub use watch_manager::DriverWatchManager;

// ── Core Types ──────────────────────────────────────────────

/// Unique driver instance identifier.
pub type DriverId = String;

/// Result of writing to driver points: per-point write results.
pub type WritePointsResult = Result<Vec<(u32, Result<(), DriverError>)>, DriverError>;

/// Driver operational status (cascades to child points via [`PointStatus`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DriverStatus {
    /// Driver not yet initialized.
    Pending,
    /// Driver running normally.
    Ok,
    /// No recent data received within stale timeout.
    Stale,
    /// Communication/hardware fault.
    Fault(String),
    /// Driver disabled by configuration.
    Disabled,
    /// Driver shut down.
    Down,
    /// Initial data load / synchronization in progress.
    Syncing,
}

impl fmt::Display for DriverStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Ok => write!(f, "ok"),
            Self::Stale => write!(f, "stale"),
            Self::Fault(msg) => write!(f, "fault: {msg}"),
            Self::Disabled => write!(f, "disabled"),
            Self::Down => write!(f, "down"),
            Self::Syncing => write!(f, "syncing"),
        }
    }
}

/// Driver metadata returned from [`Driver::open`] and [`Driver::ping`].
#[derive(Debug, Clone, Default)]
pub struct DriverMeta {
    pub firmware_version: Option<String>,
    pub model: Option<String>,
    pub extra: HashMap<String, String>,
}

/// Driver error types (Haxall-inspired categorization).
#[derive(Debug, Clone)]
pub enum DriverError {
    /// Configuration error (bad address, missing parameter) — won't recover without fix.
    ConfigFault(String),
    /// Communication error (timeout, connection refused) — may recover on retry.
    CommFault(String),
    /// Feature not supported by this driver.
    NotSupported(&'static str),
    /// Remote device/system reported an error status.
    RemoteStatus(String),
    /// Communication timeout waiting for response.
    Timeout(String),
    /// Internal driver error (logic bug, unexpected state).
    Internal(String),
    /// Hardware device not found or inaccessible.
    HardwareNotFound(String),
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConfigFault(msg) => write!(f, "config fault: {msg}"),
            Self::CommFault(msg) => write!(f, "comm fault: {msg}"),
            Self::NotSupported(feat) => write!(f, "not supported: {feat}"),
            Self::RemoteStatus(msg) => write!(f, "remote status: {msg}"),
            Self::Timeout(msg) => write!(f, "timeout: {msg}"),
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
            Self::HardwareNotFound(msg) => write!(f, "hardware not found: {msg}"),
        }
    }
}

impl std::error::Error for DriverError {}

/// Point reference for batch operations.
#[derive(Debug, Clone)]
pub struct DriverPointRef {
    /// Channel/point ID.
    pub point_id: u32,
    /// Driver-specific address (e.g., "AIN0", "40001", "AI:1").
    pub address: String,
}

/// A discovered point from [`Driver::learn`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearnPoint {
    pub name: String,
    pub address: String,
    /// Point kind: "Number", "Bool", "Str".
    pub kind: String,
    pub unit: Option<String>,
    /// Additional metadata tags.
    pub tags: HashMap<String, String>,
}

/// Result of a learn operation.
pub type LearnGrid = Vec<LearnPoint>;

/// Poll configuration mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PollMode {
    /// Automatic bucket-based polling (default).
    Buckets,
    /// Manual polling (driver controls timing).
    Manual,
}

// ── Point Status (with inheritance) ────────────────────────

/// Point-level status that can inherit from its parent driver OR carry
/// a remote-reported per-point status distinct from the driver's status.
///
/// By default, points inherit their driver's status. A point can:
/// - override with [`PointStatus::Own`] (local explicit status)
/// - or carry a `Remote*` variant when the remote device reports a
///   point-specific error state (see Phase 12.0B of the Driver Framework
///   plan, and research doc 18 §"Status Model").
///
/// The `Remote*` variants are **terminal** — they do NOT fall back to the
/// driver's status. This lets callers distinguish "device is up but this
/// point is faulted" from "driver is down so everything looks down".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum PointStatus {
    /// Point inherits status from its parent driver (default).
    #[default]
    Inherited,
    /// Point has its own explicit status (local override).
    Own(DriverStatus),
    /// Remote reports this specific point is disabled.
    RemoteDisabled,
    /// Remote reports this specific point is down / unreachable.
    RemoteDown,
    /// Remote reports this specific point has a fault, with a descriptive reason.
    RemoteFault(String),
}

impl PointStatus {
    /// Resolve the effective status given the parent driver status.
    ///
    /// If the point has its own status, that takes precedence.
    /// For `Remote*` variants, a synthetic [`DriverStatus`] is returned that
    /// reflects the remote-reported state (terminal — does NOT fall back to
    /// the driver's status).
    pub fn resolve(&self, driver_status: &DriverStatus) -> DriverStatus {
        match self {
            PointStatus::Own(s) => s.clone(),
            PointStatus::Inherited => driver_status.clone(),
            PointStatus::RemoteDisabled => DriverStatus::Disabled,
            PointStatus::RemoteDown => DriverStatus::Down,
            PointStatus::RemoteFault(msg) => DriverStatus::Fault(msg.clone()),
        }
    }

    /// Map a [`DriverError`] to the most appropriate [`PointStatus`] for
    /// per-point error reporting. Called by the actor when `sync_cur`
    /// returns `(point_id, Err(driver_err))`.
    ///
    /// - `RemoteStatus` → `RemoteFault(msg)` — remote device explicitly
    ///   reported an error status for this point
    /// - `CommFault` / `Timeout` → `RemoteDown` — can't reach the remote
    /// - `ConfigFault` / `HardwareNotFound` / `Internal` / `NotSupported`
    ///   → `Own(DriverStatus::Fault(msg))` — local config / logic fault
    pub fn from_driver_error(err: &DriverError) -> Self {
        match err {
            DriverError::RemoteStatus(msg) => PointStatus::RemoteFault(msg.clone()),
            DriverError::CommFault(_) | DriverError::Timeout(_) => PointStatus::RemoteDown,
            DriverError::ConfigFault(msg)
            | DriverError::HardwareNotFound(msg)
            | DriverError::Internal(msg) => PointStatus::Own(DriverStatus::Fault(msg.clone())),
            DriverError::NotSupported(feat) => {
                PointStatus::Own(DriverStatus::Fault(format!("not supported: {feat}")))
            }
        }
    }

    /// `true` if this status represents a remote-reported per-point state.
    pub fn is_remote(&self) -> bool {
        matches!(
            self,
            PointStatus::RemoteDisabled | PointStatus::RemoteDown | PointStatus::RemoteFault(_)
        )
    }
}

// ── Driver-specific custom messages (Phase 12.0E) ───────────

/// A driver-specific custom message.
///
/// Used by [`Driver::on_receive`] / [`AsyncDriver::on_receive`] so consumers
/// can send a typed command to a specific driver without having to extend
/// the core [`DriverCmd`] enum per feature. Examples: force a BACnet
/// Who-Is, request MQTT reconnect, set a Modbus watchdog.
///
/// Research doc 18 §"Custom Messages" — Haxall-inspired shape.
///
/// Both the request and response share the same shape: drivers return a
/// `DriverMessage` with a matching or well-known `id` plus any response
/// payload (e.g., `{id: "whoIsResult", payload: {"devices": [...]}}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriverMessage {
    /// Message-type identifier (e.g. `"whoIs"`, `"reconnect"`, `"stats"`).
    /// Drivers decide their own vocabulary; unknown ids should return
    /// [`DriverError::NotSupported`].
    pub id: String,
    /// Driver-specific payload. Empty object `{}` is valid when the id
    /// alone is self-describing.
    #[serde(default)]
    pub payload: serde_json::Value,
}

impl DriverMessage {
    /// Construct a message with an id and empty payload.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            payload: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    /// Construct a message with an id and a payload.
    pub fn with_payload(id: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            id: id.into(),
            payload,
        }
    }
}

// ── Change-of-Value (COV) event ─────────────────────────────

/// Event broadcast when a point's current value changes during a sync cycle.
///
/// Emitted by the driver actor on successful `sync_cur` reads whose value
/// differs from the last emitted value for that point. Consumers (REST
/// WebSocket bridges, SOX COV push, metrics exporters) subscribe via
/// [`crate::drivers::actor::DriverHandle::subscribe_cov`].
///
/// The `status` field is included so subscribers can see the currently
/// effective per-point status (resolved or inherited) — change-of-status
/// is NOT what gates emission; value change is. See research doc 18
/// §"Broadcast CovEvent".
#[derive(Debug, Clone)]
pub struct CovEvent {
    /// Channel/point ID whose value changed.
    pub point_id: u32,
    /// The new value (the one that differed from the last emitted).
    pub value: f64,
    /// Snapshot of the point's current effective [`PointStatus`] at emit time.
    pub status: PointStatus,
    /// Wall-clock-ish timestamp; use `Instant::elapsed()` for a duration.
    pub timestamp: std::time::Instant,
}

// ── Sync / Write contexts (callback-style results) ──────────

/// Result collector passed to [`Driver::sync_cur`] / [`AsyncDriver::sync_cur`].
///
/// Drivers call [`SyncContext::update_cur_ok`] or
/// [`SyncContext::update_cur_err`] per point instead of returning a `Vec`.
/// The wrapper ([`AnyDriver::sync_cur`]) creates a fresh context per call
/// and drains it into the `(point_id, Result<f64, DriverError>)` vector
/// the actor and REST layer already expect — so this refactor is internal
/// to the trait surface, with no behavior change.
///
/// This matches the Haxall `SyncContext` shape from research doc 18.
#[derive(Debug, Default)]
pub struct SyncContext {
    results: Vec<(u32, Result<f64, DriverError>)>,
}

impl SyncContext {
    /// Create a new, empty sync context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-size the internal buffer for a known batch size.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            results: Vec::with_capacity(cap),
        }
    }

    /// Record a successful read.
    pub fn update_cur_ok(&mut self, point_id: u32, value: f64) {
        self.results.push((point_id, Ok(value)));
    }

    /// Record a failed read.
    pub fn update_cur_err(&mut self, point_id: u32, err: DriverError) {
        self.results.push((point_id, Err(err)));
    }

    /// Consume the context and return the collected results.
    pub fn into_results(self) -> Vec<(u32, Result<f64, DriverError>)> {
        self.results
    }

    /// Borrow the collected results (mostly for tests).
    pub fn results(&self) -> &[(u32, Result<f64, DriverError>)] {
        &self.results
    }
}

/// Result collector passed to [`Driver::write`] / [`AsyncDriver::write`].
///
/// Drivers call [`WriteContext::update_write_ok`] or
/// [`WriteContext::update_write_err`] per point instead of returning a `Vec`.
#[derive(Debug, Default)]
pub struct WriteContext {
    results: Vec<(u32, Result<(), DriverError>)>,
}

impl WriteContext {
    /// Create a new, empty write context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-size the internal buffer for a known batch size.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            results: Vec::with_capacity(cap),
        }
    }

    /// Record a successful write.
    pub fn update_write_ok(&mut self, point_id: u32) {
        self.results.push((point_id, Ok(())));
    }

    /// Record a failed write.
    pub fn update_write_err(&mut self, point_id: u32, err: DriverError) {
        self.results.push((point_id, Err(err)));
    }

    /// Consume the context and return the collected results.
    pub fn into_results(self) -> Vec<(u32, Result<(), DriverError>)> {
        self.results
    }

    /// Borrow the collected results (mostly for tests).
    pub fn results(&self) -> &[(u32, Result<(), DriverError>)] {
        &self.results
    }
}

// ── Driver Trait ────────────────────────────────────────────

/// Driver lifecycle and I/O callbacks.
///
/// Each driver instance manages a set of points (channels) and provides
/// read/write access. The actor-based [`DriverHandle`] orchestrates
/// lifecycle and batch operations across all registered drivers.
pub trait Driver: Send + Sync {
    /// The driver type name (e.g., "localIo", "modbus", "bacnet").
    fn driver_type(&self) -> &'static str;

    /// Unique instance identifier.
    fn id(&self) -> &str;

    /// Current operational status.
    fn status(&self) -> &DriverStatus;

    /// Initialize the driver. Called once on startup.
    fn open(&mut self) -> Result<DriverMeta, DriverError>;

    /// Shut down the driver. Called on shutdown.
    fn close(&mut self);

    /// Health check. Called periodically by the manager.
    fn ping(&mut self) -> Result<DriverMeta, DriverError>;

    /// Discover available points at the given path (or root if `None`).
    fn learn(&mut self, _path: Option<&str>) -> Result<LearnGrid, DriverError> {
        Err(DriverError::NotSupported("learn"))
    }

    /// Read current values for a batch of points.
    ///
    /// Drivers populate the provided [`SyncContext`] by calling
    /// `ctx.update_cur_ok(id, value)` or `ctx.update_cur_err(id, err)` for
    /// each point. The wrapper drains the context into the
    /// `(point_id, Result<f64, _>)` vector the actor expects.
    fn sync_cur(&mut self, points: &[DriverPointRef], ctx: &mut SyncContext);

    /// Write values to points.
    ///
    /// Drivers populate the provided [`WriteContext`] by calling
    /// `ctx.update_write_ok(id)` or `ctx.update_write_err(id, err)` for
    /// each point.
    fn write(&mut self, writes: &[(u32, f64)], ctx: &mut WriteContext);

    /// How this driver wants to be polled.
    fn poll_mode(&self) -> PollMode {
        PollMode::Buckets
    }

    /// Subscribe to change-of-value notifications for these points.
    ///
    /// Called when a client subscribes to point updates. For COV-capable
    /// protocols (BACnet SubscribeCOV, MQTT topics), this establishes
    /// the subscription on the remote system. Polling-based drivers can
    /// use the default no-op implementation.
    fn on_watch(&mut self, _points: &[DriverPointRef]) -> Result<(), DriverError> {
        Ok(()) // Default: no-op (polling-based drivers don't need this)
    }

    /// Unsubscribe from change-of-value notifications.
    ///
    /// Called when all clients have unsubscribed from these points.
    fn on_unwatch(&mut self, _points: &[DriverPointRef]) -> Result<(), DriverError> {
        Ok(()) // Default: no-op
    }

    /// Handle a driver-specific custom message (Phase 12.0E).
    ///
    /// Drivers that accept out-of-band commands (force re-discovery,
    /// request stats, reconnect, etc.) can override this. The default
    /// returns [`DriverError::NotSupported`] so drivers opt in per-id.
    fn on_receive(&mut self, _msg: DriverMessage) -> Result<DriverMessage, DriverError> {
        Err(DriverError::NotSupported("on_receive"))
    }
}

/// Summary info for a driver (serialized for REST API).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverSummary {
    pub id: String,
    pub driver_type: String,
    pub status: DriverStatus,
    pub poll_mode: String,
    /// Number of poll buckets assigned to this driver.
    pub poll_buckets: usize,
    /// Total number of points across all poll buckets.
    pub poll_points: usize,
}

// ── REST API handlers ───────────────────────────────────────

/// Build an Axum router for driver REST endpoints backed by the async [`DriverHandle`] actor.
///
/// The router is split into public read-only routes and auth-gated mutating
/// routes (Phase 12.0G). The mutating routes are:
/// `POST /api/drivers` (create), `POST /api/drivers/{id}/{open,close,ping,write}`,
/// `DELETE /api/drivers/{id}`, and `POST /api/syncCur`. They are gated by
/// [`crate::rest::check_auth`] — no auth is required when `auth_state`
/// reports no authentication configured (unchanged behavior for dev).
///
/// Mount under the main router with `.merge(drivers::driver_router(handle, auth_state))`.
pub fn driver_router(
    handle: crate::drivers::actor::DriverHandle,
    auth_state: crate::auth::AuthState,
) -> axum::Router {
    use axum::middleware;
    use axum::routing::{delete, get, post};

    let public = axum::Router::new()
        .route("/api/drivers", get(list_drivers_async))
        .route("/api/drivers/{id}/status", get(driver_status_async))
        .route("/api/drivers/{id}/learn", get(driver_learn_async))
        .with_state(handle.clone());

    let protected = axum::Router::new()
        .route("/api/drivers", post(create_driver_async))
        .route("/api/drivers/{id}", delete(delete_driver_async))
        .route("/api/drivers/{id}/open", post(open_driver_async))
        .route("/api/drivers/{id}/close", post(close_driver_async))
        .route("/api/drivers/{id}/ping", post(ping_driver_async))
        .route("/api/drivers/{id}/message", post(driver_message_async))
        .route("/api/drivers/{id}/write", post(driver_write_async))
        .route("/api/syncCur", post(sync_cur_async))
        .route_layer(middleware::from_fn_with_state(
            auth_state,
            crate::rest::check_auth,
        ))
        .with_state(handle);

    public.merge(protected)
}

// ── Async REST handlers (DriverHandle-backed) ────────────────

use axum::extract::Path;
use axum::response::IntoResponse;
use axum::Json;

/// GET /api/drivers — list all registered drivers with status.
async fn list_drivers_async(
    axum::extract::State(handle): axum::extract::State<crate::drivers::actor::DriverHandle>,
) -> impl IntoResponse {
    match handle.status().await {
        Ok(summaries) => {
            let json: Vec<serde_json::Value> = summaries
                .into_iter()
                .map(|s| {
                    serde_json::json!({
                        "id": s.id,
                        "driverType": s.driver_type,
                        "status": s.status,
                        "pollMode": s.poll_mode,
                        "pollBuckets": s.poll_buckets,
                        "pollPoints": s.poll_points,
                    })
                })
                .collect();
            (axum::http::StatusCode::OK, Json(serde_json::json!(json))).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /api/drivers/{id}/status — get driver status with point statuses.
async fn driver_status_async(
    Path(id): Path<String>,
    axum::extract::State(handle): axum::extract::State<crate::drivers::actor::DriverHandle>,
) -> impl IntoResponse {
    match handle.driver_status(&id).await {
        Ok(Some((status, point_statuses))) => {
            let points: Vec<serde_json::Value> = point_statuses
                .into_iter()
                .map(|(pid, ps)| {
                    serde_json::json!({
                        "pointId": pid,
                        "status": ps,
                    })
                })
                .collect();
            Json(serde_json::json!({
                "id": id,
                "status": status,
                "points": points,
            }))
            .into_response()
        }
        Ok(None) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("driver '{id}' not found")})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /api/drivers/{id}/learn — discover points from a driver.
async fn driver_learn_async(
    Path(id): Path<String>,
    axum::extract::State(handle): axum::extract::State<crate::drivers::actor::DriverHandle>,
) -> impl IntoResponse {
    match handle.learn(&id, None).await {
        Ok(grid) => Json(serde_json::json!({
            "driverId": id,
            "points": grid,
        }))
        .into_response(),
        Err(DriverError::NotSupported(_)) => (
            axum::http::StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({"error": "learn not supported by this driver"})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/drivers/{id}/write — write values to driver points.
///
/// Request body: `{"writes": [[pointId, value], ...]}`.
/// Returns per-point results.
async fn driver_write_async(
    Path(id): Path<String>,
    axum::extract::State(handle): axum::extract::State<crate::drivers::actor::DriverHandle>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let writes: Vec<(u32, f64)> = match body.get("writes").and_then(|w| w.as_array()) {
        Some(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                if let Some(pair) = item.as_array() {
                    if pair.len() == 2 {
                        if let (Some(pid), Some(val)) = (pair[0].as_u64(), pair[1].as_f64()) {
                            out.push((pid as u32, val));
                        }
                    }
                }
            }
            out
        }
        None => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({"error": "expected {\"writes\": [[pointId, value], ...]}"}),
                ),
            )
                .into_response();
        }
    };

    match handle.write(&id, writes).await {
        Ok(results) => {
            let items: Vec<serde_json::Value> = results
                .into_iter()
                .map(|(pid, r)| match r {
                    Ok(()) => serde_json::json!({"pointId": pid, "ok": true}),
                    Err(e) => {
                        serde_json::json!({"pointId": pid, "ok": false, "error": e.to_string()})
                    }
                })
                .collect();
            Json(serde_json::json!({
                "driverId": id,
                "results": items,
            }))
            .into_response()
        }
        Err(e) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ── Phase 12.0G: runtime driver lifecycle endpoints ─────────

/// POST /api/drivers — create a new driver at runtime.
///
/// Request body:
/// ```json
/// { "driver_type": "bacnet" | "mqtt", "config": { ... } }
/// ```
///
/// The `config` must match the corresponding driver's JSON schema
/// (`BacnetConfig` or `MqttConfig`). Returns `{"ok": true, "id": "..."}`
/// on success. The driver is registered but NOT auto-opened — call
/// `POST /api/drivers/{id}/open` to bring it up.
async fn create_driver_async(
    axum::extract::State(handle): axum::extract::State<crate::drivers::actor::DriverHandle>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let driver_type = match body.get("driver_type").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({"error": "missing or non-string 'driver_type'"}),
                ),
            )
                .into_response();
        }
    };
    let config = match body.get("config") {
        Some(c) => c.clone(),
        None => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "missing 'config'"})),
            )
                .into_response();
        }
    };

    let (driver_id, any_driver) = match build_driver_by_type(&driver_type, config) {
        Ok(pair) => pair,
        Err(msg) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": msg})),
            )
                .into_response();
        }
    };

    match handle.register(any_driver).await {
        Ok(()) => (
            axum::http::StatusCode::CREATED,
            Json(serde_json::json!({"ok": true, "id": driver_id})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::CONFLICT,
            Json(serde_json::json!({"ok": false, "error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Factory dispatch for runtime driver creation. Returns `(id, AnyDriver)`
/// or a user-friendly error message.
fn build_driver_by_type(
    driver_type: &str,
    config: serde_json::Value,
) -> Result<(String, crate::drivers::async_driver::AnyDriver), String> {
    match driver_type {
        "bacnet" => {
            let cfg: crate::drivers::bacnet::BacnetConfig = serde_json::from_value(config)
                .map_err(|e| format!("invalid bacnet config: {e}"))?;
            let id = cfg.id.clone();
            let driver = crate::drivers::bacnet::BacnetDriver::from_config(cfg);
            Ok((
                id,
                crate::drivers::async_driver::AnyDriver::Async(Box::new(driver)),
            ))
        }
        "mqtt" => {
            let cfg: crate::drivers::mqtt::MqttConfig =
                serde_json::from_value(config).map_err(|e| format!("invalid mqtt config: {e}"))?;
            let id = cfg.id.clone();
            let driver = crate::drivers::mqtt::MqttDriver::from_config(cfg);
            Ok((
                id,
                crate::drivers::async_driver::AnyDriver::Async(Box::new(driver)),
            ))
        }
        other => Err(format!("unknown driver_type '{other}'")),
    }
}

/// DELETE /api/drivers/{id} — deregister and close a driver.
async fn delete_driver_async(
    Path(id): Path<String>,
    axum::extract::State(handle): axum::extract::State<crate::drivers::actor::DriverHandle>,
) -> impl IntoResponse {
    match handle.remove(&id).await {
        Ok(true) => Json(serde_json::json!({"ok": true, "id": id})).into_response(),
        Ok(false) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("driver '{id}' not found")})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/drivers/{id}/open — open (initialize) a specific driver.
async fn open_driver_async(
    Path(id): Path<String>,
    axum::extract::State(handle): axum::extract::State<crate::drivers::actor::DriverHandle>,
) -> impl IntoResponse {
    match handle.open_driver(&id).await {
        Ok(meta) => Json(driver_meta_json(&id, meta)).into_response(),
        Err(DriverError::ConfigFault(msg)) if msg.contains("not found") => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": msg})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/drivers/{id}/close — close a driver without removing it.
async fn close_driver_async(
    Path(id): Path<String>,
    axum::extract::State(handle): axum::extract::State<crate::drivers::actor::DriverHandle>,
) -> impl IntoResponse {
    match handle.close_driver(&id).await {
        Ok(()) => Json(serde_json::json!({"ok": true, "id": id})).into_response(),
        Err(DriverError::ConfigFault(msg)) if msg.contains("not found") => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": msg})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/drivers/{id}/ping — health-check a driver.
async fn ping_driver_async(
    Path(id): Path<String>,
    axum::extract::State(handle): axum::extract::State<crate::drivers::actor::DriverHandle>,
) -> impl IntoResponse {
    match handle.ping_driver(&id).await {
        Ok(meta) => Json(driver_meta_json(&id, meta)).into_response(),
        Err(DriverError::ConfigFault(msg)) if msg.contains("not found") => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": msg})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

fn driver_meta_json(id: &str, meta: DriverMeta) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "id": id,
        "firmwareVersion": meta.firmware_version,
        "model": meta.model,
        "extra": meta.extra,
    })
}

/// POST /api/syncCur — batch-read current values across drivers.
///
/// Request body:
/// ```json
/// {
///   "driverPoints": {
///     "bac-1": [{"pointId": 100, "address": "AI:0"}, ...],
///     "mqtt-1": [{"pointId": 200, "address": "sensors/temp"}, ...]
///   }
/// }
/// ```
///
/// Response: `{"results": [{"driverId": "...", "pointId": N, "value": X}
/// | {"driverId": "...", "pointId": N, "error": "..."}, ...]}`.
async fn sync_cur_async(
    axum::extract::State(handle): axum::extract::State<crate::drivers::actor::DriverHandle>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let driver_points_json = match body.get("driverPoints").and_then(|v| v.as_object()) {
        Some(m) => m,
        None => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({"error": "missing 'driverPoints' object"}),
                ),
            )
                .into_response();
        }
    };

    let mut point_map: HashMap<DriverId, Vec<DriverPointRef>> = HashMap::new();
    for (driver_id, pts_val) in driver_points_json {
        let arr = match pts_val.as_array() {
            Some(a) => a,
            None => {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": format!("'{driver_id}' value must be an array"),
                    })),
                )
                    .into_response();
            }
        };
        let mut refs: Vec<DriverPointRef> = Vec::with_capacity(arr.len());
        for item in arr {
            let pid = item.get("pointId").and_then(|v| v.as_u64()).map(|n| n as u32);
            let addr = item.get("address").and_then(|v| v.as_str());
            match (pid, addr) {
                (Some(point_id), Some(a)) => refs.push(DriverPointRef {
                    point_id,
                    address: a.to_string(),
                }),
                _ => {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": "each point entry requires pointId (u32) and address (string)",
                        })),
                    )
                        .into_response();
                }
            }
        }
        point_map.insert(driver_id.clone(), refs);
    }

    match handle.sync_all(point_map).await {
        Ok(results) => {
            let items: Vec<serde_json::Value> = results
                .into_iter()
                .map(|(did, pid, r)| match r {
                    Ok(v) => {
                        serde_json::json!({"driverId": did, "pointId": pid, "value": v})
                    }
                    Err(e) => serde_json::json!({
                        "driverId": did,
                        "pointId": pid,
                        "error": e.to_string(),
                    }),
                })
                .collect();
            Json(serde_json::json!({ "results": items })).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/drivers/{id}/message — dispatch a driver-specific custom
/// message (Phase 12.0E).
///
/// Request body: a JSON `DriverMessage` — `{"id": "...", "payload": {...}}`.
/// Drivers that don't override `on_receive` reply with HTTP 501
/// (`NotSupported`); unknown driver ids return 404.
async fn driver_message_async(
    Path(driver_id): Path<String>,
    axum::extract::State(handle): axum::extract::State<crate::drivers::actor::DriverHandle>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let msg: DriverMessage = match serde_json::from_value(body) {
        Ok(m) => m,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("invalid DriverMessage: {e}")})),
            )
                .into_response();
        }
    };

    match handle.send_message(&driver_id, msg).await {
        Ok(resp) => Json(serde_json::json!({"ok": true, "response": resp})).into_response(),
        Err(DriverError::ConfigFault(m)) if m.contains("not found") => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": m})),
        )
            .into_response(),
        Err(DriverError::NotSupported(feat)) => (
            axum::http::StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({"error": format!("not supported: {feat}")})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── SyncContext / WriteContext tests (Phase 12.0C) ────

    #[test]
    fn sync_context_collects_ok_and_err() {
        let mut ctx = SyncContext::new();
        ctx.update_cur_ok(1, 1.5);
        ctx.update_cur_err(2, DriverError::CommFault("x".into()));
        ctx.update_cur_ok(3, 3.5);

        let results = ctx.into_results();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, 1);
        assert!((results[0].1.as_ref().unwrap() - 1.5).abs() < f64::EPSILON);
        assert_eq!(results[1].0, 2);
        assert!(matches!(results[1].1, Err(DriverError::CommFault(_))));
        assert_eq!(results[2].0, 3);
        assert!(results[2].1.is_ok());
    }

    #[test]
    fn sync_context_with_capacity_starts_empty() {
        let ctx = SyncContext::with_capacity(16);
        assert!(ctx.results().is_empty());
    }

    #[test]
    fn write_context_collects_ok_and_err() {
        let mut ctx = WriteContext::new();
        ctx.update_write_ok(10);
        ctx.update_write_err(20, DriverError::ConfigFault("bad".into()));

        let results = ctx.into_results();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 10);
        assert!(results[0].1.is_ok());
        assert_eq!(results[1].0, 20);
        assert!(matches!(results[1].1, Err(DriverError::ConfigFault(_))));
    }

    #[test]
    fn write_context_with_capacity_starts_empty() {
        let ctx = WriteContext::with_capacity(4);
        assert!(ctx.results().is_empty());
    }

    #[test]
    fn sync_context_preserves_insertion_order() {
        let mut ctx = SyncContext::new();
        for i in (0..5u32).rev() {
            ctx.update_cur_ok(i, f64::from(i));
        }
        let results = ctx.into_results();
        assert_eq!(
            results.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![4, 3, 2, 1, 0]
        );
    }

    // ── DriverStatus tests ─────────────────────────────────

    #[test]
    fn driver_status_display() {
        assert_eq!(DriverStatus::Pending.to_string(), "pending");
        assert_eq!(DriverStatus::Ok.to_string(), "ok");
        assert_eq!(DriverStatus::Stale.to_string(), "stale");
        assert_eq!(
            DriverStatus::Fault("timeout".into()).to_string(),
            "fault: timeout"
        );
        assert_eq!(DriverStatus::Disabled.to_string(), "disabled");
        assert_eq!(DriverStatus::Down.to_string(), "down");
        assert_eq!(DriverStatus::Syncing.to_string(), "syncing");
    }

    #[test]
    fn driver_status_clone_and_eq() {
        let statuses = vec![
            DriverStatus::Pending,
            DriverStatus::Ok,
            DriverStatus::Stale,
            DriverStatus::Fault("x".into()),
            DriverStatus::Disabled,
            DriverStatus::Down,
            DriverStatus::Syncing,
        ];
        for s in &statuses {
            assert_eq!(s, &s.clone());
        }
        assert_ne!(DriverStatus::Ok, DriverStatus::Stale);
        assert_ne!(DriverStatus::Ok, DriverStatus::Syncing);
    }

    #[test]
    fn driver_status_serialize() {
        let json = serde_json::to_string(&DriverStatus::Ok).unwrap();
        assert!(json.contains("Ok"));

        let json = serde_json::to_string(&DriverStatus::Fault("bad".into())).unwrap();
        assert!(json.contains("bad"));

        let json = serde_json::to_string(&DriverStatus::Stale).unwrap();
        assert!(json.contains("Stale"));

        let json = serde_json::to_string(&DriverStatus::Syncing).unwrap();
        assert!(json.contains("Syncing"));
    }

    // ── DriverError tests ─────────────────────────────────

    #[test]
    fn driver_error_display() {
        let e = DriverError::ConfigFault("bad addr".into());
        assert!(e.to_string().contains("config fault"));

        let e = DriverError::CommFault("timeout".into());
        assert!(e.to_string().contains("comm fault"));

        let e = DriverError::NotSupported("learn");
        assert!(e.to_string().contains("not supported"));

        let e = DriverError::RemoteStatus("device offline".into());
        assert!(e.to_string().contains("remote status"));

        let e = DriverError::Timeout("5s elapsed".into());
        assert!(e.to_string().contains("timeout"));

        let e = DriverError::Internal("unexpected state".into());
        assert!(e.to_string().contains("internal error"));

        let e = DriverError::HardwareNotFound("/dev/i2c-2".into());
        assert!(e.to_string().contains("hardware not found"));
    }

    #[test]
    fn driver_error_all_variants_are_errors() {
        let errors: Vec<DriverError> = vec![
            DriverError::ConfigFault("a".into()),
            DriverError::CommFault("b".into()),
            DriverError::NotSupported("c"),
            DriverError::RemoteStatus("d".into()),
            DriverError::Timeout("e".into()),
            DriverError::Internal("f".into()),
            DriverError::HardwareNotFound("g".into()),
        ];
        for e in &errors {
            // All variants implement Display and Error
            let _msg = e.to_string();
            let _err: &dyn std::error::Error = e;
        }
    }

    // ── PointStatus tests ─────────────────────────────────

    #[test]
    fn point_status_inherited_resolves_to_driver() {
        let ps = PointStatus::Inherited;
        assert_eq!(ps.resolve(&DriverStatus::Ok), DriverStatus::Ok);
        assert_eq!(ps.resolve(&DriverStatus::Down), DriverStatus::Down);
        assert_eq!(ps.resolve(&DriverStatus::Stale), DriverStatus::Stale);
        assert_eq!(ps.resolve(&DriverStatus::Syncing), DriverStatus::Syncing);
        assert_eq!(
            ps.resolve(&DriverStatus::Fault("x".into())),
            DriverStatus::Fault("x".into())
        );
    }

    #[test]
    fn point_status_own_overrides_driver() {
        let ps = PointStatus::Own(DriverStatus::Fault("point-specific".into()));
        // Even if driver is Ok, point has its own fault
        assert_eq!(
            ps.resolve(&DriverStatus::Ok),
            DriverStatus::Fault("point-specific".into())
        );
    }

    #[test]
    fn point_status_default_is_inherited() {
        assert_eq!(PointStatus::default(), PointStatus::Inherited);
    }

    #[test]
    fn point_status_serialize() {
        let json = serde_json::to_string(&PointStatus::Inherited).unwrap();
        assert!(json.contains("Inherited"));

        let json = serde_json::to_string(&PointStatus::Own(DriverStatus::Ok)).unwrap();
        assert!(json.contains("Own"));
    }

    // ── PointStatus Remote* variants (Phase 12.0B) ─────────

    #[test]
    fn point_status_remote_disabled_is_terminal() {
        let ps = PointStatus::RemoteDisabled;
        // Terminal — does NOT fall back to driver status even when driver is Ok.
        assert_eq!(ps.resolve(&DriverStatus::Ok), DriverStatus::Disabled);
        assert_eq!(ps.resolve(&DriverStatus::Down), DriverStatus::Disabled);
    }

    #[test]
    fn point_status_remote_down_is_terminal() {
        let ps = PointStatus::RemoteDown;
        assert_eq!(ps.resolve(&DriverStatus::Ok), DriverStatus::Down);
        assert_eq!(ps.resolve(&DriverStatus::Stale), DriverStatus::Down);
    }

    #[test]
    fn point_status_remote_fault_carries_reason() {
        let ps = PointStatus::RemoteFault("write-access-denied".into());
        let resolved = ps.resolve(&DriverStatus::Ok);
        assert_eq!(resolved, DriverStatus::Fault("write-access-denied".into()));
    }

    #[test]
    fn point_status_is_remote_classifier() {
        assert!(!PointStatus::Inherited.is_remote());
        assert!(!PointStatus::Own(DriverStatus::Ok).is_remote());
        assert!(PointStatus::RemoteDisabled.is_remote());
        assert!(PointStatus::RemoteDown.is_remote());
        assert!(PointStatus::RemoteFault("x".into()).is_remote());
    }

    #[test]
    fn point_status_from_driver_error_remote_status() {
        let err = DriverError::RemoteStatus("class=2 code=31".into());
        let ps = PointStatus::from_driver_error(&err);
        assert_eq!(
            ps,
            PointStatus::RemoteFault("class=2 code=31".into())
        );
    }

    #[test]
    fn point_status_from_driver_error_comm_fault_is_remote_down() {
        let err = DriverError::CommFault("socket closed".into());
        let ps = PointStatus::from_driver_error(&err);
        assert_eq!(ps, PointStatus::RemoteDown);
    }

    #[test]
    fn point_status_from_driver_error_timeout_is_remote_down() {
        let err = DriverError::Timeout("5s".into());
        let ps = PointStatus::from_driver_error(&err);
        assert_eq!(ps, PointStatus::RemoteDown);
    }

    #[test]
    fn point_status_from_driver_error_config_fault_is_own() {
        let err = DriverError::ConfigFault("bad address".into());
        let ps = PointStatus::from_driver_error(&err);
        match ps {
            PointStatus::Own(DriverStatus::Fault(msg)) => assert_eq!(msg, "bad address"),
            other => panic!("expected Own(Fault), got {other:?}"),
        }
    }

    #[test]
    fn point_status_from_driver_error_hardware_not_found_is_own() {
        let err = DriverError::HardwareNotFound("/dev/gpiochip9".into());
        let ps = PointStatus::from_driver_error(&err);
        match ps {
            PointStatus::Own(DriverStatus::Fault(msg)) => assert_eq!(msg, "/dev/gpiochip9"),
            other => panic!("expected Own(Fault), got {other:?}"),
        }
    }

    #[test]
    fn point_status_from_driver_error_not_supported_is_own() {
        let err = DriverError::NotSupported("write");
        let ps = PointStatus::from_driver_error(&err);
        match ps {
            PointStatus::Own(DriverStatus::Fault(msg)) => {
                assert!(msg.contains("not supported"));
                assert!(msg.contains("write"));
            }
            other => panic!("expected Own(Fault), got {other:?}"),
        }
    }

    #[test]
    fn point_status_remote_variants_serialize() {
        let json = serde_json::to_string(&PointStatus::RemoteDisabled).unwrap();
        assert!(json.contains("RemoteDisabled"));

        let json = serde_json::to_string(&PointStatus::RemoteDown).unwrap();
        assert!(json.contains("RemoteDown"));

        let json = serde_json::to_string(&PointStatus::RemoteFault("why".into())).unwrap();
        assert!(json.contains("RemoteFault"));
        assert!(json.contains("why"));
    }

    #[test]
    fn point_status_remote_variants_round_trip() {
        let original = PointStatus::RemoteFault("access denied".into());
        let json = serde_json::to_string(&original).unwrap();
        let decoded: PointStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, original);
    }

    // NOTE: status-inheritance + watch-manager + poll-scheduler tests
    // that used the sync `DriverManager` were removed when the manager
    // itself was deleted (Phase 12 cleanup, 2026-04-18). The actor-based
    // equivalents (`DriverHandle::{register_point, set_point_status,
    // effective_point_status, driver_status, add_watch, remove_watch,
    // add_poll_bucket, status}`) have their own test coverage in
    // `drivers::actor::tests`. PointStatus / SyncContext / WriteContext
    // unit tests above remain — they test pure types.
}
