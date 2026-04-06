//! MQTT pub/sub driver (stub).
//!
//! Placeholder for future MQTT broker integration. Will support
//! publish/subscribe on configurable topics for telemetry and control.
//! Currently returns `DriverError::NotSupported` for I/O operations.

use super::{Driver, DriverError, DriverMeta, DriverPointRef, DriverStatus, LearnGrid, PollMode};

/// MQTT pub/sub driver (stub — not yet connected to broker).
///
/// Will support MQTT v3.1.1/v5 with TLS, topic-based point mapping,
/// JSON/Haystack payload encoding, and QoS configuration.
pub struct MqttDriver {
    id: String,
    status: DriverStatus,
    /// Broker URL (e.g. "mqtt://broker:1883" or "mqtts://broker:8883").
    broker_url: String,
}

impl MqttDriver {
    /// Create a new MQTT driver stub.
    pub fn new(id: impl Into<String>, broker_url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: DriverStatus::Pending,
            broker_url: broker_url.into(),
        }
    }

    /// Get the configured broker URL.
    pub fn broker_url(&self) -> &str {
        &self.broker_url
    }
}

impl Driver for MqttDriver {
    fn driver_type(&self) -> &'static str {
        "mqtt"
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn status(&self) -> &DriverStatus {
        &self.status
    }

    fn open(&mut self) -> Result<DriverMeta, DriverError> {
        self.status = DriverStatus::Fault("not implemented".into());
        Ok(DriverMeta {
            model: Some(format!("MQTT {}", self.broker_url)),
            ..Default::default()
        })
    }

    fn close(&mut self) {
        self.status = DriverStatus::Down;
    }

    fn ping(&mut self) -> Result<DriverMeta, DriverError> {
        Err(DriverError::NotSupported("mqtt ping"))
    }

    fn learn(&mut self, _path: Option<&str>) -> Result<LearnGrid, DriverError> {
        Err(DriverError::NotSupported("mqtt learn"))
    }

    fn sync_cur(&mut self, _points: &[DriverPointRef]) -> Vec<(u32, Result<f64, DriverError>)> {
        Vec::new()
    }

    fn write(&mut self, _writes: &[(u32, f64)]) -> Vec<(u32, Result<(), DriverError>)> {
        Vec::new()
    }

    fn poll_mode(&self) -> PollMode {
        // MQTT is event-driven, not polled. Manual mode allows the driver
        // to push updates when messages arrive from the broker.
        PollMode::Manual
    }
}

// ── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mqtt_lifecycle() {
        let mut d = MqttDriver::new("mq-1", "mqtt://broker:1883");
        assert_eq!(*d.status(), DriverStatus::Pending);
        assert_eq!(d.driver_type(), "mqtt");

        let meta = d.open().unwrap();
        assert!(meta.model.unwrap().contains("MQTT"));
        assert!(matches!(d.status(), DriverStatus::Fault(_)));

        d.close();
        assert_eq!(*d.status(), DriverStatus::Down);
    }

    #[test]
    fn mqtt_learn_not_supported() {
        let mut d = MqttDriver::new("mq-2", "mqtt://x:1883");
        assert!(d.learn(None).is_err());
    }

    #[test]
    fn mqtt_ping_not_supported() {
        let mut d = MqttDriver::new("mq-3", "mqtt://x:1883");
        assert!(d.ping().is_err());
    }

    #[test]
    fn mqtt_poll_mode_is_manual() {
        let d = MqttDriver::new("mq-4", "mqtt://x:1883");
        assert_eq!(d.poll_mode(), PollMode::Manual);
    }

    #[test]
    fn mqtt_broker_url_accessor() {
        let d = MqttDriver::new("mq-5", "mqtts://secure:8883");
        assert_eq!(d.broker_url(), "mqtts://secure:8883");
    }

    #[test]
    fn mqtt_sync_and_write_empty() {
        let mut d = MqttDriver::new("mq-6", "mqtt://x:1883");
        assert!(d.sync_cur(&[]).is_empty());
        assert!(d.write(&[]).is_empty());
    }
}
