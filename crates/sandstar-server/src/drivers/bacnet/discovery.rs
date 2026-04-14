//! BACnet device discovery via Who-Is / I-Am.
//!
//! Phase B2 will flesh out `collect_i_am` with a real UDP listener.
//! This skeleton provides the types and function signatures that the
//! rest of the driver module depends on.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::UdpSocket;

use super::{BacnetError, DeviceInfo};

// ── DeviceRegistry ─────────────────────────────────────────

/// Registry of discovered BACnet devices indexed by device instance number.
///
/// The registry is keyed by BACnet device instance (a 22-bit integer,
/// range 0–4 194 302). Inserting the same instance a second time silently
/// overwrites the previous entry, which is the correct behaviour when a
/// device re-advertises with an updated address.
#[derive(Debug, Default)]
pub struct DeviceRegistry {
    devices: HashMap<u32, DeviceInfo>,
}

impl DeviceRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or overwrite) a device.
    pub fn insert(&mut self, device: DeviceInfo) {
        self.devices.insert(device.instance, device);
    }

    /// Insert multiple devices in one call.
    pub fn bulk_insert(&mut self, devices: Vec<DeviceInfo>) {
        for d in devices {
            self.insert(d);
        }
    }

    /// Look up a device by its instance number.
    pub fn get(&self, instance: u32) -> Option<&DeviceInfo> {
        self.devices.get(&instance)
    }

    /// Return references to all known devices (order unspecified).
    pub fn all(&self) -> Vec<&DeviceInfo> {
        self.devices.values().collect()
    }

    /// Return the number of devices in the registry.
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Return `true` if the registry contains no devices.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

// ── Discovery helpers ──────────────────────────────────────

/// Send a Who-Is broadcast on the given socket.
///
/// The request is encoded with no instance-range restriction (covers all
/// device instances). The caller is responsible for binding the socket
/// and enabling `SO_BROADCAST` before calling this function.
pub async fn send_who_is(socket: &UdpSocket, broadcast: SocketAddr) -> Result<(), BacnetError> {
    let frame = super::frame::encode_who_is(None, None);
    socket.send_to(&frame, broadcast).await?;
    Ok(())
}

/// Collect I-Am responses for up to `timeout`.
///
/// Listens on `socket` for incoming UDP datagrams and attempts to decode
/// each one as a BACnet I-Am service request. Packets that cannot be
/// decoded (wrong protocol, malformed APDU) are silently ignored.
///
/// # Phase B2 note
/// The current implementation returns an empty vector because
/// `frame::decode_packet` is a stub that always returns an error.
/// Phase B2 will implement the full APDU decoder, at which point this
/// function will automatically start producing results without any
/// structural changes.
pub async fn collect_i_am(socket: &UdpSocket, timeout: Duration) -> Vec<DeviceInfo> {
    let mut devices = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = [0u8; 1500];

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, addr))) => {
                if let Ok((
                    _,
                    super::frame::Apdu::IAm {
                        device_instance,
                        max_apdu,
                        segmentation,
                        vendor_id,
                    },
                )) = super::frame::decode_packet(&buf[..n])
                {
                    devices.push(DeviceInfo {
                        instance: device_instance,
                        addr,
                        max_apdu,
                        vendor_id,
                        segmentation,
                    });
                }
            }
            // Timeout or socket error — stop listening.
            _ => break,
        }
    }

    devices
}

// ── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    fn make_device(instance: u32, port: u16) -> DeviceInfo {
        DeviceInfo {
            instance,
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, instance as u8)), port),
            max_apdu: 1476,
            vendor_id: 8,
            segmentation: 0,
        }
    }

    #[test]
    fn registry_starts_empty() {
        let r = DeviceRegistry::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn insert_and_get_works() {
        let mut r = DeviceRegistry::new();
        r.insert(make_device(42, 47808));
        assert_eq!(r.len(), 1);
        assert!(!r.is_empty());
        let d = r.get(42).expect("device 42 should be present");
        assert_eq!(d.instance, 42);
        assert_eq!(d.max_apdu, 1476);
    }

    #[test]
    fn get_missing_returns_none() {
        let r = DeviceRegistry::new();
        assert!(r.get(999).is_none());
    }

    #[test]
    fn bulk_insert_adds_multiple() {
        let mut r = DeviceRegistry::new();
        r.bulk_insert(vec![
            make_device(1, 47808),
            make_device(2, 47808),
            make_device(3, 47808),
        ]);
        assert_eq!(r.len(), 3);
        assert!(r.get(1).is_some());
        assert!(r.get(2).is_some());
        assert!(r.get(3).is_some());
    }

    #[test]
    fn duplicate_insert_overwrites() {
        let mut r = DeviceRegistry::new();
        r.insert(make_device(10, 47808));
        // Overwrite with a different port to confirm replacement.
        let updated = DeviceInfo {
            instance: 10,
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 9999),
            max_apdu: 480,
            vendor_id: 15,
            segmentation: 3,
        };
        r.insert(updated);
        assert_eq!(r.len(), 1, "should still be 1 entry after overwrite");
        let d = r.get(10).unwrap();
        assert_eq!(d.addr.port(), 9999, "should reflect the updated entry");
        assert_eq!(d.max_apdu, 480);
    }

    #[test]
    fn all_returns_all_devices() {
        let mut r = DeviceRegistry::new();
        r.bulk_insert(vec![make_device(7, 47808), make_device(8, 47808)]);
        let all = r.all();
        assert_eq!(all.len(), 2);
    }
}
