//! Driver Framework v2 — core traits and LocalIoDriver.
//!
//! Provides a unified abstraction for hardware and protocol drivers. Each driver
//! implements the [`Driver`] trait and is managed by the [`DriverManager`].
//!
//! This is the foundation layer. Modbus, BACnet, and MQTT drivers will be added
//! as separate implementations of the `Driver` trait in future phases.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

// ── Core Types ──────────────────────────────────────────────

/// Unique driver instance identifier.
pub type DriverId = String;

/// Driver operational status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DriverStatus {
    /// Driver not yet initialized.
    Pending,
    /// Driver running normally.
    Ok,
    /// Communication/hardware fault.
    Fault(String),
    /// Driver disabled by configuration.
    Disabled,
    /// Driver shut down.
    Down,
}

impl fmt::Display for DriverStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Ok => write!(f, "ok"),
            Self::Fault(msg) => write!(f, "fault: {msg}"),
            Self::Disabled => write!(f, "disabled"),
            Self::Down => write!(f, "down"),
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

/// Driver error types.
#[derive(Debug, Clone)]
pub enum DriverError {
    /// Configuration error (bad address, missing parameter).
    ConfigFault(String),
    /// Communication error (timeout, connection refused).
    CommFault(String),
    /// Feature not supported by this driver.
    NotSupported(&'static str),
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConfigFault(msg) => write!(f, "config fault: {msg}"),
            Self::CommFault(msg) => write!(f, "comm fault: {msg}"),
            Self::NotSupported(feat) => write!(f, "not supported: {feat}"),
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
}

// ── DriverManager ───────────────────────────────────────────

/// Manages all driver instances — lifecycle, polling, and dispatch.
pub struct DriverManager {
    drivers: HashMap<DriverId, Box<dyn Driver>>,
}

impl DriverManager {
    /// Create an empty driver manager.
    pub fn new() -> Self {
        Self {
            drivers: HashMap::new(),
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
    /// Returns `true` if the driver was found and removed.
    pub fn remove(&mut self, id: &str) -> bool {
        if let Some(mut driver) = self.drivers.remove(id) {
            driver.close();
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
    pub fn learn(
        &mut self,
        driver_id: &str,
        path: Option<&str>,
    ) -> Result<LearnGrid, DriverError> {
        self.drivers
            .get_mut(driver_id)
            .ok_or_else(|| {
                DriverError::ConfigFault(format!("driver '{driver_id}' not found"))
            })?
            .learn(path)
    }

    /// Write to points via a specific driver.
    pub fn write(
        &mut self,
        driver_id: &str,
        writes: &[(u32, f64)],
    ) -> Result<Vec<(u32, Result<(), DriverError>)>, DriverError> {
        let driver = self.drivers.get_mut(driver_id).ok_or_else(|| {
            DriverError::ConfigFault(format!("driver '{driver_id}' not found"))
        })?;
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
    pub fn driver_summaries(&self) -> Vec<DriverSummary> {
        self.drivers
            .values()
            .map(|d| DriverSummary {
                id: d.id().to_string(),
                driver_type: d.driver_type().to_string(),
                status: d.status().clone(),
                poll_mode: format!("{:?}", d.poll_mode()),
            })
            .collect()
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
    pub fn add_point(&mut self, point_id: u32, address: impl Into<String>, kind: impl Into<String>) {
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
        points
            .iter()
            .map(|p| (p.point_id, Ok(0.0)))
            .collect()
    }

    fn write(&mut self, writes: &[(u32, f64)]) -> Vec<(u32, Result<(), DriverError>)> {
        // Writes for local I/O go through the engine's existing write path.
        // This acknowledges the write structurally.
        writes.iter().map(|(id, _val)| (*id, Ok(()))).collect()
    }
}

// ── REST API handlers ───────────────────────────────────────

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use std::sync::{Arc, Mutex};

/// Shared driver manager state for REST endpoints.
pub type SharedDriverManager = Arc<Mutex<DriverManager>>;

/// GET /api/drivers — list all registered drivers with status.
pub async fn list_drivers(
    State(mgr): State<SharedDriverManager>,
) -> impl IntoResponse {
    let mgr = mgr.lock().expect("driver manager lock poisoned");
    Json(mgr.driver_summaries())
}

/// GET /api/drivers/{id}/status — get driver status.
pub async fn driver_status(
    Path(id): Path<String>,
    State(mgr): State<SharedDriverManager>,
) -> impl IntoResponse {
    let mgr = mgr.lock().expect("driver manager lock poisoned");
    match mgr.driver_status(&id) {
        Some(status) => Json(serde_json::json!({
            "id": id,
            "status": status,
        }))
        .into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("driver '{id}' not found") })),
        )
            .into_response(),
    }
}

/// GET /api/drivers/{id}/learn — discover points from a driver.
pub async fn driver_learn(
    Path(id): Path<String>,
    State(mgr): State<SharedDriverManager>,
) -> impl IntoResponse {
    let mut mgr = mgr.lock().expect("driver manager lock poisoned");
    match mgr.learn(&id, None) {
        Ok(grid) => Json(serde_json::json!({
            "driverId": id,
            "points": grid,
        }))
        .into_response(),
        Err(DriverError::NotSupported(_)) => (
            axum::http::StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({ "error": "learn not supported by this driver" })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Build an Axum router for driver REST endpoints.
///
/// Mount under the main router with `.merge(drivers::driver_router(mgr))`.
pub fn driver_router(mgr: SharedDriverManager) -> axum::Router {
    axum::Router::new()
        .route("/api/drivers", axum::routing::get(list_drivers))
        .route("/api/drivers/{id}/status", axum::routing::get(driver_status))
        .route("/api/drivers/{id}/learn", axum::routing::get(driver_learn))
        .with_state(mgr)
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
        assert_eq!(
            DriverStatus::Fault("timeout".into()).to_string(),
            "fault: timeout"
        );
        assert_eq!(DriverStatus::Disabled.to_string(), "disabled");
        assert_eq!(DriverStatus::Down.to_string(), "down");
    }

    #[test]
    fn driver_status_serialize() {
        let json = serde_json::to_string(&DriverStatus::Ok).unwrap();
        assert!(json.contains("Ok"));

        let json = serde_json::to_string(&DriverStatus::Fault("bad".into())).unwrap();
        assert!(json.contains("bad"));
    }

    #[test]
    fn driver_error_display() {
        let e = DriverError::ConfigFault("bad addr".into());
        assert!(e.to_string().contains("config fault"));

        let e = DriverError::CommFault("timeout".into());
        assert!(e.to_string().contains("comm fault"));

        let e = DriverError::NotSupported("learn");
        assert!(e.to_string().contains("not supported"));
    }

    // ── Default trait ──────────────────────────────────────

    #[test]
    fn driver_manager_default() {
        let mgr = DriverManager::default();
        assert_eq!(mgr.driver_ids().len(), 0);
    }
}
