//! Async driver trait for network-based drivers.
//!
//! Provides [`AsyncDriver`] — an async counterpart to the synchronous [`Driver`]
//! trait — and [`AnyDriver`], a wrapper that unifies both into a single dispatch
//! type. This enables network-based drivers (Modbus TCP, BACnet IP, MQTT) to
//! perform real async I/O while local drivers (GPIO/ADC) keep their simple
//! synchronous implementation.
//!
//! ## Design Rationale
//!
//! Rather than converting the existing `Driver` trait (which would break all
//! implementations), we introduce a parallel `AsyncDriver` trait. The
//! [`AnyDriver`] enum bridges the two so the actor-based
//! [`DriverHandle`](super::actor::DriverHandle) can manage both kinds
//! uniformly.

use async_trait::async_trait;

use super::{
    DriverError, DriverMessage, DriverMeta, DriverPointRef, DriverStatus, LearnGrid, PollMode,
    SyncContext, WriteContext,
};

// ── AsyncDriver Trait ─────────────────────────────────────

/// Async version of the [`Driver`](super::Driver) trait for network-based drivers.
///
/// Drivers that perform network I/O (Modbus TCP, BACnet IP, MQTT) should
/// implement this trait so their I/O operations can be awaited without
/// blocking the Tokio runtime.
///
/// Sync drivers (e.g., `LocalIoDriver`) continue using the regular
/// [`Driver`](super::Driver) trait. Both are managed uniformly via [`AnyDriver`].
#[async_trait]
pub trait AsyncDriver: Send + Sync {
    /// The driver type name (e.g., "modbus", "bacnet", "mqtt").
    fn driver_type(&self) -> &'static str;

    /// Unique instance identifier.
    fn id(&self) -> &str;

    /// Current operational status.
    fn status(&self) -> &DriverStatus;

    /// How this driver wants to be polled.
    fn poll_mode(&self) -> PollMode {
        PollMode::Buckets
    }

    /// Initialize the driver (connect to remote device, open socket, etc.).
    async fn open(&mut self) -> Result<DriverMeta, DriverError>;

    /// Shut down the driver (close connections, release resources).
    async fn close(&mut self);

    /// Health check (verify remote device is reachable).
    async fn ping(&mut self) -> Result<DriverMeta, DriverError>;

    /// Discover available points at the given path (or root if `None`).
    async fn learn(&mut self, _path: Option<&str>) -> Result<LearnGrid, DriverError> {
        Err(DriverError::NotSupported("learn"))
    }

    /// Read current values for a batch of points.
    ///
    /// Drivers populate the provided [`SyncContext`] by calling
    /// `ctx.update_cur_ok(id, value)` or `ctx.update_cur_err(id, err)` for
    /// each point.
    async fn sync_cur(&mut self, points: &[DriverPointRef], ctx: &mut SyncContext);

    /// Write values to points.
    ///
    /// Drivers populate the provided [`WriteContext`] by calling
    /// `ctx.update_write_ok(id)` or `ctx.update_write_err(id, err)` for
    /// each point.
    async fn write(&mut self, writes: &[(u32, f64)], ctx: &mut WriteContext);

    /// Subscribe to change-of-value notifications for these points.
    ///
    /// For COV-capable protocols (BACnet SubscribeCOV, MQTT topics), this
    /// establishes the subscription on the remote system. Polling-based
    /// drivers can use the default no-op implementation.
    async fn on_watch(&mut self, _points: &[DriverPointRef]) -> Result<(), DriverError> {
        Ok(())
    }

    /// Unsubscribe from change-of-value notifications.
    async fn on_unwatch(&mut self, _points: &[DriverPointRef]) -> Result<(), DriverError> {
        Ok(())
    }

    /// Handle a driver-specific custom message (Phase 12.0E).
    ///
    /// Drivers override to accept out-of-band commands (re-discover,
    /// reconnect, stats). Default returns [`DriverError::NotSupported`].
    async fn on_receive(&mut self, _msg: DriverMessage) -> Result<DriverMessage, DriverError> {
        Err(DriverError::NotSupported("on_receive"))
    }
}

// ── AnyDriver Wrapper ─────────────────────────────────────

/// Wrapper that unifies sync [`Driver`](super::Driver) and [`AsyncDriver`] into
/// one dispatchable type.
///
/// The actor-based [`DriverHandle`](super::actor::DriverHandle) stores
/// drivers as `AnyDriver` instances and calls methods through this wrapper,
/// which `.await`s async drivers and calls sync drivers directly.
pub enum AnyDriver {
    /// A synchronous driver (e.g., `LocalIoDriver`).
    Sync(Box<dyn super::Driver>),
    /// An async driver (e.g., `ModbusDriver`, `BacnetDriver`, `MqttDriver`).
    Async(Box<dyn AsyncDriver>),
}

impl AnyDriver {
    /// Get the driver type name.
    pub fn driver_type(&self) -> &'static str {
        match self {
            AnyDriver::Sync(d) => d.driver_type(),
            AnyDriver::Async(d) => d.driver_type(),
        }
    }

    /// Get the unique instance identifier.
    pub fn id(&self) -> &str {
        match self {
            AnyDriver::Sync(d) => d.id(),
            AnyDriver::Async(d) => d.id(),
        }
    }

    /// Get the current operational status.
    pub fn status(&self) -> &DriverStatus {
        match self {
            AnyDriver::Sync(d) => d.status(),
            AnyDriver::Async(d) => d.status(),
        }
    }

    /// Get the poll mode.
    pub fn poll_mode(&self) -> PollMode {
        match self {
            AnyDriver::Sync(d) => d.poll_mode(),
            AnyDriver::Async(d) => d.poll_mode(),
        }
    }

    /// Initialize the driver.
    pub async fn open(&mut self) -> Result<DriverMeta, DriverError> {
        match self {
            AnyDriver::Sync(d) => d.open(),
            AnyDriver::Async(d) => d.open().await,
        }
    }

    /// Shut down the driver.
    pub async fn close(&mut self) {
        match self {
            AnyDriver::Sync(d) => d.close(),
            AnyDriver::Async(d) => d.close().await,
        }
    }

    /// Health check.
    pub async fn ping(&mut self) -> Result<DriverMeta, DriverError> {
        match self {
            AnyDriver::Sync(d) => d.ping(),
            AnyDriver::Async(d) => d.ping().await,
        }
    }

    /// Discover available points.
    pub async fn learn(&mut self, path: Option<&str>) -> Result<LearnGrid, DriverError> {
        match self {
            AnyDriver::Sync(d) => d.learn(path),
            AnyDriver::Async(d) => d.learn(path).await,
        }
    }

    /// Read current values for a batch of points.
    ///
    /// Internally constructs a [`SyncContext`], passes it to the underlying
    /// driver, then drains it into the `(point_id, Result<f64, _>)` vector
    /// that the actor and REST layer consume. Drivers never see the vector.
    pub async fn sync_cur(
        &mut self,
        points: &[DriverPointRef],
    ) -> Vec<(u32, Result<f64, DriverError>)> {
        let mut ctx = SyncContext::with_capacity(points.len());
        match self {
            AnyDriver::Sync(d) => d.sync_cur(points, &mut ctx),
            AnyDriver::Async(d) => d.sync_cur(points, &mut ctx).await,
        }
        ctx.into_results()
    }

    /// Write values to points.
    ///
    /// Internally constructs a [`WriteContext`], passes it to the underlying
    /// driver, then drains it into the `(point_id, Result<(), _>)` vector
    /// that the actor consumes.
    pub async fn write(&mut self, writes: &[(u32, f64)]) -> Vec<(u32, Result<(), DriverError>)> {
        let mut ctx = WriteContext::with_capacity(writes.len());
        match self {
            AnyDriver::Sync(d) => d.write(writes, &mut ctx),
            AnyDriver::Async(d) => d.write(writes, &mut ctx).await,
        }
        ctx.into_results()
    }

    /// Subscribe to COV notifications.
    pub async fn on_watch(&mut self, points: &[DriverPointRef]) -> Result<(), DriverError> {
        match self {
            AnyDriver::Sync(d) => d.on_watch(points),
            AnyDriver::Async(d) => d.on_watch(points).await,
        }
    }

    /// Unsubscribe from COV notifications.
    pub async fn on_unwatch(&mut self, points: &[DriverPointRef]) -> Result<(), DriverError> {
        match self {
            AnyDriver::Sync(d) => d.on_unwatch(points),
            AnyDriver::Async(d) => d.on_unwatch(points).await,
        }
    }

    /// Handle a driver-specific custom message (Phase 12.0E).
    pub async fn on_receive(
        &mut self,
        msg: DriverMessage,
    ) -> Result<DriverMessage, DriverError> {
        match self {
            AnyDriver::Sync(d) => d.on_receive(msg),
            AnyDriver::Async(d) => d.on_receive(msg).await,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::{Driver, DriverMeta, DriverStatus};

    // ── Mock sync driver ──────────────────────────────────

    struct MockSyncDriver {
        id: String,
        status: DriverStatus,
    }

    impl MockSyncDriver {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                status: DriverStatus::Pending,
            }
        }
    }

    impl Driver for MockSyncDriver {
        fn driver_type(&self) -> &'static str {
            "mock-sync"
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
                model: Some("MockSync".into()),
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
                ctx.update_cur_ok(p.point_id, 42.0);
            }
        }
        fn write(&mut self, writes: &[(u32, f64)], ctx: &mut WriteContext) {
            for (id, _) in writes {
                ctx.update_write_ok(*id);
            }
        }
    }

    // ── Mock async driver ─────────────────────────────────

    struct MockAsyncDriver {
        id: String,
        status: DriverStatus,
    }

    impl MockAsyncDriver {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                status: DriverStatus::Pending,
            }
        }
    }

    #[async_trait]
    impl AsyncDriver for MockAsyncDriver {
        fn driver_type(&self) -> &'static str {
            "mock-async"
        }
        fn id(&self) -> &str {
            &self.id
        }
        fn status(&self) -> &DriverStatus {
            &self.status
        }
        async fn open(&mut self) -> Result<DriverMeta, DriverError> {
            self.status = DriverStatus::Ok;
            Ok(DriverMeta {
                model: Some("MockAsync".into()),
                ..Default::default()
            })
        }
        async fn close(&mut self) {
            self.status = DriverStatus::Down;
        }
        async fn ping(&mut self) -> Result<DriverMeta, DriverError> {
            Ok(DriverMeta::default())
        }
        async fn sync_cur(&mut self, points: &[DriverPointRef], ctx: &mut SyncContext) {
            for p in points {
                ctx.update_cur_ok(p.point_id, 99.0);
            }
        }
        async fn write(&mut self, writes: &[(u32, f64)], ctx: &mut WriteContext) {
            for (id, _) in writes {
                ctx.update_write_ok(*id);
            }
        }
    }

    // ── AnyDriver tests ───────────────────────────────────

    #[tokio::test]
    async fn any_driver_sync_open_close() {
        let mut drv = AnyDriver::Sync(Box::new(MockSyncDriver::new("s1")));
        assert_eq!(drv.id(), "s1");
        assert_eq!(drv.driver_type(), "mock-sync");
        assert_eq!(*drv.status(), DriverStatus::Pending);

        let meta = drv.open().await.unwrap();
        assert_eq!(meta.model, Some("MockSync".into()));
        assert_eq!(*drv.status(), DriverStatus::Ok);

        drv.close().await;
        assert_eq!(*drv.status(), DriverStatus::Down);
    }

    #[tokio::test]
    async fn any_driver_async_open_close() {
        let mut drv = AnyDriver::Async(Box::new(MockAsyncDriver::new("a1")));
        assert_eq!(drv.id(), "a1");
        assert_eq!(drv.driver_type(), "mock-async");
        assert_eq!(*drv.status(), DriverStatus::Pending);

        let meta = drv.open().await.unwrap();
        assert_eq!(meta.model, Some("MockAsync".into()));
        assert_eq!(*drv.status(), DriverStatus::Ok);

        drv.close().await;
        assert_eq!(*drv.status(), DriverStatus::Down);
    }

    #[tokio::test]
    async fn any_driver_sync_sync_cur() {
        let mut drv = AnyDriver::Sync(Box::new(MockSyncDriver::new("s2")));
        drv.open().await.unwrap();

        let refs = vec![DriverPointRef {
            point_id: 100,
            address: "A".into(),
        }];
        let results = drv.sync_cur(&refs).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 100);
        assert!((results[0].1.as_ref().unwrap() - 42.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn any_driver_async_sync_cur() {
        let mut drv = AnyDriver::Async(Box::new(MockAsyncDriver::new("a2")));
        drv.open().await.unwrap();

        let refs = vec![DriverPointRef {
            point_id: 200,
            address: "B".into(),
        }];
        let results = drv.sync_cur(&refs).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 200);
        assert!((results[0].1.as_ref().unwrap() - 99.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn any_driver_sync_write() {
        let mut drv = AnyDriver::Sync(Box::new(MockSyncDriver::new("s3")));
        drv.open().await.unwrap();

        let results = drv.write(&[(1, 10.0), (2, 20.0)]).await;
        assert_eq!(results.len(), 2);
        assert!(results[0].1.is_ok());
        assert!(results[1].1.is_ok());
    }

    #[tokio::test]
    async fn any_driver_async_write() {
        let mut drv = AnyDriver::Async(Box::new(MockAsyncDriver::new("a3")));
        drv.open().await.unwrap();

        let results = drv.write(&[(1, 10.0)]).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_ok());
    }

    #[tokio::test]
    async fn any_driver_sync_ping() {
        let mut drv = AnyDriver::Sync(Box::new(MockSyncDriver::new("s4")));
        drv.open().await.unwrap();
        assert!(drv.ping().await.is_ok());
    }

    #[tokio::test]
    async fn any_driver_async_ping() {
        let mut drv = AnyDriver::Async(Box::new(MockAsyncDriver::new("a4")));
        drv.open().await.unwrap();
        assert!(drv.ping().await.is_ok());
    }

    #[tokio::test]
    async fn any_driver_sync_learn_default() {
        let mut drv = AnyDriver::Sync(Box::new(MockSyncDriver::new("s5")));
        // Default learn returns NotSupported
        let result = drv.learn(None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn any_driver_async_learn_default() {
        let mut drv = AnyDriver::Async(Box::new(MockAsyncDriver::new("a5")));
        let result = drv.learn(None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn any_driver_poll_mode() {
        let drv_sync = AnyDriver::Sync(Box::new(MockSyncDriver::new("s6")));
        assert_eq!(drv_sync.poll_mode(), PollMode::Buckets);

        let drv_async = AnyDriver::Async(Box::new(MockAsyncDriver::new("a6")));
        assert_eq!(drv_async.poll_mode(), PollMode::Buckets);
    }

    #[tokio::test]
    async fn any_driver_on_watch_default() {
        let mut drv = AnyDriver::Sync(Box::new(MockSyncDriver::new("s7")));
        let refs = vec![DriverPointRef {
            point_id: 1,
            address: "X".into(),
        }];
        assert!(drv.on_watch(&refs).await.is_ok());
        assert!(drv.on_unwatch(&refs).await.is_ok());
    }

    #[tokio::test]
    async fn any_driver_async_on_watch_default() {
        let mut drv = AnyDriver::Async(Box::new(MockAsyncDriver::new("a7")));
        let refs = vec![DriverPointRef {
            point_id: 1,
            address: "X".into(),
        }];
        assert!(drv.on_watch(&refs).await.is_ok());
        assert!(drv.on_unwatch(&refs).await.is_ok());
    }
}
