//! Tokio actor-based driver manager.
//!
//! Provides [`DriverManagerActor`] — a message-driven actor that manages all
//! driver instances via an mpsc command channel. External code interacts with it
//! through the [`DriverHandle`], which sends commands and awaits responses via
//! oneshot channels.
//!
//! ## Architecture
//!
//! ```text
//!  REST handler ──┐
//!  Poll task ─────┤── DriverHandle::tx ──► mpsc ──► DriverManagerActor loop
//!  SOX handler ───┘                                    ├── AnyDriver instances
//!                                                      ├── PollScheduler
//!                                                      └── DriverWatchManager
//! ```
//!
//! The actor owns all mutable driver state, eliminating the need for
//! `Arc<Mutex<DriverManager>>`. The [`DriverHandle`] is cheaply cloneable
//! and can be shared across tasks.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::debug;

use super::async_driver::AnyDriver;
use super::poll_scheduler::PollScheduler;
use super::watch_manager::DriverWatchManager;
use super::{
    CovEvent, DriverError, DriverId, DriverMeta, DriverPointRef, DriverStatus, DriverSummary,
    LearnGrid, PointStatus,
};

/// Default capacity for the broadcast CovEvent channel.
///
/// If subscribers fall behind by more than this many events, they'll
/// receive `broadcast::error::RecvError::Lagged` and can resync. 512 is
/// large enough for the expected per-tick point count on a device with
/// hundreds of channels, while bounded enough to avoid unbounded memory
/// on a dropped WebSocket.
pub const DEFAULT_COV_CAPACITY: usize = 512;

// ── Type Aliases (clippy::type_complexity) ───────────────
/// Result of syncing all driver points: (driver_id, point_id, read_result).
type SyncAllResult = Vec<(DriverId, u32, Result<f64, DriverError>)>;
/// Result of writing to driver points: per-point write results.
type WriteResult = Result<Vec<(u32, Result<(), DriverError>)>, DriverError>;
/// Detailed driver status with per-point statuses.
type DetailedDriverStatus = Option<(DriverStatus, Vec<(u32, DriverStatus)>)>;

// ── Commands ──────────────────────────────────────────────

/// Commands accepted by the [`DriverManagerActor`].
///
/// Each variant carries its payload plus a oneshot sender for the response.
pub enum DriverCmd {
    /// Register a new driver instance.
    Register {
        driver: AnyDriver,
        reply: oneshot::Sender<Result<(), DriverError>>,
    },
    /// Remove a driver by ID (calls close first).
    Remove {
        id: String,
        reply: oneshot::Sender<bool>,
    },
    /// Open (initialize) all registered drivers.
    OpenAll {
        reply: oneshot::Sender<Vec<(DriverId, Result<DriverMeta, DriverError>)>>,
    },
    /// Close (shut down) all registered drivers.
    CloseAll { reply: oneshot::Sender<()> },
    /// Open (initialize) a single driver by id.
    OpenDriver {
        id: String,
        reply: oneshot::Sender<Result<DriverMeta, DriverError>>,
    },
    /// Close a single driver by id without removing it from the registry.
    CloseDriver {
        id: String,
        reply: oneshot::Sender<Result<(), DriverError>>,
    },
    /// Ping a single driver by id.
    PingDriver {
        id: String,
        reply: oneshot::Sender<Result<DriverMeta, DriverError>>,
    },
    /// Sync (read) current values for specified driver/point pairs.
    SyncAll {
        point_map: HashMap<DriverId, Vec<DriverPointRef>>,
        reply: oneshot::Sender<SyncAllResult>,
    },
    /// Discover points from a specific driver.
    Learn {
        driver_id: String,
        path: Option<String>,
        reply: oneshot::Sender<Result<LearnGrid, DriverError>>,
    },
    /// Write values to a specific driver's points.
    Write {
        driver_id: String,
        writes: Vec<(u32, f64)>,
        reply: oneshot::Sender<WriteResult>,
    },
    /// Get summary info for all drivers.
    Status {
        reply: oneshot::Sender<Vec<DriverSummary>>,
    },
    /// Get detailed status for a specific driver and its points.
    DriverStatus {
        id: String,
        reply: oneshot::Sender<DetailedDriverStatus>,
    },
    /// List all registered driver IDs.
    ListIds { reply: oneshot::Sender<Vec<String>> },
    /// Get status of a specific driver.
    GetDriverStatus {
        id: String,
        reply: oneshot::Sender<Option<DriverStatus>>,
    },
    /// Register a point as belonging to a driver (for status inheritance).
    RegisterPoint {
        point_id: u32,
        driver_id: String,
        reply: oneshot::Sender<()>,
    },
    /// Set a point-specific status override.
    SetPointStatus {
        point_id: u32,
        status: PointStatus,
        reply: oneshot::Sender<()>,
    },
    /// Get the effective status of a point (with inheritance).
    EffectivePointStatus {
        point_id: u32,
        reply: oneshot::Sender<Option<DriverStatus>>,
    },
    /// Add a watch subscription.
    AddWatch {
        subscriber: String,
        point_ids: Vec<u32>,
        reply: oneshot::Sender<()>,
    },
    /// Remove a watch subscription.
    RemoveWatch {
        subscriber: String,
        point_ids: Vec<u32>,
        reply: oneshot::Sender<()>,
    },
    /// Add a polling bucket.
    AddPollBucket {
        driver_id: String,
        interval: Duration,
        points: Vec<DriverPointRef>,
        reply: oneshot::Sender<()>,
    },
}

// ── DriverHandle (client-side) ────────────────────────────

/// Cloneable handle for sending commands to the [`DriverManagerActor`].
///
/// All methods are async — they send a command and await the response.
/// Safe to clone and share across Tokio tasks.
#[derive(Clone)]
pub struct DriverHandle {
    tx: mpsc::Sender<DriverCmd>,
    /// Broadcast endpoint for change-of-value events. Held as a `Sender`
    /// (not a `Receiver`) so each caller can mint a fresh `Receiver` via
    /// `.subscribe()` without actor round-trips. See
    /// [`DriverHandle::subscribe_cov`].
    cov_tx: broadcast::Sender<CovEvent>,
}

impl DriverHandle {
    /// Subscribe to change-of-value events.
    ///
    /// Returns a fresh [`broadcast::Receiver`] — each subscriber gets its
    /// own receive cursor. Events start flowing from the moment this is
    /// called; there is no backlog. If a subscriber falls behind by more
    /// than [`DEFAULT_COV_CAPACITY`] events, subsequent `recv()` calls
    /// return `RecvError::Lagged(n)` before resuming — standard broadcast
    /// semantics.
    ///
    /// This is a sync method (no actor round-trip) — cheap enough to call
    /// per-request from a REST handler or WebSocket session.
    pub fn subscribe_cov(&self) -> broadcast::Receiver<CovEvent> {
        self.cov_tx.subscribe()
    }

    /// Register a new driver.
    pub async fn register(&self, driver: AnyDriver) -> Result<(), DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::Register { driver, reply })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))?
    }

    /// Remove a driver by ID.
    pub async fn remove(&self, id: &str) -> Result<bool, DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::Remove {
                id: id.to_string(),
                reply,
            })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))
    }

    /// Open all registered drivers.
    pub async fn open_all(
        &self,
    ) -> Result<Vec<(DriverId, Result<DriverMeta, DriverError>)>, DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::OpenAll { reply })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))
    }

    /// Close all registered drivers.
    pub async fn close_all(&self) -> Result<(), DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::CloseAll { reply })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))
    }

    /// Open (initialize) a single registered driver by id.
    ///
    /// Returns `DriverError::ConfigFault` if no driver with that id is
    /// registered, or whatever the driver's own `open()` returns.
    pub async fn open_driver(&self, id: &str) -> Result<DriverMeta, DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::OpenDriver {
                id: id.to_string(),
                reply,
            })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))?
    }

    /// Close a single registered driver by id WITHOUT removing it.
    ///
    /// Use [`DriverHandle::remove`] instead if you want to fully deregister
    /// the driver (that variant also closes it first).
    pub async fn close_driver(&self, id: &str) -> Result<(), DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::CloseDriver {
                id: id.to_string(),
                reply,
            })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))?
    }

    /// Health-check (`ping`) a single driver by id.
    pub async fn ping_driver(&self, id: &str) -> Result<DriverMeta, DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::PingDriver {
                id: id.to_string(),
                reply,
            })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))?
    }

    /// Sync current values for all specified driver/point pairs.
    pub async fn sync_all(
        &self,
        point_map: HashMap<DriverId, Vec<DriverPointRef>>,
    ) -> Result<Vec<(DriverId, u32, Result<f64, DriverError>)>, DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::SyncAll { point_map, reply })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))
    }

    /// Discover points from a driver.
    pub async fn learn(
        &self,
        driver_id: &str,
        path: Option<&str>,
    ) -> Result<LearnGrid, DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::Learn {
                driver_id: driver_id.to_string(),
                path: path.map(|s| s.to_string()),
                reply,
            })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))?
    }

    /// Write values to a driver's points.
    pub async fn write(
        &self,
        driver_id: &str,
        writes: Vec<(u32, f64)>,
    ) -> Result<Vec<(u32, Result<(), DriverError>)>, DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::Write {
                driver_id: driver_id.to_string(),
                writes,
                reply,
            })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))?
    }

    /// Get summary info for all drivers.
    pub async fn status(&self) -> Result<Vec<DriverSummary>, DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::Status { reply })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))
    }

    /// Get detailed status for a specific driver and its points.
    pub async fn driver_status(
        &self,
        id: &str,
    ) -> Result<Option<(DriverStatus, Vec<(u32, DriverStatus)>)>, DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::DriverStatus {
                id: id.to_string(),
                reply,
            })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))
    }

    /// List all registered driver IDs.
    pub async fn list_ids(&self) -> Result<Vec<String>, DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::ListIds { reply })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))
    }

    /// Get the status of a specific driver.
    pub async fn get_driver_status(&self, id: &str) -> Result<Option<DriverStatus>, DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::GetDriverStatus {
                id: id.to_string(),
                reply,
            })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))
    }

    /// Register a point as belonging to a driver.
    pub async fn register_point(&self, point_id: u32, driver_id: &str) -> Result<(), DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::RegisterPoint {
                point_id,
                driver_id: driver_id.to_string(),
                reply,
            })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))
    }

    /// Set a point-specific status override.
    pub async fn set_point_status(
        &self,
        point_id: u32,
        status: PointStatus,
    ) -> Result<(), DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::SetPointStatus {
                point_id,
                status,
                reply,
            })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))
    }

    /// Get the effective status of a point.
    pub async fn effective_point_status(
        &self,
        point_id: u32,
    ) -> Result<Option<DriverStatus>, DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::EffectivePointStatus { point_id, reply })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))
    }

    /// Add a watch subscription.
    pub async fn add_watch(
        &self,
        subscriber: &str,
        point_ids: Vec<u32>,
    ) -> Result<(), DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::AddWatch {
                subscriber: subscriber.to_string(),
                point_ids,
                reply,
            })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))
    }

    /// Remove a watch subscription.
    pub async fn remove_watch(
        &self,
        subscriber: &str,
        point_ids: Vec<u32>,
    ) -> Result<(), DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::RemoveWatch {
                subscriber: subscriber.to_string(),
                point_ids,
                reply,
            })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))
    }

    /// Add a polling bucket for a driver.
    pub async fn add_poll_bucket(
        &self,
        driver_id: &str,
        interval: Duration,
        points: Vec<DriverPointRef>,
    ) -> Result<(), DriverError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(DriverCmd::AddPollBucket {
                driver_id: driver_id.to_string(),
                interval,
                points,
                reply,
            })
            .await
            .map_err(|_| DriverError::Internal("actor channel closed".into()))?;
        rx.await
            .map_err(|_| DriverError::Internal("actor response dropped".into()))
    }
}

// ── DriverManagerActor (server-side) ──────────────────────

/// Internal state of the driver manager actor.
///
/// This is the server-side of the actor — it holds all mutable state and
/// processes commands received via the mpsc channel.
struct DriverManagerInner {
    drivers: HashMap<DriverId, AnyDriver>,
    poll_scheduler: PollScheduler,
    watch_manager: DriverWatchManager,
    point_statuses: HashMap<u32, PointStatus>,
    point_driver_map: HashMap<u32, DriverId>,
    /// Change-of-value broadcast sender. Cloned from the one the
    /// [`DriverHandle`] holds, so `DriverHandle::subscribe_cov()` and the
    /// actor emit through the same channel.
    cov_tx: broadcast::Sender<CovEvent>,
    /// Last emitted value per point, used to gate CovEvent emission on
    /// actual change (vs. repeating the same value on every poll tick).
    /// A point appears here the first time it emits a successful read, so
    /// the very first successful read also produces a CovEvent.
    last_emitted: HashMap<u32, f64>,
}

impl DriverManagerInner {
    fn new(cov_tx: broadcast::Sender<CovEvent>) -> Self {
        Self {
            drivers: HashMap::new(),
            poll_scheduler: PollScheduler::new(),
            watch_manager: DriverWatchManager::new(),
            point_statuses: HashMap::new(),
            point_driver_map: HashMap::new(),
            cov_tx,
            last_emitted: HashMap::new(),
        }
    }

    fn register(&mut self, driver: AnyDriver) -> Result<(), DriverError> {
        let id = driver.id().to_string();
        if self.drivers.contains_key(&id) {
            return Err(DriverError::ConfigFault(format!(
                "driver '{id}' already registered"
            )));
        }
        self.drivers.insert(id, driver);
        Ok(())
    }

    async fn remove(&mut self, id: &str) -> bool {
        if let Some(mut driver) = self.drivers.remove(id) {
            driver.close().await;
            self.poll_scheduler.remove_driver(id);
            self.point_driver_map.retain(|_, did| did != id);
            true
        } else {
            false
        }
    }

    async fn open_all(&mut self) -> Vec<(DriverId, Result<DriverMeta, DriverError>)> {
        let ids: Vec<DriverId> = self.drivers.keys().cloned().collect();
        let mut results = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(driver) = self.drivers.get_mut(&id) {
                let result = driver.open().await;
                results.push((id, result));
            }
        }
        results
    }

    async fn close_all(&mut self) {
        let ids: Vec<DriverId> = self.drivers.keys().cloned().collect();
        for id in ids {
            if let Some(driver) = self.drivers.get_mut(&id) {
                driver.close().await;
            }
        }
    }

    async fn open_driver(&mut self, id: &str) -> Result<DriverMeta, DriverError> {
        match self.drivers.get_mut(id) {
            Some(d) => d.open().await,
            None => Err(DriverError::ConfigFault(format!(
                "driver '{id}' not found"
            ))),
        }
    }

    async fn close_driver(&mut self, id: &str) -> Result<(), DriverError> {
        match self.drivers.get_mut(id) {
            Some(d) => {
                d.close().await;
                Ok(())
            }
            None => Err(DriverError::ConfigFault(format!(
                "driver '{id}' not found"
            ))),
        }
    }

    async fn ping_driver(&mut self, id: &str) -> Result<DriverMeta, DriverError> {
        match self.drivers.get_mut(id) {
            Some(d) => d.ping().await,
            None => Err(DriverError::ConfigFault(format!(
                "driver '{id}' not found"
            ))),
        }
    }

    async fn sync_all(
        &mut self,
        point_map: &HashMap<DriverId, Vec<DriverPointRef>>,
    ) -> Vec<(DriverId, u32, Result<f64, DriverError>)> {
        let mut results = Vec::new();
        // Collect per-point status updates so we can apply them after the
        // driver mutable borrow is released. (Phase 12.0B — per-point
        // remote error reporting.)
        let mut status_updates: Vec<(u32, PointStatus)> = Vec::new();
        // Collect COV candidates (point_id, new value) to emit after the
        // driver borrow is released. Phase 12.0D — change-of-value broadcast.
        let mut cov_candidates: Vec<(u32, f64)> = Vec::new();
        for (driver_id, points) in point_map {
            if let Some(driver) = self.drivers.get_mut(driver_id) {
                for (point_id, result) in driver.sync_cur(points).await {
                    match &result {
                        Err(e) => {
                            status_updates.push((point_id, PointStatus::from_driver_error(e)));
                        }
                        Ok(v) => {
                            // Successful read — clear any previous remote-error
                            // status so the point appears healthy again.
                            status_updates.push((point_id, PointStatus::Inherited));
                            cov_candidates.push((point_id, *v));
                        }
                    }
                    results.push((driver_id.clone(), point_id, result));
                }
            }
        }
        for (pid, ps) in status_updates {
            // Inherited is the default — don't clutter the map with entries
            // that match the default.
            if matches!(ps, PointStatus::Inherited) {
                self.point_statuses.remove(&pid);
            } else {
                self.point_statuses.insert(pid, ps);
            }
        }
        // Emit CovEvent for each point whose value differs from the last
        // emitted one (or that has never emitted before). `bit_eq` style
        // via `!=` on f64 is intentional: NaN != NaN means a NaN read will
        // retrigger emission, which is the right behavior for "the value
        // stopped being a real number".
        let now = Instant::now();
        for (pid, v) in cov_candidates {
            let changed = match self.last_emitted.get(&pid) {
                Some(prev) => (*prev).to_bits() != v.to_bits(),
                None => true,
            };
            if !changed {
                continue;
            }
            self.last_emitted.insert(pid, v);
            let status = self
                .point_statuses
                .get(&pid)
                .cloned()
                .unwrap_or(PointStatus::Inherited);
            // `send` returns Err iff there are no live receivers. That's
            // normal — drop the event silently.
            let _ = self.cov_tx.send(CovEvent {
                point_id: pid,
                value: v,
                status,
                timestamp: now,
            });
        }
        results
    }

    async fn learn(
        &mut self,
        driver_id: &str,
        path: Option<&str>,
    ) -> Result<LearnGrid, DriverError> {
        self.drivers
            .get_mut(driver_id)
            .ok_or_else(|| DriverError::ConfigFault(format!("driver '{driver_id}' not found")))?
            .learn(path)
            .await
    }

    async fn write(
        &mut self,
        driver_id: &str,
        writes: &[(u32, f64)],
    ) -> Result<Vec<(u32, Result<(), DriverError>)>, DriverError> {
        let driver = self
            .drivers
            .get_mut(driver_id)
            .ok_or_else(|| DriverError::ConfigFault(format!("driver '{driver_id}' not found")))?;
        Ok(driver.write(writes).await)
    }

    fn driver_ids(&self) -> Vec<String> {
        self.drivers.keys().cloned().collect()
    }

    fn get_driver_status(&self, id: &str) -> Option<DriverStatus> {
        self.drivers.get(id).map(|d| d.status().clone())
    }

    fn driver_summaries(&self) -> Vec<DriverSummary> {
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

    fn driver_with_points_status(
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

    fn effective_point_status(&self, point_id: u32) -> Option<DriverStatus> {
        let driver_id = self.point_driver_map.get(&point_id)?;
        let driver_status = self.drivers.get(driver_id)?.status();
        let point_status = self
            .point_statuses
            .get(&point_id)
            .cloned()
            .unwrap_or_default();
        Some(point_status.resolve(driver_status))
    }

    async fn add_watch(&mut self, subscriber: &str, point_ids: &[u32]) {
        let newly_watched: Vec<u32> = point_ids
            .iter()
            .filter(|&&pid| !self.watch_manager.is_watched(pid))
            .copied()
            .collect();

        self.watch_manager.subscribe(subscriber, point_ids);

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
                    let _ = driver.on_watch(refs).await;
                }
            }
        }
    }

    async fn remove_watch(&mut self, subscriber: &str, point_ids: &[u32]) {
        let will_unwatch: Vec<u32> = point_ids
            .iter()
            .filter(|&&pid| {
                let subs = self.watch_manager.subscribers_for(pid);
                subs.len() == 1 && subs.contains(subscriber)
            })
            .copied()
            .collect();

        self.watch_manager.unsubscribe(subscriber, point_ids);

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
                    let _ = driver.on_unwatch(refs).await;
                }
            }
        }
    }
}

// ── Actor Spawn ───────────────────────────────────────────

/// Spawn the driver manager actor task, returning a [`DriverHandle`].
///
/// The actor runs in a background Tokio task and processes commands
/// until all [`DriverHandle`] clones are dropped (channel closes).
///
/// # Arguments
///
/// * `buffer` - mpsc channel buffer size (default: 64 is reasonable)
pub fn spawn_driver_actor(buffer: usize) -> DriverHandle {
    let (tx, mut rx) = mpsc::channel::<DriverCmd>(buffer);
    let (cov_tx, _cov_rx) = broadcast::channel::<CovEvent>(DEFAULT_COV_CAPACITY);

    let actor_cov_tx = cov_tx.clone();
    tokio::spawn(async move {
        let mut inner = DriverManagerInner::new(actor_cov_tx);
        debug!("driver manager actor started");

        while let Some(cmd) = rx.recv().await {
            match cmd {
                DriverCmd::Register { driver, reply } => {
                    let result = inner.register(driver);
                    let _ = reply.send(result);
                }
                DriverCmd::Remove { id, reply } => {
                    let removed = inner.remove(&id).await;
                    let _ = reply.send(removed);
                }
                DriverCmd::OpenAll { reply } => {
                    let results = inner.open_all().await;
                    let _ = reply.send(results);
                }
                DriverCmd::CloseAll { reply } => {
                    inner.close_all().await;
                    let _ = reply.send(());
                }
                DriverCmd::OpenDriver { id, reply } => {
                    let result = inner.open_driver(&id).await;
                    let _ = reply.send(result);
                }
                DriverCmd::CloseDriver { id, reply } => {
                    let result = inner.close_driver(&id).await;
                    let _ = reply.send(result);
                }
                DriverCmd::PingDriver { id, reply } => {
                    let result = inner.ping_driver(&id).await;
                    let _ = reply.send(result);
                }
                DriverCmd::SyncAll { point_map, reply } => {
                    let results = inner.sync_all(&point_map).await;
                    let _ = reply.send(results);
                }
                DriverCmd::Learn {
                    driver_id,
                    path,
                    reply,
                } => {
                    let result = inner.learn(&driver_id, path.as_deref()).await;
                    let _ = reply.send(result);
                }
                DriverCmd::Write {
                    driver_id,
                    writes,
                    reply,
                } => {
                    let result = inner.write(&driver_id, &writes).await;
                    let _ = reply.send(result);
                }
                DriverCmd::Status { reply } => {
                    let summaries = inner.driver_summaries();
                    let _ = reply.send(summaries);
                }
                DriverCmd::DriverStatus { id, reply } => {
                    let result = inner.driver_with_points_status(&id);
                    let _ = reply.send(result);
                }
                DriverCmd::ListIds { reply } => {
                    let ids = inner.driver_ids();
                    let _ = reply.send(ids);
                }
                DriverCmd::GetDriverStatus { id, reply } => {
                    let status = inner.get_driver_status(&id);
                    let _ = reply.send(status);
                }
                DriverCmd::RegisterPoint {
                    point_id,
                    driver_id,
                    reply,
                } => {
                    inner.point_driver_map.insert(point_id, driver_id);
                    let _ = reply.send(());
                }
                DriverCmd::SetPointStatus {
                    point_id,
                    status,
                    reply,
                } => {
                    inner.point_statuses.insert(point_id, status);
                    let _ = reply.send(());
                }
                DriverCmd::EffectivePointStatus { point_id, reply } => {
                    let status = inner.effective_point_status(point_id);
                    let _ = reply.send(status);
                }
                DriverCmd::AddWatch {
                    subscriber,
                    point_ids,
                    reply,
                } => {
                    inner.add_watch(&subscriber, &point_ids).await;
                    let _ = reply.send(());
                }
                DriverCmd::RemoveWatch {
                    subscriber,
                    point_ids,
                    reply,
                } => {
                    inner.remove_watch(&subscriber, &point_ids).await;
                    let _ = reply.send(());
                }
                DriverCmd::AddPollBucket {
                    driver_id,
                    interval,
                    points,
                    reply,
                } => {
                    inner
                        .poll_scheduler
                        .add_bucket(&driver_id, interval, points);
                    let _ = reply.send(());
                }
            }
        }

        debug!("driver manager actor stopped (channel closed)");
    });

    DriverHandle { tx, cov_tx }
}

// ── Tests ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::async_driver::AnyDriver;
    use crate::drivers::{
        Driver, DriverMeta, DriverPointRef, DriverStatus, SyncContext, WriteContext,
    };

    // ── Minimal sync driver for testing ───────────────────

    struct TestDriver {
        id: String,
        status: DriverStatus,
    }

    impl TestDriver {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                status: DriverStatus::Pending,
            }
        }
    }

    impl Driver for TestDriver {
        fn driver_type(&self) -> &'static str {
            "test"
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
                model: Some("Test".into()),
                ..Default::default()
            })
        }
        fn close(&mut self) {
            self.status = DriverStatus::Down;
        }
        fn ping(&mut self) -> Result<DriverMeta, DriverError> {
            Ok(DriverMeta::default())
        }
        fn sync_cur(&mut self, points: &[DriverPointRef], ctx: &mut SyncContext) {
            for p in points {
                ctx.update_cur_ok(p.point_id, 0.0);
            }
        }
        fn write(&mut self, writes: &[(u32, f64)], ctx: &mut WriteContext) {
            for (id, _) in writes {
                ctx.update_write_ok(*id);
            }
        }
    }

    fn test_driver(id: &str) -> AnyDriver {
        AnyDriver::Sync(Box::new(TestDriver::new(id)))
    }

    // ── Actor tests ───────────────────────────────────────

    #[tokio::test]
    async fn actor_register_and_list() {
        let handle = spawn_driver_actor(16);
        handle.register(test_driver("d1")).await.unwrap();
        handle.register(test_driver("d2")).await.unwrap();

        let ids = handle.list_ids().await.unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"d1".to_string()));
        assert!(ids.contains(&"d2".to_string()));
    }

    #[tokio::test]
    async fn actor_reject_duplicate() {
        let handle = spawn_driver_actor(16);
        handle.register(test_driver("dup")).await.unwrap();
        let result = handle.register(test_driver("dup")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn actor_open_all() {
        let handle = spawn_driver_actor(16);
        handle.register(test_driver("a")).await.unwrap();
        handle.register(test_driver("b")).await.unwrap();

        let results = handle.open_all().await.unwrap();
        assert_eq!(results.len(), 2);
        for (_, result) in &results {
            assert!(result.is_ok());
        }

        // Verify status changed
        let status = handle.get_driver_status("a").await.unwrap();
        assert_eq!(status, Some(DriverStatus::Ok));
    }

    #[tokio::test]
    async fn actor_close_all() {
        let handle = spawn_driver_actor(16);
        handle.register(test_driver("x")).await.unwrap();
        handle.open_all().await.unwrap();

        handle.close_all().await.unwrap();

        let status = handle.get_driver_status("x").await.unwrap();
        assert_eq!(status, Some(DriverStatus::Down));
    }

    #[tokio::test]
    async fn actor_remove() {
        let handle = spawn_driver_actor(16);
        handle.register(test_driver("rm")).await.unwrap();
        handle.open_all().await.unwrap();

        let removed = handle.remove("rm").await.unwrap();
        assert!(removed);

        let removed_again = handle.remove("rm").await.unwrap();
        assert!(!removed_again);

        let ids = handle.list_ids().await.unwrap();
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn actor_sync_all() {
        let handle = spawn_driver_actor(16);
        handle.register(test_driver("io")).await.unwrap();
        handle.open_all().await.unwrap();

        let mut point_map = HashMap::new();
        point_map.insert(
            "io".to_string(),
            vec![
                DriverPointRef {
                    point_id: 100,
                    address: "A".into(),
                },
                DriverPointRef {
                    point_id: 200,
                    address: "B".into(),
                },
            ],
        );

        let results = handle.sync_all(point_map).await.unwrap();
        assert_eq!(results.len(), 2);
        for (_, _, result) in &results {
            assert!(result.is_ok());
        }
    }

    #[tokio::test]
    async fn actor_write() {
        let handle = spawn_driver_actor(16);
        handle.register(test_driver("w")).await.unwrap();
        handle.open_all().await.unwrap();

        let results = handle
            .write("w", vec![(100, 72.5), (200, 1.0)])
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        for (_, result) in &results {
            assert!(result.is_ok());
        }
    }

    #[tokio::test]
    async fn actor_write_unknown_driver() {
        let handle = spawn_driver_actor(16);
        let result = handle.write("nonexistent", vec![(1, 0.0)]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn actor_learn_unknown_driver() {
        let handle = spawn_driver_actor(16);
        let result = handle.learn("nonexistent", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn actor_status_summaries() {
        let handle = spawn_driver_actor(16);
        handle.register(test_driver("s1")).await.unwrap();
        handle.open_all().await.unwrap();

        let summaries = handle.status().await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "s1");
        assert_eq!(summaries[0].driver_type, "test");
        assert_eq!(summaries[0].status, DriverStatus::Ok);
    }

    #[tokio::test]
    async fn actor_point_status_inheritance() {
        let handle = spawn_driver_actor(16);
        handle.register(test_driver("io")).await.unwrap();
        handle.open_all().await.unwrap();

        handle.register_point(100, "io").await.unwrap();
        handle.register_point(200, "io").await.unwrap();

        // Default: inherit driver status (Ok)
        let status = handle.effective_point_status(100).await.unwrap();
        assert_eq!(status, Some(DriverStatus::Ok));

        // Override point 200
        handle
            .set_point_status(200, PointStatus::Own(DriverStatus::Stale))
            .await
            .unwrap();
        let status = handle.effective_point_status(200).await.unwrap();
        assert_eq!(status, Some(DriverStatus::Stale));

        // Point 100 still inherits
        let status = handle.effective_point_status(100).await.unwrap();
        assert_eq!(status, Some(DriverStatus::Ok));
    }

    #[tokio::test]
    async fn actor_point_status_unknown() {
        let handle = spawn_driver_actor(16);
        let status = handle.effective_point_status(999).await.unwrap();
        assert_eq!(status, None);
    }

    #[tokio::test]
    async fn actor_driver_with_points_status() {
        let handle = spawn_driver_actor(16);
        handle.register(test_driver("io")).await.unwrap();
        handle.open_all().await.unwrap();

        handle.register_point(100, "io").await.unwrap();
        handle.register_point(200, "io").await.unwrap();
        handle
            .set_point_status(200, PointStatus::Own(DriverStatus::Fault("bad".into())))
            .await
            .unwrap();

        let result = handle.driver_status("io").await.unwrap();
        let (ds, pts) = result.unwrap();
        assert_eq!(ds, DriverStatus::Ok);
        assert_eq!(pts.len(), 2);

        let p100 = pts.iter().find(|(pid, _)| *pid == 100).unwrap();
        assert_eq!(p100.1, DriverStatus::Ok);
        let p200 = pts.iter().find(|(pid, _)| *pid == 200).unwrap();
        assert_eq!(p200.1, DriverStatus::Fault("bad".into()));
    }

    #[tokio::test]
    async fn actor_driver_status_unknown() {
        let handle = spawn_driver_actor(16);
        let result = handle.driver_status("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn actor_add_remove_watch() {
        let handle = spawn_driver_actor(16);
        handle.register(test_driver("io")).await.unwrap();
        handle.open_all().await.unwrap();
        handle.register_point(100, "io").await.unwrap();

        handle.add_watch("client-1", vec![100]).await.unwrap();
        // The watch is tracked internally — we verify via status
        // (no direct watch_manager access from handle, which is by design)

        handle.remove_watch("client-1", vec![100]).await.unwrap();
    }

    #[tokio::test]
    async fn actor_add_poll_bucket() {
        let handle = spawn_driver_actor(16);
        handle.register(test_driver("io")).await.unwrap();
        handle.open_all().await.unwrap();

        handle
            .add_poll_bucket(
                "io",
                Duration::from_secs(10),
                vec![DriverPointRef {
                    point_id: 1,
                    address: "A".into(),
                }],
            )
            .await
            .unwrap();

        // Verify via summaries
        let summaries = handle.status().await.unwrap();
        assert_eq!(summaries[0].poll_buckets, 1);
        assert_eq!(summaries[0].poll_points, 1);
    }

    #[tokio::test]
    async fn actor_handle_is_clone() {
        let handle = spawn_driver_actor(16);
        let handle2 = handle.clone();

        handle.register(test_driver("d1")).await.unwrap();
        // Second handle can see what the first registered
        let ids = handle2.list_ids().await.unwrap();
        assert_eq!(ids.len(), 1);
    }

    // ── Phase 12.0D CovEvent tests ────────────────────────

    use std::sync::{Arc, Mutex};

    /// Test driver whose `sync_cur` returns a value controlled via a shared
    /// `Arc<Mutex<f64>>`. The mutex is held briefly, so serialisation under
    /// the actor's single-threaded mpsc loop is fine.
    struct ValueDriver {
        id: String,
        status: DriverStatus,
        value: Arc<Mutex<f64>>,
    }

    impl ValueDriver {
        fn new(id: &str, value: Arc<Mutex<f64>>) -> Self {
            Self {
                id: id.to_string(),
                status: DriverStatus::Pending,
                value,
            }
        }
    }

    impl Driver for ValueDriver {
        fn driver_type(&self) -> &'static str {
            "valuetest"
        }
        fn id(&self) -> &str {
            &self.id
        }
        fn status(&self) -> &DriverStatus {
            &self.status
        }
        fn open(&mut self) -> Result<DriverMeta, DriverError> {
            self.status = DriverStatus::Ok;
            Ok(DriverMeta::default())
        }
        fn close(&mut self) {
            self.status = DriverStatus::Down;
        }
        fn ping(&mut self) -> Result<DriverMeta, DriverError> {
            Ok(DriverMeta::default())
        }
        fn sync_cur(&mut self, points: &[DriverPointRef], ctx: &mut SyncContext) {
            let v = *self.value.lock().unwrap();
            for p in points {
                ctx.update_cur_ok(p.point_id, v);
            }
        }
        fn write(&mut self, writes: &[(u32, f64)], ctx: &mut WriteContext) {
            for (id, _) in writes {
                ctx.update_write_ok(*id);
            }
        }
    }

    fn value_driver(id: &str, value: Arc<Mutex<f64>>) -> AnyDriver {
        AnyDriver::Sync(Box::new(ValueDriver::new(id, value)))
    }

    fn sync_map(driver_id: &str, point_id: u32) -> HashMap<DriverId, Vec<DriverPointRef>> {
        let mut m = HashMap::new();
        m.insert(
            driver_id.to_string(),
            vec![DriverPointRef {
                point_id,
                address: "test".into(),
            }],
        );
        m
    }

    #[tokio::test]
    async fn cov_emits_on_first_read() {
        let handle = spawn_driver_actor(16);
        let value = Arc::new(Mutex::new(42.0f64));
        handle
            .register(value_driver("vd", value.clone()))
            .await
            .unwrap();
        handle.open_all().await.unwrap();

        let mut rx = handle.subscribe_cov();
        let _ = handle.sync_all(sync_map("vd", 1)).await.unwrap();

        let evt = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("cov event should arrive")
            .expect("receiver should be open");
        assert_eq!(evt.point_id, 1);
        assert!((evt.value - 42.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn cov_suppresses_repeat_value() {
        let handle = spawn_driver_actor(16);
        let value = Arc::new(Mutex::new(10.0f64));
        handle
            .register(value_driver("vd", value.clone()))
            .await
            .unwrap();
        handle.open_all().await.unwrap();

        let mut rx = handle.subscribe_cov();
        handle.sync_all(sync_map("vd", 7)).await.unwrap();
        // Drain the first event.
        let _first = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .unwrap()
            .unwrap();

        // Same value — no new event should arrive within the timeout window.
        handle.sync_all(sync_map("vd", 7)).await.unwrap();
        let res = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(res.is_err(), "no CovEvent expected for repeat value");
    }

    #[tokio::test]
    async fn cov_emits_on_change() {
        let handle = spawn_driver_actor(16);
        let value = Arc::new(Mutex::new(1.0f64));
        handle
            .register(value_driver("vd", value.clone()))
            .await
            .unwrap();
        handle.open_all().await.unwrap();

        let mut rx = handle.subscribe_cov();
        handle.sync_all(sync_map("vd", 5)).await.unwrap();
        let first = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!((first.value - 1.0).abs() < f64::EPSILON);

        // Mutate value, sync again — expect a second event with the new value.
        *value.lock().unwrap() = 2.5;
        handle.sync_all(sync_map("vd", 5)).await.unwrap();
        let second = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.point_id, 5);
        assert!((second.value - 2.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn cov_multiple_subscribers_each_receive() {
        let handle = spawn_driver_actor(16);
        let value = Arc::new(Mutex::new(7.5f64));
        handle
            .register(value_driver("vd", value.clone()))
            .await
            .unwrap();
        handle.open_all().await.unwrap();

        let mut a = handle.subscribe_cov();
        let mut b = handle.subscribe_cov();
        handle.sync_all(sync_map("vd", 11)).await.unwrap();

        let ea = tokio::time::timeout(Duration::from_millis(200), a.recv())
            .await
            .unwrap()
            .unwrap();
        let eb = tokio::time::timeout(Duration::from_millis(200), b.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ea.point_id, 11);
        assert_eq!(eb.point_id, 11);
    }

    #[tokio::test]
    async fn cov_late_subscriber_no_backlog() {
        let handle = spawn_driver_actor(16);
        let value = Arc::new(Mutex::new(3.0f64));
        handle
            .register(value_driver("vd", value.clone()))
            .await
            .unwrap();
        handle.open_all().await.unwrap();

        // First sync happens before anyone subscribes — event is dropped.
        handle.sync_all(sync_map("vd", 9)).await.unwrap();

        // Subscribe after.
        let mut rx = handle.subscribe_cov();

        // Nothing queued.
        let res = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(res.is_err(), "broadcast must not backlog before subscribe");

        // But a change AFTER subscribing should arrive.
        *value.lock().unwrap() = 4.0;
        handle.sync_all(sync_map("vd", 9)).await.unwrap();
        let evt = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!((evt.value - 4.0).abs() < f64::EPSILON);
    }

    // ── Phase 12.0G lifecycle tests ────────────────────────

    #[tokio::test]
    async fn actor_open_close_ping_single_driver() {
        let handle = spawn_driver_actor(16);
        handle.register(test_driver("d1")).await.unwrap();

        // open_driver brings just that one up; status should be Ok.
        let meta = handle.open_driver("d1").await.unwrap();
        assert_eq!(meta.model.as_deref(), Some("Test"));
        assert_eq!(
            handle.get_driver_status("d1").await.unwrap(),
            Some(DriverStatus::Ok)
        );

        // ping_driver after open.
        let _ = handle.ping_driver("d1").await.unwrap();

        // close_driver flips status to Down but does NOT remove the driver.
        handle.close_driver("d1").await.unwrap();
        assert_eq!(
            handle.get_driver_status("d1").await.unwrap(),
            Some(DriverStatus::Down)
        );
        assert_eq!(handle.list_ids().await.unwrap(), vec!["d1".to_string()]);
    }

    #[tokio::test]
    async fn actor_lifecycle_unknown_driver_errors() {
        let handle = spawn_driver_actor(16);

        let open_err = handle.open_driver("missing").await.unwrap_err();
        assert!(open_err.to_string().contains("not found"));

        let close_err = handle.close_driver("missing").await.unwrap_err();
        assert!(close_err.to_string().contains("not found"));

        let ping_err = handle.ping_driver("missing").await.unwrap_err();
        assert!(ping_err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn cov_error_does_not_emit() {
        let handle = spawn_driver_actor(16);
        // TestDriver returns Ok — we need an error path.  Register a driver
        // but ask to sync a point_id that doesn't map to any driver in the
        // point_map — sync_all silently skips those, so we use a stub that
        // deliberately errors.

        struct ErrDriver {
            id: String,
            status: DriverStatus,
        }
        impl Driver for ErrDriver {
            fn driver_type(&self) -> &'static str {
                "err"
            }
            fn id(&self) -> &str {
                &self.id
            }
            fn status(&self) -> &DriverStatus {
                &self.status
            }
            fn open(&mut self) -> Result<DriverMeta, DriverError> {
                self.status = DriverStatus::Ok;
                Ok(DriverMeta::default())
            }
            fn close(&mut self) {
                self.status = DriverStatus::Down;
            }
            fn ping(&mut self) -> Result<DriverMeta, DriverError> {
                Ok(DriverMeta::default())
            }
            fn sync_cur(&mut self, points: &[DriverPointRef], ctx: &mut SyncContext) {
                for p in points {
                    ctx.update_cur_err(p.point_id, DriverError::CommFault("boom".into()));
                }
            }
            fn write(&mut self, _: &[(u32, f64)], _: &mut WriteContext) {}
        }

        handle
            .register(AnyDriver::Sync(Box::new(ErrDriver {
                id: "err".into(),
                status: DriverStatus::Pending,
            })))
            .await
            .unwrap();
        handle.open_all().await.unwrap();

        let mut rx = handle.subscribe_cov();
        handle.sync_all(sync_map("err", 1)).await.unwrap();

        let res = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(res.is_err(), "errored reads must not emit CovEvent");
    }
}
