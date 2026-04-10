//! Local I/O driver for BeagleBone hardware.
//!
//! Reads ADC inputs, digital inputs, and writes to digital/PWM outputs
//! via the existing sandstar-hal abstraction. This bridges the Driver
//! Framework v2 with the existing HAL layer.
//!
//! The engine's existing HAL does the real hardware I/O — this driver
//! provides the [`Driver`] trait interface for management, discovery,
//! status tracking, and future integration with the DriverManager.

use std::collections::HashMap;

use super::DriverStatus;
use super::{Driver, DriverError, DriverMeta, DriverPointRef, LearnGrid, LearnPoint, PollMode};

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
}

impl LocalIoDriver {
    /// Create a new local I/O driver with the given instance ID.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: DriverStatus::Pending,
            channels: Vec::new(),
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
            firmware_version: Some("1.6.0".into()),
            model: Some("BeagleBone Black".into()),
            ..Default::default()
        })
    }

    fn close(&mut self) {
        self.status = DriverStatus::Down;
    }

    fn ping(&mut self) -> Result<DriverMeta, DriverError> {
        // Local I/O is always healthy if open succeeded.
        Ok(DriverMeta {
            model: Some("BeagleBone Black".into()),
            ..Default::default()
        })
    }

    fn learn(&mut self, _path: Option<&str>) -> Result<LearnGrid, DriverError> {
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

    fn sync_cur(&mut self, points: &[DriverPointRef]) -> Vec<(u32, Result<f64, DriverError>)> {
        points
            .iter()
            .map(
                |p| match self.channels.iter().find(|ch| ch.channel_id == p.point_id) {
                    Some(ch) if ch.enabled => (p.point_id, Ok(ch.last_value)),
                    Some(_) => (
                        p.point_id,
                        Err(DriverError::ConfigFault("channel disabled".into())),
                    ),
                    None => (
                        p.point_id,
                        Err(DriverError::ConfigFault(format!(
                            "channel {} not found",
                            p.point_id
                        ))),
                    ),
                },
            )
            .collect()
    }

    fn write(&mut self, writes: &[(u32, f64)]) -> Vec<(u32, Result<(), DriverError>)> {
        writes
            .iter()
            .map(|&(id, val)| {
                match self.channels.iter_mut().find(|ch| ch.channel_id == id) {
                    Some(ch) if ch.is_output() => {
                        // Cache the written value locally.
                        ch.last_value = val;
                        (id, Ok(()))
                    }
                    Some(_) => (
                        id,
                        Err(DriverError::ConfigFault("not an output channel".into())),
                    ),
                    None => (
                        id,
                        Err(DriverError::ConfigFault(format!(
                            "channel {} not found",
                            id
                        ))),
                    ),
                }
            })
            .collect()
    }

    fn poll_mode(&self) -> PollMode {
        PollMode::Buckets
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

    #[test]
    fn open_close_lifecycle() {
        let mut d = LocalIoDriver::new("lc");
        assert_eq!(*d.status(), DriverStatus::Pending);

        let meta = d.open().unwrap();
        assert_eq!(meta.firmware_version, Some("1.6.0".into()));
        assert_eq!(meta.model, Some("BeagleBone Black".into()));
        assert_eq!(*d.status(), DriverStatus::Ok);

        d.close();
        assert_eq!(*d.status(), DriverStatus::Down);
    }

    #[test]
    fn ping_returns_model() {
        let mut d = LocalIoDriver::new("ping");
        d.open().unwrap();
        let meta = d.ping().unwrap();
        assert_eq!(meta.model, Some("BeagleBone Black".into()));
    }

    #[test]
    fn driver_type_is_local_io() {
        let d = LocalIoDriver::new("dt");
        assert_eq!(d.driver_type(), "localIo");
    }

    #[test]
    fn learn_returns_all_channels() {
        let mut d = make_driver_with_channels();
        let grid = d.learn(None).unwrap();
        assert_eq!(grid.len(), 5);
        assert_eq!(grid[0].name, "AI1 RTD");
        assert_eq!(grid[0].address, "AIN0");
        assert_eq!(grid[0].kind, "analog");
        assert_eq!(grid[0].tags.get("direction").unwrap(), "AI");

        // Disabled channel has disabled tag
        assert_eq!(grid[4].tags.get("disabled").unwrap(), "true");
    }

    #[test]
    fn sync_cur_enabled_channel() {
        let mut d = make_driver_with_channels();
        d.open().unwrap();
        d.update_value(1100, 72.5, "ok");

        let refs = vec![DriverPointRef {
            point_id: 1100,
            address: "AIN0".into(),
        }];
        let results = d.sync_cur(&refs);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1100);
        assert!((results[0].1.as_ref().unwrap() - 72.5).abs() < f64::EPSILON);
    }

    #[test]
    fn sync_cur_disabled_channel() {
        let mut d = make_driver_with_channels();
        d.open().unwrap();

        let refs = vec![DriverPointRef {
            point_id: 7000,
            address: "AIN2".into(),
        }];
        let results = d.sync_cur(&refs);
        assert!(results[0].1.is_err());
        assert!(results[0]
            .1
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("disabled"));
    }

    #[test]
    fn sync_cur_unknown_channel() {
        let mut d = make_driver_with_channels();
        d.open().unwrap();

        let refs = vec![DriverPointRef {
            point_id: 9999,
            address: "X".into(),
        }];
        let results = d.sync_cur(&refs);
        assert!(results[0].1.is_err());
        assert!(results[0]
            .1
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("not found"));
    }

    #[test]
    fn write_output_channel() {
        let mut d = make_driver_with_channels();
        d.open().unwrap();

        let results = d.write(&[(5000, 1.0), (6000, 0.75)]);
        assert_eq!(results.len(), 2);
        assert!(results[0].1.is_ok());
        assert!(results[1].1.is_ok());

        // Verify value was cached
        let ch = d.channels().iter().find(|c| c.channel_id == 5000).unwrap();
        assert!((ch.last_value - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn write_input_channel_fails() {
        let mut d = make_driver_with_channels();
        d.open().unwrap();

        let results = d.write(&[(1100, 50.0)]);
        assert!(results[0].1.is_err());
        assert!(results[0]
            .1
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("not an output"));
    }

    #[test]
    fn write_unknown_channel_fails() {
        let mut d = make_driver_with_channels();
        d.open().unwrap();

        let results = d.write(&[(9999, 0.0)]);
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
}
