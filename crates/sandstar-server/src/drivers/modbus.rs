//! Modbus TCP/RTU driver (stub).
//!
//! Placeholder for future Modbus protocol support. Will implement
//! function codes 01-06, 15-16 for coils and holding registers.
//! Currently returns `DriverError::NotSupported` for I/O operations.

use super::{Driver, DriverError, DriverMeta, DriverPointRef, DriverStatus, LearnGrid, PollMode};

/// Modbus TCP/RTU driver (stub — not yet connected to hardware).
///
/// Will support Modbus function codes 01-06, 15-16 for coils and
/// holding registers over TCP or RTU (serial RS-485).
pub struct ModbusDriver {
    id: String,
    status: DriverStatus,
    /// Target host address (e.g. "192.168.1.100").
    host: String,
    /// TCP port (default 502).
    port: u16,
    /// Modbus slave/unit ID (1-247).
    slave_id: u8,
}

impl ModbusDriver {
    /// Create a new Modbus driver stub.
    pub fn new(id: impl Into<String>, host: impl Into<String>, port: u16, slave_id: u8) -> Self {
        Self {
            id: id.into(),
            status: DriverStatus::Pending,
            host: host.into(),
            port,
            slave_id,
        }
    }

    /// Get the configured host address.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Get the configured TCP port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Get the configured slave ID.
    pub fn slave_id(&self) -> u8 {
        self.slave_id
    }
}

impl Driver for ModbusDriver {
    fn driver_type(&self) -> &'static str {
        "modbus"
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn status(&self) -> &DriverStatus {
        &self.status
    }

    fn open(&mut self) -> Result<DriverMeta, DriverError> {
        // Stub: mark as pending until real TCP connection is implemented.
        self.status = DriverStatus::Fault("not implemented".into());
        Ok(DriverMeta {
            model: Some(format!("Modbus TCP {}:{} unit {}", self.host, self.port, self.slave_id)),
            ..Default::default()
        })
    }

    fn close(&mut self) {
        self.status = DriverStatus::Down;
    }

    fn ping(&mut self) -> Result<DriverMeta, DriverError> {
        Err(DriverError::NotSupported("modbus ping"))
    }

    fn learn(&mut self, _path: Option<&str>) -> Result<LearnGrid, DriverError> {
        Err(DriverError::NotSupported("modbus learn"))
    }

    fn sync_cur(&mut self, _points: &[DriverPointRef]) -> Vec<(u32, Result<f64, DriverError>)> {
        // Stub: all reads return not-supported.
        Vec::new()
    }

    fn write(&mut self, _writes: &[(u32, f64)]) -> Vec<(u32, Result<(), DriverError>)> {
        Vec::new()
    }

    fn poll_mode(&self) -> PollMode {
        PollMode::Buckets
    }
}

// ── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modbus_lifecycle() {
        let mut d = ModbusDriver::new("mb-1", "192.168.1.100", 502, 1);
        assert_eq!(*d.status(), DriverStatus::Pending);
        assert_eq!(d.driver_type(), "modbus");

        let meta = d.open().unwrap();
        assert!(meta.model.unwrap().contains("Modbus TCP"));
        // Status is fault (not implemented)
        assert!(matches!(d.status(), DriverStatus::Fault(_)));

        d.close();
        assert_eq!(*d.status(), DriverStatus::Down);
    }

    #[test]
    fn modbus_learn_not_supported() {
        let mut d = ModbusDriver::new("mb-2", "10.0.0.1", 502, 5);
        let result = d.learn(None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not supported"));
    }

    #[test]
    fn modbus_ping_not_supported() {
        let mut d = ModbusDriver::new("mb-3", "10.0.0.1", 502, 1);
        assert!(d.ping().is_err());
    }

    #[test]
    fn modbus_accessors() {
        let d = ModbusDriver::new("mb-4", "10.0.0.1", 503, 7);
        assert_eq!(d.host(), "10.0.0.1");
        assert_eq!(d.port(), 503);
        assert_eq!(d.slave_id(), 7);
    }

    #[test]
    fn modbus_sync_cur_empty() {
        let mut d = ModbusDriver::new("mb-5", "x", 502, 1);
        let results = d.sync_cur(&[]);
        assert!(results.is_empty());
    }

    #[test]
    fn modbus_write_empty() {
        let mut d = ModbusDriver::new("mb-6", "x", 502, 1);
        let results = d.write(&[]);
        assert!(results.is_empty());
    }
}
