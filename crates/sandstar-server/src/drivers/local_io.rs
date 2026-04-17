//! Local I/O driver for BeagleBone hardware (Phase 12.0F).
//!
//! This driver unifies local hardware I/O (GPIO / ADC / I2C / PWM) under
//! the [`AsyncDriver`] trait so it can be registered and managed through
//! the same `DriverHandle` actor as the network drivers (BACnet, MQTT).
//!
//! ## Engine delegation
//!
//! The engine's existing poll loop — which is what Device 1-3's production
//! HVAC runs on — is **not** touched. Instead, this driver optionally
//! holds an [`EngineHandle`] and delegates:
//!
//! - `sync_cur` → `engine.read_channel(id)` for each point
//! - `write`    → `engine.write_channel(id, value, level=16, …)`
//!
//! When no engine handle is supplied (unit tests), the driver keeps a
//! cached value per channel that callers can seed via `update_value` and
//! `configure_channels`, matching the legacy behavior so existing tests
//! continue to pass.
//!
//! The engine's own poll loop remains authoritative for hardware reads.
//! This driver is a façade — it lets operators list, inspect, and drive
//! local-I/O channels through `/api/drivers/{id}/…` consistently with
//! other drivers.

use std::collections::HashMap;

use async_trait::async_trait;

use super::async_driver::AsyncDriver;
use super::DriverStatus;
use super::{
    DriverError, DriverMeta, DriverPointRef, LearnGrid, LearnPoint, PollMode, SyncContext,
    WriteContext,
};

use crate::rest::EngineHandle;

/// Write level used by `LocalIoDriver::write` when delegating to the
/// engine. Level 16 is the lowest-priority driver write (same as BACnet
/// and MQTT), so higher-priority SOX / manual writes override cleanly.
const LOCAL_IO_WRITE_LEVEL: u8 = 16;
/// Duration of engine writes from `LocalIoDriver::write`, in seconds.
const LOCAL_IO_WRITE_DURATION_SECS: f64 = 30.0;

// ── LocalIoChannel ─────────────────────────────────────────

/// A single I/O channel mapped to hardware.
///
/// Carries richer metadata than the base `LocalIoPoint` — direction,
/// channel type, enable state, and cached last-read value. Used by
/// the `LocalIoDriver` for channel-aware learn/sync/write operations.
#[derive(Debug, Clone)]
pub struct LocalIoChannel {
    /// Numeric channel ID (matches engine channel numbering, e.g. 1713).
    pub channel_id: u32,
    /// Human-readable label (e.g. "AI1 10K Thermistor").
    pub label: String,
    /// I/O direction: `"AI"`, `"DI"`, `"AO"`, `"DO"`, `"PWM"`.
    pub direction: String,
    /// Channel kind: `"analog"`, `"digital"`, `"pwm"`.
    pub channel_type: String,
    /// Hardware address string (e.g. `"AIN0"`, `"GPIO60"`, `"I2C2:0x25"`).
    pub address: String,
    /// Whether the channel is enabled for polling.
    pub enabled: bool,
    /// Last value read from hardware (cached by poll cycle).
    pub last_value: f64,
    /// Status of the last read (e.g. `"ok"`, `"fault"`, `"stale"`).
    pub last_status: String,
}

impl LocalIoChannel {
    /// Create a new enabled channel with default values.
    pub fn new(
        channel_id: u32,
        label: impl Into<String>,
        direction: impl Into<String>,
        channel_type: impl Into<String>,
        address: impl Into<String>,
    ) -> Self {
        Self {
            channel_id,
            label: label.into(),
            direction: direction.into(),
            channel_type: channel_type.into(),
            address: address.into(),
            enabled: true,
            last_value: 0.0,
            last_status: "ok".into(),
        }
    }

    /// Returns true if this channel is a writable output.
    pub fn is_output(&self) -> bool {
        matches!(self.direction.as_str(), "DO" | "AO" | "PWM")
    }
}

// ── LocalIoDriver ──────────────────────────────────────────

/// Local I/O driver that wraps the existing HAL for GPIO/ADC/I2C/PWM.
///
/// This driver registers local hardware channels and responds to
/// learn/sync/write requests. Actual hardware reads continue through
/// the engine's existing HAL and poll infrastructure — this driver
/// provides the [`Driver`] trait interface for management, discovery,
/// and status tracking.
pub struct LocalIoDriver {
    id: String,
    status: DriverStatus,
    /// Channel configuration from the engine.
    channels: Vec<LocalIoChannel>,
    /// Optional engine handle. When present, `sync_cur` and `write`
    /// delegate to the engine. When `None`, the driver falls back to
    /// reading / caching values on its internal `channels` list (keeps
    /// existing unit tests functional without an engine).
    engine: Option<EngineHandle>,
}

impl LocalIoDriver {
    /// Create a new local I/O driver with the given instance ID and no
    /// engine backing (test-style: values come from `configure_channels`
    /// / `update_value`).
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: DriverStatus::Pending,
            channels: Vec::new(),
            engine: None,
        }
    }

    /// Create a new local I/O driver backed by the engine (production
    /// usage: `sync_cur` and `write` delegate through `EngineHandle`).
    pub fn with_engine(id: impl Into<String>, engine: EngineHandle) -> Self {
        Self {
            id: id.into(),
            status: DriverStatus::Pending,
            channels: Vec::new(),
            engine: Some(engine),
        }
    }

    /// Add a point using the simple (id, address, kind) form.
    ///
    /// Creates a default `LocalIoChannel` with direction inferred from
    /// the kind (`"Bool"` → `"DI"`, `"Number"` → `"AI"`).
    pub fn add_point(
        &mut self,
        point_id: u32,
        address: impl Into<String>,
        kind: impl Into<String>,
    ) {
        let kind_str = kind.into();
        let addr_str = address.into();
        let direction = if kind_str == "Bool" { "DI" } else { "AI" };
        let channel_type = if kind_str == "Bool" {
            "digital"
        } else {
            "analog"
        };
        self.channels.push(LocalIoChannel::new(
            point_id,
            format!("point_{}", point_id),
            direction,
            channel_type,
            addr_str,
        ));
    }

    /// Configure channels from a pre-built list.
    pub fn configure_channels(&mut self, channels: Vec<LocalIoChannel>) {
        self.channels = channels;
    }

    /// Get the number of registered channels/points.
    pub fn point_count(&self) -> usize {
        self.channels.len()
    }

    /// Get a reference to all configured channels.
    pub fn channels(&self) -> &[LocalIoChannel] {
        &self.channels
    }

    /// Returns `true` iff this driver was constructed with an engine
    /// handle (i.e. `sync_cur` / `write` will delegate to the engine).
    pub fn is_engine_backed(&self) -> bool {
        self.engine.is_some()
    }

    /// Update the cached value for a channel (called by the engine poll loop).
    pub fn update_value(&mut self, channel_id: u32, value: f64, status: &str) {
        if let Some(ch) = self
            .channels
            .iter_mut()
            .find(|c| c.channel_id == channel_id)
        {
            ch.last_value = value;
            ch.last_status = status.to_string();
        }
    }
}

#[async_trait]
impl AsyncDriver for LocalIoDriver {
    fn driver_type(&self) -> &'static str {
        "localIo"
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn status(&self) -> &DriverStatus {
        &self.status
    }

    fn poll_mode(&self) -> PollMode {
        PollMode::Buckets
    }

    async fn open(&mut self) -> Result<DriverMeta, DriverError> {
        self.status = DriverStatus::Ok;
        Ok(DriverMeta {
            firmware_version: Some("1.6.0".into()),
            model: Some("BeagleBone Black".into()),
            ..Default::default()
        })
    }

    async fn close(&mut self) {
        self.status = DriverStatus::Down;
    }

    async fn ping(&mut self) -> Result<DriverMeta, DriverError> {
        // Local I/O is always healthy if open succeeded. When we have an
        // engine handle, a cheap round-trip would catch a dropped actor,
        // but the engine's own supervisor handles that — keep ping free.
        Ok(DriverMeta {
            model: Some("BeagleBone Black".into()),
            ..Default::default()
        })
    }

    async fn learn(&mut self, _path: Option<&str>) -> Result<LearnGrid, DriverError> {
        let grid = self
            .channels
            .iter()
            .map(|ch| LearnPoint {
                name: ch.label.clone(),
                address: ch.address.clone(),
                kind: ch.channel_type.clone(),
                unit: None,
                tags: {
                    let mut t = HashMap::new();
                    t.insert("direction".into(), ch.direction.clone());
                    if !ch.enabled {
                        t.insert("disabled".into(), "true".into());
                    }
                    t
                },
            })
            .collect();
        Ok(grid)
    }

    async fn sync_cur(&mut self, points: &[DriverPointRef], ctx: &mut SyncContext) {
        // Engine-backed path: delegate to engine.read_channel per point.
        if let Some(engine) = self.engine.clone() {
            for p in points {
                match engine.read_channel(p.point_id).await {
                    Ok(cv) => ctx.update_cur_ok(p.point_id, cv.cur),
                    Err(msg) => ctx.update_cur_err(
                        p.point_id,
                        // Engine returns "channel N not found" or similar;
                        // ConfigFault surfaces that up to the caller.
                        DriverError::ConfigFault(msg),
                    ),
                }
            }
            return;
        }

        // Cache-only path (tests / engineless): read from in-memory list.
        for p in points {
            match self.channels.iter().find(|ch| ch.channel_id == p.point_id) {
                Some(ch) if ch.enabled => ctx.update_cur_ok(p.point_id, ch.last_value),
                Some(_) => ctx.update_cur_err(
                    p.point_id,
                    DriverError::ConfigFault("channel disabled".into()),
                ),
                None => ctx.update_cur_err(
                    p.point_id,
                    DriverError::ConfigFault(format!("channel {} not found", p.point_id)),
                ),
            }
        }
    }

    async fn write(&mut self, writes: &[(u32, f64)], ctx: &mut WriteContext) {
        // Engine-backed path: write through `EngineHandle` at level 16.
        if let Some(engine) = self.engine.clone() {
            let who = format!("localIo:{}", self.id);
            for &(id, val) in writes {
                match engine
                    .write_channel(
                        id,
                        Some(val),
                        LOCAL_IO_WRITE_LEVEL,
                        who.clone(),
                        LOCAL_IO_WRITE_DURATION_SECS,
                    )
                    .await
                {
                    Ok(()) => ctx.update_write_ok(id),
                    Err(msg) => ctx.update_write_err(id, DriverError::ConfigFault(msg)),
                }
            }
            return;
        }

        // Cache-only path: validate and update the internal channel list.
        for &(id, val) in writes {
            match self.channels.iter_mut().find(|ch| ch.channel_id == id) {
                Some(ch) if ch.is_output() => {
                    ch.last_value = val;
                    ctx.update_write_ok(id);
                }
                Some(_) => ctx.update_write_err(
                    id,
                    DriverError::ConfigFault("not an output channel".into()),
                ),
                None => ctx.update_write_err(
                    id,
                    DriverError::ConfigFault(format!("channel {} not found", id)),
                ),
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_driver_with_channels() -> LocalIoDriver {
        let mut d = LocalIoDriver::new("test-local");
        d.configure_channels(vec![
            LocalIoChannel::new(1100, "AI1 RTD", "AI", "analog", "AIN0"),
            LocalIoChannel::new(1200, "AI2 Therm", "AI", "analog", "AIN1"),
            LocalIoChannel::new(5000, "DO1 Relay", "DO", "digital", "GPIO60"),
            LocalIoChannel::new(6000, "PWM Fan", "PWM", "pwm", "PWM0:0"),
            {
                let mut ch = LocalIoChannel::new(7000, "AI3 Disabled", "AI", "analog", "AIN2");
                ch.enabled = false;
                ch
            },
        ]);
        d
    }

    #[tokio::test]
    async fn open_close_lifecycle() {
        let mut d = LocalIoDriver::new("lc");
        assert_eq!(*d.status(), DriverStatus::Pending);

        let meta = d.open().await.unwrap();
        assert_eq!(meta.firmware_version, Some("1.6.0".into()));
        assert_eq!(meta.model, Some("BeagleBone Black".into()));
        assert_eq!(*d.status(), DriverStatus::Ok);

        d.close().await;
        assert_eq!(*d.status(), DriverStatus::Down);
    }

    #[tokio::test]
    async fn ping_returns_model() {
        let mut d = LocalIoDriver::new("ping");
        d.open().await.unwrap();
        let meta = d.ping().await.unwrap();
        assert_eq!(meta.model, Some("BeagleBone Black".into()));
    }

    #[test]
    fn driver_type_is_local_io() {
        let d = LocalIoDriver::new("dt");
        assert_eq!(d.driver_type(), "localIo");
    }

    #[tokio::test]
    async fn learn_returns_all_channels() {
        let mut d = make_driver_with_channels();
        let grid = d.learn(None).await.unwrap();
        assert_eq!(grid.len(), 5);
        assert_eq!(grid[0].name, "AI1 RTD");
        assert_eq!(grid[0].address, "AIN0");
        assert_eq!(grid[0].kind, "analog");
        assert_eq!(grid[0].tags.get("direction").unwrap(), "AI");

        // Disabled channel has disabled tag
        assert_eq!(grid[4].tags.get("disabled").unwrap(), "true");
    }

    #[tokio::test]
    async fn sync_cur_enabled_channel() {
        let mut d = make_driver_with_channels();
        d.open().await.unwrap();
        d.update_value(1100, 72.5, "ok");

        let refs = vec![DriverPointRef {
            point_id: 1100,
            address: "AIN0".into(),
        }];
        let mut ctx = SyncContext::new();
        d.sync_cur(&refs, &mut ctx).await;
        let results = ctx.into_results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1100);
        assert!((results[0].1.as_ref().unwrap() - 72.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn sync_cur_disabled_channel() {
        let mut d = make_driver_with_channels();
        d.open().await.unwrap();

        let refs = vec![DriverPointRef {
            point_id: 7000,
            address: "AIN2".into(),
        }];
        let mut ctx = SyncContext::new();
        d.sync_cur(&refs, &mut ctx).await;
        let results = ctx.into_results();
        assert!(results[0].1.is_err());
        assert!(results[0]
            .1
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("disabled"));
    }

    #[tokio::test]
    async fn sync_cur_unknown_channel() {
        let mut d = make_driver_with_channels();
        d.open().await.unwrap();

        let refs = vec![DriverPointRef {
            point_id: 9999,
            address: "X".into(),
        }];
        let mut ctx = SyncContext::new();
        d.sync_cur(&refs, &mut ctx).await;
        let results = ctx.into_results();
        assert!(results[0].1.is_err());
        assert!(results[0]
            .1
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("not found"));
    }

    #[tokio::test]
    async fn write_output_channel() {
        let mut d = make_driver_with_channels();
        d.open().await.unwrap();

        let mut ctx = WriteContext::new();
        d.write(&[(5000, 1.0), (6000, 0.75)], &mut ctx).await;
        let results = ctx.into_results();
        assert_eq!(results.len(), 2);
        assert!(results[0].1.is_ok());
        assert!(results[1].1.is_ok());

        // Verify value was cached
        let ch = d.channels().iter().find(|c| c.channel_id == 5000).unwrap();
        assert!((ch.last_value - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn write_input_channel_fails() {
        let mut d = make_driver_with_channels();
        d.open().await.unwrap();

        let mut ctx = WriteContext::new();
        d.write(&[(1100, 50.0)], &mut ctx).await;
        let results = ctx.into_results();
        assert!(results[0].1.is_err());
        assert!(results[0]
            .1
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("not an output"));
    }

    #[tokio::test]
    async fn write_unknown_channel_fails() {
        let mut d = make_driver_with_channels();
        d.open().await.unwrap();

        let mut ctx = WriteContext::new();
        d.write(&[(9999, 0.0)], &mut ctx).await;
        let results = ctx.into_results();
        assert!(results[0].1.is_err());
    }

    #[test]
    fn poll_mode_is_buckets() {
        let d = LocalIoDriver::new("pm");
        assert_eq!(d.poll_mode(), PollMode::Buckets);
    }

    #[test]
    fn point_count_tracks_channels() {
        let mut d = LocalIoDriver::new("pc");
        assert_eq!(d.point_count(), 0);
        d.add_point(1, "AIN0", "Number");
        d.add_point(2, "GPIO60", "Bool");
        assert_eq!(d.point_count(), 2);
    }

    #[test]
    fn add_point_infers_direction() {
        let mut d = LocalIoDriver::new("inf");
        d.add_point(100, "AIN0", "Number");
        d.add_point(200, "GPIO5", "Bool");

        assert_eq!(d.channels()[0].direction, "AI");
        assert_eq!(d.channels()[0].channel_type, "analog");
        assert_eq!(d.channels()[1].direction, "DI");
        assert_eq!(d.channels()[1].channel_type, "digital");
    }

    #[test]
    fn update_value_updates_cache() {
        let mut d = make_driver_with_channels();
        d.update_value(1100, 85.3, "ok");
        let ch = d.channels().iter().find(|c| c.channel_id == 1100).unwrap();
        assert!((ch.last_value - 85.3).abs() < f64::EPSILON);
        assert_eq!(ch.last_status, "ok");
    }

    #[test]
    fn local_io_channel_is_output() {
        assert!(LocalIoChannel::new(1, "x", "DO", "digital", "G").is_output());
        assert!(LocalIoChannel::new(2, "x", "AO", "analog", "A").is_output());
        assert!(LocalIoChannel::new(3, "x", "PWM", "pwm", "P").is_output());
        assert!(!LocalIoChannel::new(4, "x", "AI", "analog", "A").is_output());
        assert!(!LocalIoChannel::new(5, "x", "DI", "digital", "G").is_output());
    }

    // ── Engine-backed delegation tests (Phase 12.0F) ──────

    /// Spawn a mock engine-cmd responder that answers `ReadChannel` with
    /// the channel id cast to f64 and `WriteChannel` with `Ok(())`, so
    /// the test can validate delegation without a full engine.
    fn spawn_mock_engine() -> EngineHandle {
        use crate::rest::{ChannelValue, EngineCmd};
        use tokio::sync::mpsc;

        let (tx, mut rx) = mpsc::channel::<EngineCmd>(16);
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    EngineCmd::ReadChannel { channel, reply } => {
                        let _ = reply.send(Ok(ChannelValue {
                            channel,
                            status: "ok".into(),
                            raw: f64::from(channel),
                            cur: f64::from(channel) + 0.5,
                        }));
                    }
                    EngineCmd::WriteChannel { reply, .. } => {
                        let _ = reply.send(Ok(()));
                    }
                    _ => {} // ignore others
                }
            }
        });
        EngineHandle::new(tx)
    }

    #[test]
    fn is_engine_backed_reports_constructor_choice() {
        let d1 = LocalIoDriver::new("no-eng");
        assert!(!d1.is_engine_backed());

        // Spawn on a runtime so we can build an EngineHandle.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let d2 = rt.block_on(async {
            let eh = spawn_mock_engine();
            LocalIoDriver::with_engine("with-eng", eh)
        });
        assert!(d2.is_engine_backed());
    }

    #[tokio::test]
    async fn sync_cur_engine_backed_uses_read_channel() {
        let eh = spawn_mock_engine();
        let mut d = LocalIoDriver::with_engine("eng-sync", eh);
        d.open().await.unwrap();

        let refs = vec![
            DriverPointRef {
                point_id: 100,
                address: "AIN0".into(),
            },
            DriverPointRef {
                point_id: 42,
                address: "AIN1".into(),
            },
        ];
        let mut ctx = SyncContext::new();
        d.sync_cur(&refs, &mut ctx).await;
        let results = ctx.into_results();

        // Mock returns cur = point_id + 0.5
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 100);
        assert!((results[0].1.as_ref().unwrap() - 100.5).abs() < f64::EPSILON);
        assert_eq!(results[1].0, 42);
        assert!((results[1].1.as_ref().unwrap() - 42.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn write_engine_backed_uses_write_channel() {
        let eh = spawn_mock_engine();
        let mut d = LocalIoDriver::with_engine("eng-write", eh);
        d.open().await.unwrap();

        let mut ctx = WriteContext::new();
        d.write(&[(500, 72.5), (600, 1.0)], &mut ctx).await;
        let results = ctx.into_results();
        assert_eq!(results.len(), 2);
        assert!(results[0].1.is_ok());
        assert!(results[1].1.is_ok());
    }
}
