//! Driver Framework v2 — core traits, polling, watch subscriptions, and status inheritance.
//!
//! Provides a unified abstraction for hardware and protocol drivers. Each driver
//! implements the [`Driver`] trait and is managed by the [`DriverManager`].
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

pub use actor::{spawn_driver_actor, DriverCmd, DriverHandle};
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

/// Point-level status that can inherit from its parent driver.
///
/// By default, points inherit their driver's status. A point can
/// override this with its own status (e.g., when a remote device
/// reports a point-specific fault).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum PointStatus {
    /// Point has its own explicit status.
    Own(DriverStatus),
    /// Point inherits status from its parent driver (default).
    #[default]
    Inherited,
}

impl PointStatus {
    /// Resolve the effective status given the parent driver status.
    ///
    /// If the point has its own status, that takes precedence.
    /// Otherwise, the driver status is used.
    pub fn resolve(&self, driver_status: &DriverStatus) -> DriverStatus {
        match self {
            PointStatus::Own(s) => s.clone(),
            PointStatus::Inherited => driver_status.clone(),
        }
    }
}

// ── Driver Trait ────────────────────────────────────────────

/// Driver lifecycle and I/O callbacks.
///
/// Each driver instance manages a set of points (channels) and provides
/// read/write access. The [`DriverManager`] orchestrates lifecycle and
/// batch operations across all registered drivers.
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
    /// Returns `(point_id, result)` pairs. Failed reads return `DriverError`.
    fn sync_cur(&mut self, points: &[DriverPointRef]) -> Vec<(u32, Result<f64, DriverError>)>;

    /// Write values to points.
    ///
    /// Returns `(point_id, result)` pairs. Failed writes return `DriverError`.
    fn write(&mut self, writes: &[(u32, f64)]) -> Vec<(u32, Result<(), DriverError>)>;

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
}

// ── DriverManager ───────────────────────────────────────────

/// Manages all driver instances — lifecycle, polling, watch subscriptions, and dispatch.
///
/// Integrates [`PollScheduler`] for bucket-based polling and
/// [`DriverWatchManager`] for COV subscription tracking.
pub struct DriverManager {
    drivers: HashMap<DriverId, Box<dyn Driver>>,
    /// Polling bucket scheduler.
    poll_scheduler: PollScheduler,
    /// COV subscription manager.
    watch_manager: DriverWatchManager,
    /// Per-point status overrides (point_id -> PointStatus).
    point_statuses: HashMap<u32, PointStatus>,
    /// Point-to-driver mapping for status inheritance.
    point_driver_map: HashMap<u32, DriverId>,
}

impl DriverManager {
    /// Create an empty driver manager.
    pub fn new() -> Self {
        Self {
            drivers: HashMap::new(),
            poll_scheduler: PollScheduler::new(),
            watch_manager: DriverWatchManager::new(),
            point_statuses: HashMap::new(),
            point_driver_map: HashMap::new(),
        }
    }

    /// Register a driver instance.
    ///
    /// Returns `Err` if a driver with the same ID is already registered.
    pub fn register(&mut self, driver: Box<dyn Driver>) -> Result<(), DriverError> {
        let id = driver.id().to_string();
        if self.drivers.contains_key(&id) {
            return Err(DriverError::ConfigFault(format!(
                "driver '{id}' already registered"
            )));
        }
        self.drivers.insert(id, driver);
        Ok(())
    }

    /// Remove a driver by ID. Calls `close()` before removing.
    ///
    /// Also removes all poll buckets and watch subscriptions for this driver.
    /// Returns `true` if the driver was found and removed.
    pub fn remove(&mut self, id: &str) -> bool {
        if let Some(mut driver) = self.drivers.remove(id) {
            driver.close();
            self.poll_scheduler.remove_driver(id);
            // Remove point-driver mappings for this driver
            self.point_driver_map.retain(|_, did| did != id);
            true
        } else {
            false
        }
    }

    /// Initialize all registered drivers.
    ///
    /// Returns a list of `(driver_id, result)` pairs.
    pub fn open_all(&mut self) -> Vec<(DriverId, Result<DriverMeta, DriverError>)> {
        let ids: Vec<DriverId> = self.drivers.keys().cloned().collect();
        let mut results = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(driver) = self.drivers.get_mut(&id) {
                let result = driver.open();
                results.push((id, result));
            }
        }
        results
    }

    /// Shut down all registered drivers.
    pub fn close_all(&mut self) {
        for driver in self.drivers.values_mut() {
            driver.close();
        }
    }

    /// Poll all drivers for current values.
    ///
    /// Each driver is asked to sync all its registered points. Returns
    /// `(driver_id, point_id, result)` triples.
    pub fn sync_all(
        &mut self,
        point_map: &HashMap<DriverId, Vec<DriverPointRef>>,
    ) -> Vec<(DriverId, u32, Result<f64, DriverError>)> {
        let mut results = Vec::new();
        for (driver_id, points) in point_map {
            if let Some(driver) = self.drivers.get_mut(driver_id) {
                for (point_id, result) in driver.sync_cur(points) {
                    results.push((driver_id.clone(), point_id, result));
                }
            }
        }
        results
    }

    /// Discover points from a specific driver.
    pub fn learn(&mut self, driver_id: &str, path: Option<&str>) -> Result<LearnGrid, DriverError> {
        self.drivers
            .get_mut(driver_id)
            .ok_or_else(|| DriverError::ConfigFault(format!("driver '{driver_id}' not found")))?
            .learn(path)
    }

    /// Write to points via a specific driver.
    pub fn write(&mut self, driver_id: &str, writes: &[(u32, f64)]) -> WritePointsResult {
        let driver = self
            .drivers
            .get_mut(driver_id)
            .ok_or_else(|| DriverError::ConfigFault(format!("driver '{driver_id}' not found")))?;
        Ok(driver.write(writes))
    }

    /// List all registered driver IDs.
    pub fn driver_ids(&self) -> Vec<&str> {
        self.drivers.keys().map(|s| s.as_str()).collect()
    }

    /// Get the status of a specific driver.
    pub fn driver_status(&self, id: &str) -> Option<&DriverStatus> {
        self.drivers.get(id).map(|d| d.status())
    }

    /// Get a summary of all drivers (for REST API).
    ///
    /// Includes poll scheduler and watch manager statistics.
    pub fn driver_summaries(&self) -> Vec<DriverSummary> {
        self.drivers
            .values()
            .map(|d| {
                let id = d.id().to_string();
                let poll_buckets = self.poll_scheduler.buckets_for_driver(&id).len();
                let poll_points: usize = self
                    .poll_scheduler
                    .buckets_for_driver(&id)
                    .iter()
                    .filter_map(|&idx| self.poll_scheduler.bucket(idx))
                    .map(|b| b.points.len())
                    .sum();
                DriverSummary {
                    id,
                    driver_type: d.driver_type().to_string(),
                    status: d.status().clone(),
                    poll_mode: format!("{:?}", d.poll_mode()),
                    poll_buckets,
                    poll_points,
                }
            })
            .collect()
    }

    // ── Poll Scheduler integration ─────────────────────────

    /// Get a reference to the poll scheduler.
    pub fn poll_scheduler(&self) -> &PollScheduler {
        &self.poll_scheduler
    }

    /// Get a mutable reference to the poll scheduler.
    pub fn poll_scheduler_mut(&mut self) -> &mut PollScheduler {
        &mut self.poll_scheduler
    }

    // ── Watch Manager integration ──────────────────────────

    /// Add a watch subscription for a subscriber on the given points.
    ///
    /// Calls `on_watch` on the appropriate drivers for any newly-watched points.
    pub fn add_watch(&mut self, subscriber: &str, point_ids: &[u32]) {
        // Track which points are newly watched (transition from unwatched)
        let newly_watched: Vec<u32> = point_ids
            .iter()
            .filter(|&&pid| !self.watch_manager.is_watched(pid))
            .copied()
            .collect();

        self.watch_manager.subscribe(subscriber, point_ids);

        // Notify drivers about newly-watched points (grouped by driver)
        if !newly_watched.is_empty() {
            let mut by_driver: HashMap<DriverId, Vec<DriverPointRef>> = HashMap::new();
            for pid in &newly_watched {
                if let Some(driver_id) = self.point_driver_map.get(pid) {
                    by_driver
                        .entry(driver_id.clone())
                        .or_default()
                        .push(DriverPointRef {
                            point_id: *pid,
                            address: String::new(),
                        });
                }
            }
            for (driver_id, refs) in &by_driver {
                if let Some(driver) = self.drivers.get_mut(driver_id) {
                    let _ = driver.on_watch(refs);
                }
            }
        }
    }

    /// Remove a watch subscription.
    ///
    /// Calls `on_unwatch` on drivers for points that are no longer watched by anyone.
    pub fn remove_watch(&mut self, subscriber: &str, point_ids: &[u32]) {
        // Identify points that will become completely unwatched
        let will_unwatch: Vec<u32> = point_ids
            .iter()
            .filter(|&&pid| {
                let subs = self.watch_manager.subscribers_for(pid);
                // This point will be unwatched if the only subscriber is the one being removed
                subs.len() == 1 && subs.contains(subscriber)
            })
            .copied()
            .collect();

        self.watch_manager.unsubscribe(subscriber, point_ids);

        // Notify drivers about newly-unwatched points
        if !will_unwatch.is_empty() {
            let mut by_driver: HashMap<DriverId, Vec<DriverPointRef>> = HashMap::new();
            for pid in &will_unwatch {
                if let Some(driver_id) = self.point_driver_map.get(pid) {
                    by_driver
                        .entry(driver_id.clone())
                        .or_default()
                        .push(DriverPointRef {
                            point_id: *pid,
                            address: String::new(),
                        });
                }
            }
            for (driver_id, refs) in &by_driver {
                if let Some(driver) = self.drivers.get_mut(driver_id) {
                    let _ = driver.on_unwatch(refs);
                }
            }
        }
    }

    /// Get a reference to the watch manager.
    pub fn watch_manager(&self) -> &DriverWatchManager {
        &self.watch_manager
    }

    // ── Point Status Inheritance ───────────────────────────

    /// Register a point as belonging to a driver (for status inheritance).
    pub fn register_point(&mut self, point_id: u32, driver_id: &str) {
        self.point_driver_map
            .insert(point_id, driver_id.to_string());
    }

    /// Set a point-specific status override.
    pub fn set_point_status(&mut self, point_id: u32, status: PointStatus) {
        self.point_statuses.insert(point_id, status);
    }

    /// Get the effective status of a point, considering inheritance.
    ///
    /// Returns `None` if the point is not registered with any driver.
    pub fn effective_point_status(&self, point_id: u32) -> Option<DriverStatus> {
        let driver_id = self.point_driver_map.get(&point_id)?;
        let driver_status = self.drivers.get(driver_id)?.status();
        let point_status = self
            .point_statuses
            .get(&point_id)
            .cloned()
            .unwrap_or_default();
        Some(point_status.resolve(driver_status))
    }

    /// Get aggregated status for a driver and its inherited point statuses.
    ///
    /// Returns (driver_status, vec of (point_id, effective_status)) or None if driver not found.
    pub fn driver_with_points_status(
        &self,
        driver_id: &str,
    ) -> Option<(DriverStatus, Vec<(u32, DriverStatus)>)> {
        let driver = self.drivers.get(driver_id)?;
        let driver_status = driver.status().clone();

        let point_statuses: Vec<(u32, DriverStatus)> = self
            .point_driver_map
            .iter()
            .filter(|(_, did)| did.as_str() == driver_id)
            .map(|(&pid, _)| {
                let ps = self.point_statuses.get(&pid).cloned().unwrap_or_default();
                (pid, ps.resolve(&driver_status))
            })
            .collect();

        Some((driver_status, point_statuses))
    }
}

impl Default for DriverManager {
    fn default() -> Self {
        Self::new()
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

// ── LocalIoDriver ───────────────────────────────────────────

/// A local I/O driver that wraps the existing HAL for GPIO/ADC/I2C/PWM.
///
/// This driver serves as the structural foundation for the driver framework.
/// It registers local hardware points and responds to learn/sync requests.
/// Actual hardware reads continue through the engine's existing HAL and
/// poll infrastructure — this driver provides the Driver trait interface
/// for management, discovery, and status tracking.
pub struct LocalIoDriver {
    id: DriverId,
    status: DriverStatus,
    points: Vec<LocalIoPoint>,
}

/// A local I/O point (channel mapped to a hardware address).
#[derive(Debug, Clone)]
struct LocalIoPoint {
    point_id: u32,
    address: String, // "AIN0", "GPIO60", "I2C2:0x25"
    kind: String,    // "Number", "Bool"
}

impl LocalIoDriver {
    /// Create a new local I/O driver with the given instance ID.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: DriverStatus::Pending,
            points: Vec::new(),
        }
    }

    /// Add a point to this driver.
    pub fn add_point(
        &mut self,
        point_id: u32,
        address: impl Into<String>,
        kind: impl Into<String>,
    ) {
        self.points.push(LocalIoPoint {
            point_id,
            address: address.into(),
            kind: kind.into(),
        });
    }

    /// Get the number of registered points.
    pub fn point_count(&self) -> usize {
        self.points.len()
    }
}

impl Driver for LocalIoDriver {
    fn driver_type(&self) -> &'static str {
        "localIo"
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn status(&self) -> &DriverStatus {
        &self.status
    }

    fn open(&mut self) -> Result<DriverMeta, DriverError> {
        self.status = DriverStatus::Ok;
        Ok(DriverMeta {
            model: Some("LocalIo".into()),
            ..Default::default()
        })
    }

    fn close(&mut self) {
        self.status = DriverStatus::Down;
    }

    fn ping(&mut self) -> Result<DriverMeta, DriverError> {
        // Local I/O is always healthy if open succeeded.
        Ok(DriverMeta::default())
    }

    fn learn(&mut self, _path: Option<&str>) -> Result<LearnGrid, DriverError> {
        let grid = self
            .points
            .iter()
            .map(|p| LearnPoint {
                name: format!("point_{}", p.point_id),
                address: p.address.clone(),
                kind: p.kind.clone(),
                unit: None,
                tags: HashMap::new(),
            })
            .collect();
        Ok(grid)
    }

    fn sync_cur(&mut self, points: &[DriverPointRef]) -> Vec<(u32, Result<f64, DriverError>)> {
        // The actual hardware reads are done by the engine's HAL.
        // This returns 0.0 as a placeholder — the engine poll loop
        // is the real source of truth for local I/O values.
        points.iter().map(|p| (p.point_id, Ok(0.0))).collect()
    }

    fn write(&mut self, writes: &[(u32, f64)]) -> Vec<(u32, Result<(), DriverError>)> {
        // Writes for local I/O go through the engine's existing write path.
        // This acknowledges the write structurally.
        writes.iter().map(|(id, _val)| (*id, Ok(()))).collect()
    }
}

// ── REST API handlers ───────────────────────────────────────

/// Build an Axum router for driver REST endpoints backed by the async [`DriverHandle`] actor.
///
/// Mount under the main router with `.merge(drivers::driver_router(handle))`.
pub fn driver_router(handle: crate::drivers::actor::DriverHandle) -> axum::Router {
    axum::Router::new()
        .route("/api/drivers", axum::routing::get(list_drivers_async))
        .route(
            "/api/drivers/{id}/status",
            axum::routing::get(driver_status_async),
        )
        .route(
            "/api/drivers/{id}/learn",
            axum::routing::get(driver_learn_async),
        )
        .route(
            "/api/drivers/{id}/write",
            axum::routing::post(driver_write_async),
        )
        .with_state(handle)
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

// ── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── DriverManager tests ────────────────────────────────

    #[test]
    fn manager_register_and_list() {
        let mut mgr = DriverManager::new();
        let driver = LocalIoDriver::new("local-1");
        mgr.register(Box::new(driver)).unwrap();

        let ids = mgr.driver_ids();
        assert_eq!(ids.len(), 1);
        assert!(ids.contains(&"local-1"));
    }

    #[test]
    fn manager_reject_duplicate_id() {
        let mut mgr = DriverManager::new();
        mgr.register(Box::new(LocalIoDriver::new("dup"))).unwrap();
        let result = mgr.register(Box::new(LocalIoDriver::new("dup")));
        assert!(result.is_err());
    }

    #[test]
    fn manager_open_all() {
        let mut mgr = DriverManager::new();
        mgr.register(Box::new(LocalIoDriver::new("a"))).unwrap();
        mgr.register(Box::new(LocalIoDriver::new("b"))).unwrap();

        let results = mgr.open_all();
        assert_eq!(results.len(), 2);
        for (_id, result) in &results {
            assert!(result.is_ok());
        }

        // After open, status should be Ok.
        assert_eq!(mgr.driver_status("a"), Some(&DriverStatus::Ok));
        assert_eq!(mgr.driver_status("b"), Some(&DriverStatus::Ok));
    }

    #[test]
    fn manager_close_all() {
        let mut mgr = DriverManager::new();
        mgr.register(Box::new(LocalIoDriver::new("x"))).unwrap();
        mgr.open_all();
        mgr.close_all();

        assert_eq!(mgr.driver_status("x"), Some(&DriverStatus::Down));
    }

    #[test]
    fn manager_remove() {
        let mut mgr = DriverManager::new();
        mgr.register(Box::new(LocalIoDriver::new("rm"))).unwrap();
        mgr.open_all();

        assert!(mgr.remove("rm"));
        assert!(!mgr.remove("rm")); // already removed
        assert_eq!(mgr.driver_ids().len(), 0);
    }

    #[test]
    fn manager_sync_all() {
        let mut mgr = DriverManager::new();
        let mut driver = LocalIoDriver::new("io");
        driver.add_point(100, "AIN0", "Number");
        driver.add_point(200, "AIN1", "Number");
        mgr.register(Box::new(driver)).unwrap();
        mgr.open_all();

        let mut point_map = HashMap::new();
        point_map.insert(
            "io".to_string(),
            vec![
                DriverPointRef {
                    point_id: 100,
                    address: "AIN0".into(),
                },
                DriverPointRef {
                    point_id: 200,
                    address: "AIN1".into(),
                },
            ],
        );

        let results = mgr.sync_all(&point_map);
        assert_eq!(results.len(), 2);
        for (_driver_id, _point_id, result) in &results {
            assert!(result.is_ok());
        }
    }

    #[test]
    fn manager_write() {
        let mut mgr = DriverManager::new();
        mgr.register(Box::new(LocalIoDriver::new("w"))).unwrap();
        mgr.open_all();

        let results = mgr.write("w", &[(100, 72.5), (200, 1.0)]).unwrap();
        assert_eq!(results.len(), 2);
        for (_id, result) in &results {
            assert!(result.is_ok());
        }
    }

    #[test]
    fn manager_write_unknown_driver() {
        let mut mgr = DriverManager::new();
        let result = mgr.write("nonexistent", &[(1, 0.0)]);
        assert!(result.is_err());
    }

    #[test]
    fn manager_learn_unknown_driver() {
        let mut mgr = DriverManager::new();
        let result = mgr.learn("nonexistent", None);
        assert!(result.is_err());
    }

    #[test]
    fn manager_driver_summaries() {
        let mut mgr = DriverManager::new();
        mgr.register(Box::new(LocalIoDriver::new("s1"))).unwrap();
        mgr.open_all();

        let summaries = mgr.driver_summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "s1");
        assert_eq!(summaries[0].driver_type, "localIo");
        assert_eq!(summaries[0].status, DriverStatus::Ok);
    }

    // ── LocalIoDriver tests ────────────────────────────────

    #[test]
    fn local_io_lifecycle() {
        let mut driver = LocalIoDriver::new("test-io");
        assert_eq!(*driver.status(), DriverStatus::Pending);

        let meta = driver.open().unwrap();
        assert_eq!(meta.model, Some("LocalIo".into()));
        assert_eq!(*driver.status(), DriverStatus::Ok);

        driver.close();
        assert_eq!(*driver.status(), DriverStatus::Down);
    }

    #[test]
    fn local_io_driver_type() {
        let driver = LocalIoDriver::new("t");
        assert_eq!(driver.driver_type(), "localIo");
    }

    #[test]
    fn local_io_learn_returns_points() {
        let mut driver = LocalIoDriver::new("learn-test");
        driver.add_point(1100, "AIN0", "Number");
        driver.add_point(2100, "AIN1", "Number");
        driver.add_point(5000, "GPIO60", "Bool");

        let grid = driver.learn(None).unwrap();
        assert_eq!(grid.len(), 3);
        assert_eq!(grid[0].address, "AIN0");
        assert_eq!(grid[0].kind, "Number");
        assert_eq!(grid[2].address, "GPIO60");
        assert_eq!(grid[2].kind, "Bool");
    }

    #[test]
    fn local_io_sync_cur() {
        let mut driver = LocalIoDriver::new("sync-test");
        driver.add_point(100, "AIN0", "Number");
        driver.open().unwrap();

        let refs = vec![DriverPointRef {
            point_id: 100,
            address: "AIN0".into(),
        }];
        let results = driver.sync_cur(&refs);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 100);
        assert!(results[0].1.is_ok());
    }

    #[test]
    fn local_io_write() {
        let mut driver = LocalIoDriver::new("write-test");
        driver.open().unwrap();

        let results = driver.write(&[(100, 72.0)]);
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_ok());
    }

    #[test]
    fn local_io_ping() {
        let mut driver = LocalIoDriver::new("ping-test");
        driver.open().unwrap();
        let meta = driver.ping().unwrap();
        assert!(meta.firmware_version.is_none()); // local I/O has no firmware
    }

    #[test]
    fn local_io_poll_mode() {
        let driver = LocalIoDriver::new("pm");
        assert_eq!(driver.poll_mode(), PollMode::Buckets);
    }

    #[test]
    fn local_io_point_count() {
        let mut driver = LocalIoDriver::new("pc");
        assert_eq!(driver.point_count(), 0);
        driver.add_point(1, "A", "Number");
        driver.add_point(2, "B", "Bool");
        assert_eq!(driver.point_count(), 2);
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

    // ── Status inheritance via DriverManager ───────────────

    #[test]
    fn manager_point_status_inheritance() {
        let mut mgr = DriverManager::new();
        mgr.register(Box::new(LocalIoDriver::new("io"))).unwrap();
        mgr.open_all();

        // Register points with the driver
        mgr.register_point(100, "io");
        mgr.register_point(200, "io");

        // By default, points inherit driver status (Ok)
        assert_eq!(mgr.effective_point_status(100), Some(DriverStatus::Ok));

        // Override one point with its own status
        mgr.set_point_status(200, PointStatus::Own(DriverStatus::Stale));
        assert_eq!(mgr.effective_point_status(200), Some(DriverStatus::Stale));

        // Point 100 still inherits
        assert_eq!(mgr.effective_point_status(100), Some(DriverStatus::Ok));
    }

    #[test]
    fn manager_point_status_unknown_point() {
        let mgr = DriverManager::new();
        assert_eq!(mgr.effective_point_status(999), None);
    }

    #[test]
    fn manager_driver_with_points_status() {
        let mut mgr = DriverManager::new();
        mgr.register(Box::new(LocalIoDriver::new("io"))).unwrap();
        mgr.open_all();

        mgr.register_point(100, "io");
        mgr.register_point(200, "io");
        mgr.set_point_status(200, PointStatus::Own(DriverStatus::Fault("bad".into())));

        let (ds, pts) = mgr.driver_with_points_status("io").unwrap();
        assert_eq!(ds, DriverStatus::Ok);
        assert_eq!(pts.len(), 2);

        // Find point 100 (inherited Ok) and 200 (own Fault)
        let p100 = pts.iter().find(|(pid, _)| *pid == 100).unwrap();
        assert_eq!(p100.1, DriverStatus::Ok);
        let p200 = pts.iter().find(|(pid, _)| *pid == 200).unwrap();
        assert_eq!(p200.1, DriverStatus::Fault("bad".into()));
    }

    #[test]
    fn manager_driver_with_points_status_unknown() {
        let mgr = DriverManager::new();
        assert!(mgr.driver_with_points_status("nonexistent").is_none());
    }

    // ── Watch Manager integration ─────────────────────────

    #[test]
    fn manager_add_and_remove_watch() {
        let mut mgr = DriverManager::new();
        mgr.register(Box::new(LocalIoDriver::new("io"))).unwrap();
        mgr.open_all();
        mgr.register_point(100, "io");

        mgr.add_watch("client-1", &[100]);
        assert!(mgr.watch_manager().is_watched(100));
        assert_eq!(mgr.watch_manager().watch_count(), 1);

        mgr.remove_watch("client-1", &[100]);
        assert!(!mgr.watch_manager().is_watched(100));
        assert_eq!(mgr.watch_manager().watch_count(), 0);
    }

    #[test]
    fn manager_poll_scheduler_access() {
        let mut mgr = DriverManager::new();
        assert_eq!(mgr.poll_scheduler().bucket_count(), 0);

        mgr.poll_scheduler_mut().add_bucket(
            "drv-1",
            std::time::Duration::from_secs(10),
            vec![DriverPointRef {
                point_id: 1,
                address: "A".into(),
            }],
        );
        assert_eq!(mgr.poll_scheduler().bucket_count(), 1);
        assert_eq!(mgr.poll_scheduler().total_points(), 1);
    }

    #[test]
    fn manager_remove_cleans_poll_buckets() {
        let mut mgr = DriverManager::new();
        mgr.register(Box::new(LocalIoDriver::new("io"))).unwrap();
        mgr.open_all();

        mgr.poll_scheduler_mut().add_bucket(
            "io",
            std::time::Duration::from_secs(5),
            vec![DriverPointRef {
                point_id: 1,
                address: "A".into(),
            }],
        );
        assert_eq!(mgr.poll_scheduler().bucket_count(), 1);

        mgr.remove("io");
        assert_eq!(mgr.poll_scheduler().bucket_count(), 0);
    }

    #[test]
    fn manager_summaries_include_poll_stats() {
        let mut mgr = DriverManager::new();
        mgr.register(Box::new(LocalIoDriver::new("io"))).unwrap();
        mgr.open_all();

        mgr.poll_scheduler_mut().add_bucket(
            "io",
            std::time::Duration::from_secs(5),
            vec![
                DriverPointRef {
                    point_id: 1,
                    address: "A".into(),
                },
                DriverPointRef {
                    point_id: 2,
                    address: "B".into(),
                },
            ],
        );

        let summaries = mgr.driver_summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].poll_buckets, 1);
        assert_eq!(summaries[0].poll_points, 2);
    }

    // ── Default trait ──────────────────────────────────────

    #[test]
    fn driver_manager_default() {
        let mgr = DriverManager::default();
        assert_eq!(mgr.driver_ids().len(), 0);
    }

    // ── on_watch / on_unwatch default impls ───────────────

    #[test]
    fn driver_on_watch_default_is_noop() {
        let mut driver = LocalIoDriver::new("w");
        let refs = vec![DriverPointRef {
            point_id: 1,
            address: "A".into(),
        }];
        assert!(driver.on_watch(&refs).is_ok());
        assert!(driver.on_unwatch(&refs).is_ok());
    }
}
