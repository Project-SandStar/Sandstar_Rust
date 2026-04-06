//! BACnet IP driver (stub).
//!
//! Placeholder for future BACnet IP/MSTP protocol support. Will
//! implement ReadProperty, WriteProperty, and COV subscriptions.
//! Currently returns `DriverError::NotSupported` for I/O operations.

use async_trait::async_trait;

use super::async_driver::AsyncDriver;
use super::{DriverError, DriverMeta, DriverPointRef, DriverStatus, LearnGrid, PollMode};

/// BACnet IP driver (stub — not yet connected to hardware).
///
/// Will support BACnet/IP with ReadProperty, WriteProperty,
/// SubscribeCOV, and WhoIs/IAm discovery.
pub struct BacnetDriver {
    id: String,
    status: DriverStatus,
}

impl BacnetDriver {
    /// Create a new BACnet driver stub.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: DriverStatus::Pending,
        }
    }
}

#[async_trait]
impl AsyncDriver for BacnetDriver {
    fn driver_type(&self) -> &'static str {
        "bacnet"
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn status(&self) -> &DriverStatus {
        &self.status
    }

    async fn open(&mut self) -> Result<DriverMeta, DriverError> {
        self.status = DriverStatus::Fault("not implemented".into());
        Ok(DriverMeta {
            model: Some("BACnet/IP".into()),
            ..Default::default()
        })
    }

    async fn close(&mut self) {
        self.status = DriverStatus::Down;
    }

    async fn ping(&mut self) -> Result<DriverMeta, DriverError> {
        Err(DriverError::NotSupported("bacnet ping"))
    }

    async fn learn(&mut self, _path: Option<&str>) -> Result<LearnGrid, DriverError> {
        Err(DriverError::NotSupported("bacnet learn"))
    }

    async fn sync_cur(
        &mut self,
        _points: &[DriverPointRef],
    ) -> Vec<(u32, Result<f64, DriverError>)> {
        Vec::new()
    }

    async fn write(
        &mut self,
        _writes: &[(u32, f64)],
    ) -> Vec<(u32, Result<(), DriverError>)> {
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

    #[tokio::test]
    async fn bacnet_lifecycle() {
        let mut d = BacnetDriver::new("bac-1");
        assert_eq!(*d.status(), DriverStatus::Pending);
        assert_eq!(d.driver_type(), "bacnet");

        let meta = d.open().await.unwrap();
        assert_eq!(meta.model, Some("BACnet/IP".into()));
        assert!(matches!(d.status(), DriverStatus::Fault(_)));

        d.close().await;
        assert_eq!(*d.status(), DriverStatus::Down);
    }

    #[tokio::test]
    async fn bacnet_learn_not_supported() {
        let mut d = BacnetDriver::new("bac-2");
        assert!(d.learn(None).await.is_err());
    }

    #[tokio::test]
    async fn bacnet_ping_not_supported() {
        let mut d = BacnetDriver::new("bac-3");
        assert!(d.ping().await.is_err());
    }

    #[tokio::test]
    async fn bacnet_sync_and_write_empty() {
        let mut d = BacnetDriver::new("bac-4");
        assert!(d.sync_cur(&[]).await.is_empty());
        assert!(d.write(&[]).await.is_empty());
    }
}
