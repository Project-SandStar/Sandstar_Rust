//! BACnet/IP driver.
//!
//! Implements BACnet/IP device discovery (Who-Is / I-Am) and will support
//! ReadProperty, WriteProperty, and COV subscriptions in later phases.
//!
//! ## Module layout
//!
//! | Submodule | Contents |
//! |-----------|----------|
//! | [`frame`] | BVLC + NPDU + APDU codec (stub for Phase B-frame) |
//! | [`object`] | `BacnetObject` — point-to-BACnet-object mapping |
//! | [`value`] | Application-tagged value decoding |
//! | [`discovery`] | `DeviceRegistry`, Who-Is / I-Am helpers |
//! | [`transaction`] | `TransactionTable` — invoke-ID allocation & dispatch |

pub mod discovery;
pub mod frame;
pub mod object;
pub mod transaction;
pub mod value;

use std::collections::HashMap;
use std::net::SocketAddr;

use async_trait::async_trait;
use tokio::net::UdpSocket;

use super::async_driver::AsyncDriver;
use super::{DriverError, DriverMeta, DriverPointRef, DriverStatus, LearnGrid, PollMode};

// Re-export DeviceRegistry for use in tests and Phase B3.
pub use discovery::DeviceRegistry;

// ── BacnetError ────────────────────────────────────────────

/// Errors specific to the BACnet/IP driver.
///
/// These complement the driver-framework [`DriverError`]. Use the
/// [`From<BacnetError> for DriverError`] impl to convert when returning
/// from [`AsyncDriver`] methods.
#[derive(Debug, thiserror::Error)]
pub enum BacnetError {
    #[error("malformed frame: {0}")]
    MalformedFrame(String),

    #[error("unsupported encoding: {0}")]
    UnsupportedEncoding(String),

    #[error("property not found")]
    PropertyNotFound,

    #[error("device not in registry: {0}")]
    DeviceNotFound(u32),

    #[error("transaction timeout after {0} retries")]
    Timeout(u32),

    #[error("remote error class={class} code={code}")]
    RemoteError { class: u32, code: u32 },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<BacnetError> for super::DriverError {
    fn from(e: BacnetError) -> Self {
        super::DriverError::CommFault(e.to_string())
    }
}

// ── DeviceInfo ─────────────────────────────────────────────

/// A discovered BACnet device.
///
/// Populated by [`discovery::collect_i_am`] in response to a Who-Is
/// broadcast. The driver stores these in its `device_registry` so that
/// per-object poll requests can be addressed to the correct UDP endpoint.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// BACnet device instance number (22-bit, range 0–4 194 302).
    pub instance: u32,
    /// UDP address of the device on the BACnet/IP network.
    pub addr: SocketAddr,
    /// Maximum APDU size accepted by the device (typically 480 or 1476).
    pub max_apdu: u16,
    /// BACnet vendor ID registered with ASHRAE.
    pub vendor_id: u16,
    /// Segmentation support byte (0 = both, 1 = transmit, 2 = receive, 3 = none).
    pub segmentation: u8,
}

// ── BacnetConfig ───────────────────────────────────────────

/// Top-level BACnet driver configuration (deserialized from TOML/JSON).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BacnetConfig {
    /// Unique driver instance identifier.
    pub id: String,
    /// UDP port number. Defaults to 47808 (0xBAC0) when `None`.
    pub port: Option<u16>,
    /// Broadcast address string (e.g. `"255.255.255.255"`).
    /// Defaults to `"255.255.255.255"` when `None`.
    pub broadcast: Option<String>,
    /// List of BACnet objects to poll.
    pub objects: Vec<BacnetObjectConfig>,
}

/// Configuration for a single BACnet object to poll.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BacnetObjectConfig {
    /// Sandstar point ID this object maps to.
    pub point_id: u32,
    /// BACnet device instance that owns this object.
    pub device_id: u32,
    /// BACnet object type (0 = Analog Input, 1 = Analog Output, …).
    pub object_type: u16,
    /// BACnet object instance number.
    pub instance: u32,
    /// Optional engineering-unit string.
    pub unit: Option<String>,
    /// Multiplicative scale factor applied to the raw value. Defaults to 1.0.
    pub scale: Option<f64>,
    /// Additive offset applied after `scale`. Defaults to 0.0.
    pub offset: Option<f64>,
}

// ── Default port constant ──────────────────────────────────

/// Standard BACnet/IP UDP port number (0xBAC0).
pub const DEFAULT_BACNET_PORT: u16 = 47808;

// ── BacnetDriver ───────────────────────────────────────────

/// BACnet/IP driver.
///
/// Manages a UDP socket bound to the BACnet port, maintains a registry of
/// discovered devices, and tracks the BACnet objects that map to Sandstar
/// point IDs.
pub struct BacnetDriver {
    id: String,
    status: DriverStatus,
    port: u16,
    broadcast_addr: String,
    socket: Option<UdpSocket>,
    device_registry: discovery::DeviceRegistry,
    /// Mapping from Sandstar `point_id` to BACnet object descriptor.
    objects: HashMap<u32, object::BacnetObject>,
    /// Monotonically-increasing invoke ID counter — allocated by Phase B3 transaction logic.
    #[allow(dead_code)]
    next_invoke_id: u8,
    /// How long to wait for I-Am responses after sending Who-Is.
    /// Default: 2 seconds. Set to 50–200 ms in tests.
    discovery_timeout: std::time::Duration,
    /// Port to send Who-Is broadcast to (defaults to `self.port`).
    /// In tests, set this to the mock device's port while `port` is 0
    /// (letting the OS assign an ephemeral bind port).
    broadcast_port: u16,
}

impl BacnetDriver {
    /// Create a new driver with an explicit broadcast address and port.
    ///
    /// The driver starts in [`DriverStatus::Pending`] and does not bind the
    /// UDP socket until [`AsyncDriver::open`] is called.
    pub fn new(id: impl Into<String>, broadcast: impl Into<String>, port: u16) -> Self {
        Self {
            id: id.into(),
            status: DriverStatus::Pending,
            broadcast_port: port,
            port,
            broadcast_addr: broadcast.into(),
            socket: None,
            device_registry: discovery::DeviceRegistry::new(),
            objects: HashMap::new(),
            next_invoke_id: 0,
            discovery_timeout: std::time::Duration::from_secs(2),
        }
    }

    /// Override the discovery timeout (useful in tests to speed things up).
    pub fn with_discovery_timeout(mut self, t: std::time::Duration) -> Self {
        self.discovery_timeout = t;
        self
    }

    /// Override the broadcast port (useful in tests where the mock listens on
    /// a non-standard port while `port` is 0 for an OS-assigned bind port).
    pub fn with_broadcast_port(mut self, port: u16) -> Self {
        self.broadcast_port = port;
        self
    }

    /// Register a BACnet object that should be polled for a given point ID.
    pub fn add_object(&mut self, point_id: u32, obj: object::BacnetObject) {
        self.objects.insert(point_id, obj);
    }

    /// Construct a driver from a deserialized [`BacnetConfig`].
    pub fn from_config(config: BacnetConfig) -> Self {
        let port = config.port.unwrap_or(DEFAULT_BACNET_PORT);
        let broadcast = config.broadcast.unwrap_or_else(|| "255.255.255.255".into());
        let mut driver = Self::new(config.id, broadcast, port);
        // broadcast_port tracks the configured port, not the bind port.
        driver.broadcast_port = port;
        for obj_cfg in config.objects {
            driver.add_object(
                obj_cfg.point_id,
                object::BacnetObject {
                    device_id: obj_cfg.device_id,
                    object_type: obj_cfg.object_type,
                    instance: obj_cfg.instance,
                    scale: obj_cfg.scale.unwrap_or(1.0),
                    offset: obj_cfg.offset.unwrap_or(0.0),
                    unit: obj_cfg.unit,
                },
            );
        }
        driver
    }

    /// Return a reference to the object map (used in tests and Phase B3).
    pub fn objects(&self) -> &HashMap<u32, object::BacnetObject> {
        &self.objects
    }

    /// Return a reference to the device registry (used in tests and Phase B2).
    pub fn device_registry(&self) -> &discovery::DeviceRegistry {
        &self.device_registry
    }
}

// ── AsyncDriver impl ───────────────────────────────────────

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

    /// Bind the UDP socket, broadcast a Who-Is, and collect I-Am responses
    /// for [`Self::discovery_timeout`] before returning.
    ///
    /// The discovery window is intentionally short in tests (50–200 ms) and
    /// the default production value is 2 s.  `open()` always succeeds even
    /// when no devices respond — the registry simply stays empty.
    async fn open(&mut self) -> Result<DriverMeta, DriverError> {
        // 1. Bind UDP socket (port 0 = OS-assigned ephemeral, useful in tests).
        let socket = UdpSocket::bind(format!("0.0.0.0:{}", self.port))
            .await
            .map_err(|e| DriverError::CommFault(format!("bacnet bind: {e}")))?;
        socket
            .set_broadcast(true)
            .map_err(|e| DriverError::CommFault(format!("bacnet set_broadcast: {e}")))?;

        // 2. Send Who-Is broadcast.
        let bcast: SocketAddr = format!("{}:{}", self.broadcast_addr, self.broadcast_port)
            .parse()
            .map_err(|e: std::net::AddrParseError| {
                DriverError::ConfigFault(format!("invalid broadcast addr: {e}"))
            })?;

        if let Err(e) = discovery::send_who_is(&socket, bcast).await {
            tracing::warn!(driver = %self.id, "bacnet who-is send failed: {e}");
            // Non-fatal: we still listen for any I-Am packets that arrive.
        }

        // 3. Collect I-Am responses during the discovery window.
        let devices = discovery::collect_i_am(&socket, self.discovery_timeout).await;
        let n = devices.len();
        tracing::info!(driver = %self.id, devices = n, "BACnet discovery complete");

        // 4. Update registry.
        self.device_registry.bulk_insert(devices);

        self.socket = Some(socket);
        self.status = DriverStatus::Ok;
        Ok(DriverMeta {
            model: Some(format!(
                "BACnet/IP port={} ({} device{})",
                self.port,
                n,
                if n == 1 { "" } else { "s" }
            )),
            ..Default::default()
        })
    }

    async fn close(&mut self) {
        self.socket = None;
        self.status = DriverStatus::Down;
    }

    /// Return `Ok` if the socket is open, `Err` otherwise.
    async fn ping(&mut self) -> Result<DriverMeta, DriverError> {
        match &self.socket {
            Some(_) => Ok(DriverMeta {
                model: Some(format!("BACnet/IP port={}", self.port)),
                ..Default::default()
            }),
            None => Err(DriverError::CommFault("not connected".into())),
        }
    }

    async fn learn(&mut self, _path: Option<&str>) -> Result<LearnGrid, DriverError> {
        Err(DriverError::NotSupported("bacnet learn"))
    }

    async fn sync_cur(
        &mut self,
        _points: &[DriverPointRef],
    ) -> Vec<(u32, Result<f64, DriverError>)> {
        // Phase B3 will implement ReadProperty requests here.
        Vec::new()
    }

    async fn write(&mut self, _writes: &[(u32, f64)]) -> Vec<(u32, Result<(), DriverError>)> {
        // Phase B3 will implement WriteProperty requests here.
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

    // ── Carried-over lifecycle tests ────────────────────────

    #[tokio::test]
    async fn bacnet_lifecycle() {
        let mut d = BacnetDriver::new("bac-1", "255.255.255.255", DEFAULT_BACNET_PORT);
        assert_eq!(*d.status(), DriverStatus::Pending);
        assert_eq!(d.driver_type(), "bacnet");
        d.close().await;
        assert_eq!(*d.status(), DriverStatus::Down);
    }

    #[tokio::test]
    async fn bacnet_learn_not_supported() {
        let mut d = BacnetDriver::new("bac-2", "255.255.255.255", DEFAULT_BACNET_PORT);
        assert!(d.learn(None).await.is_err());
    }

    #[tokio::test]
    async fn bacnet_ping_not_connected() {
        // Before open(), socket is None — ping must fail.
        let mut d = BacnetDriver::new("bac-3", "255.255.255.255", DEFAULT_BACNET_PORT);
        assert!(d.ping().await.is_err());
    }

    #[tokio::test]
    async fn bacnet_sync_and_write_empty() {
        let mut d = BacnetDriver::new("bac-4", "255.255.255.255", DEFAULT_BACNET_PORT);
        assert!(d.sync_cur(&[]).await.is_empty());
        assert!(d.write(&[]).await.is_empty());
    }

    // ── New structural tests ────────────────────────────────

    #[test]
    fn bacnet_driver_new_has_expected_fields() {
        let d = BacnetDriver::new("bac-new", "192.168.1.255", DEFAULT_BACNET_PORT);
        assert_eq!(d.id(), "bac-new");
        assert_eq!(d.port, DEFAULT_BACNET_PORT);
        assert_eq!(d.broadcast_addr, "192.168.1.255");
        assert!(matches!(d.status(), DriverStatus::Pending));
        assert!(d.socket.is_none());
        assert!(d.objects.is_empty());
        assert!(d.device_registry.is_empty());
    }

    #[test]
    fn bacnet_add_object_stores_it() {
        let mut d = BacnetDriver::new("bac-obj", "255.255.255.255", DEFAULT_BACNET_PORT);
        d.add_object(
            1001,
            object::BacnetObject {
                device_id: 42,
                object_type: 0, // Analog Input
                instance: 5,
                scale: 1.0,
                offset: 0.0,
                unit: Some("degF".into()),
            },
        );
        assert_eq!(d.objects().len(), 1);
        let obj = d
            .objects()
            .get(&1001)
            .expect("point 1001 should be present");
        assert_eq!(obj.device_id, 42);
        assert_eq!(obj.object_type, 0);
        assert_eq!(obj.instance, 5);
        assert_eq!(obj.unit.as_deref(), Some("degF"));
    }

    #[test]
    fn bacnet_from_config_creates_objects() {
        let config = BacnetConfig {
            id: "bac-cfg".into(),
            port: Some(47808),
            broadcast: Some("192.168.0.255".into()),
            objects: vec![
                BacnetObjectConfig {
                    point_id: 2001,
                    device_id: 100,
                    object_type: 0,
                    instance: 10,
                    unit: Some("psi".into()),
                    scale: Some(2.5),
                    offset: Some(-10.0),
                },
                BacnetObjectConfig {
                    point_id: 2002,
                    device_id: 100,
                    object_type: 1,
                    instance: 1,
                    unit: None,
                    scale: None,
                    offset: None,
                },
            ],
        };

        let d = BacnetDriver::from_config(config);
        assert_eq!(d.id(), "bac-cfg");
        assert_eq!(d.port, 47808);
        assert_eq!(d.broadcast_addr, "192.168.0.255");
        assert_eq!(d.objects().len(), 2);

        let obj = d.objects().get(&2001).expect("point 2001 should exist");
        assert_eq!(obj.scale, 2.5);
        assert_eq!(obj.offset, -10.0);
        assert_eq!(obj.unit.as_deref(), Some("psi"));

        let obj2 = d.objects().get(&2002).expect("point 2002 should exist");
        assert_eq!(obj2.scale, 1.0, "default scale should be 1.0");
        assert_eq!(obj2.offset, 0.0, "default offset should be 0.0");
    }

    #[test]
    fn bacnet_from_config_default_port_and_broadcast() {
        let config = BacnetConfig {
            id: "bac-defaults".into(),
            port: None,
            broadcast: None,
            objects: vec![],
        };
        let d = BacnetDriver::from_config(config);
        assert_eq!(d.port, DEFAULT_BACNET_PORT);
        assert_eq!(d.broadcast_addr, "255.255.255.255");
    }

    // ── BacnetError conversion and display ──────────────────

    #[test]
    fn bacnet_error_converts_to_driver_error() {
        let e: DriverError = BacnetError::DeviceNotFound(99).into();
        assert!(matches!(e, DriverError::CommFault(_)));
    }

    #[test]
    fn bacnet_error_display_messages() {
        assert_eq!(
            BacnetError::MalformedFrame("bad header".into()).to_string(),
            "malformed frame: bad header"
        );
        assert_eq!(
            BacnetError::PropertyNotFound.to_string(),
            "property not found"
        );
        assert_eq!(
            BacnetError::Timeout(3).to_string(),
            "transaction timeout after 3 retries"
        );
        assert_eq!(
            BacnetError::RemoteError { class: 2, code: 31 }.to_string(),
            "remote error class=2 code=31"
        );
    }
}

// ── Integration tests ──────────────────────────────────────

#[cfg(test)]
mod discovery_integration {
    use super::*;

    // ── Helpers ────────────────────────────────────────────

    /// Bind TCP to port 0 to obtain an OS-assigned free port, then release it.
    /// (Tiny TOCTOU race is acceptable for tests.)
    fn find_free_port() -> u16 {
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    /// Encode a minimal BACnet I-Am frame.
    fn encode_i_am(
        device_instance: u32,
        max_apdu: u16,
        segmentation: u8,
        vendor_id: u16,
    ) -> Vec<u8> {
        let obj_id_val: u32 = (8u32 << 22) | (device_instance & 0x3F_FFFF);
        let obj_id = obj_id_val.to_be_bytes();

        let mut apdu: Vec<u8> = vec![
            0x10, 0x00, // Unconfirmed-Request, I-Am service
            // Object ID: application tag 12, LVT=4
            0xC4, obj_id[0], obj_id[1], obj_id[2], obj_id[3],
        ];

        // Max-APDU length accepted: application tag 2 (Unsigned)
        if max_apdu <= 0xFF {
            apdu.extend_from_slice(&[0x21, max_apdu as u8]);
        } else {
            apdu.extend_from_slice(&[0x22, (max_apdu >> 8) as u8, (max_apdu & 0xFF) as u8]);
        }
        // Segmentation: application tag 9 (Enumerated), LVT=1
        apdu.extend_from_slice(&[0x91, segmentation]);
        // Vendor ID: application tag 2 (Unsigned)
        if vendor_id <= 0xFF {
            apdu.extend_from_slice(&[0x21, vendor_id as u8]);
        } else {
            apdu.extend_from_slice(&[0x22, (vendor_id >> 8) as u8, (vendor_id & 0xFF) as u8]);
        }

        // BVLL unicast header + NPDU
        let total_len = 4u16 + 2 + apdu.len() as u16;
        let mut frame = vec![
            0x81,
            0x0A, // BVLL unicast
            (total_len >> 8) as u8,
            (total_len & 0xFF) as u8,
            0x01,
            0x00, // NPDU version=1, control=0
        ];
        frame.extend_from_slice(&apdu);
        frame
    }

    /// Spawn a UDP task on `port` that waits for ONE packet (the Who-Is),
    /// then replies with an I-Am from `device_instance`.
    async fn spawn_mock_device(port: u16, device_instance: u32) -> tokio::task::JoinHandle<()> {
        let sock = tokio::net::UdpSocket::bind(format!("127.0.0.1:{port}"))
            .await
            .expect("mock bind failed");
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            if let Ok((_, from)) = sock.recv_from(&mut buf).await {
                let reply = encode_i_am(device_instance, 1476, 3, 8);
                let _ = sock.send_to(&reply, from).await;
            }
        })
    }

    // ── Tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_open_with_no_response_succeeds_but_registry_empty() {
        // Listen on a mock port but never respond — driver should still return Ok.
        let mock_port = find_free_port();
        let _sock = tokio::net::UdpSocket::bind(format!("127.0.0.1:{mock_port}"))
            .await
            .unwrap();

        let mut driver = BacnetDriver::new("no-resp", "127.0.0.1", 0)
            .with_broadcast_port(mock_port)
            .with_discovery_timeout(std::time::Duration::from_millis(50));

        let meta = driver
            .open()
            .await
            .expect("open should succeed even with no devices");
        assert!(
            meta.model.as_deref().unwrap_or("").contains("0 devices"),
            "model should report 0 devices, got: {:?}",
            meta.model
        );
        assert_eq!(*driver.status(), DriverStatus::Ok);
        assert!(driver.device_registry().is_empty());
        driver.close().await;
    }

    #[tokio::test]
    async fn test_open_discovers_single_device() {
        let mock_port = find_free_port();
        let device_instance = 12345u32;

        let _handle = spawn_mock_device(mock_port, device_instance).await;
        // Give the mock task time to bind before the driver sends Who-Is.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let mut driver = BacnetDriver::new("single-dev", "127.0.0.1", 0)
            .with_broadcast_port(mock_port)
            .with_discovery_timeout(std::time::Duration::from_millis(200));

        let meta = driver.open().await.expect("open should succeed");
        assert!(
            meta.model.as_deref().unwrap_or("").contains("1 device"),
            "model should report 1 device, got: {:?}",
            meta.model
        );

        let dev = driver
            .device_registry()
            .get(device_instance)
            .expect("device 12345 should be in registry");
        assert_eq!(dev.instance, device_instance);
        assert_eq!(dev.max_apdu, 1476);
        assert_eq!(dev.vendor_id, 8);

        driver.close().await;
        assert_eq!(*driver.status(), DriverStatus::Down);
    }

    #[tokio::test]
    async fn test_open_discovers_multiple_devices() {
        // One mock socket sends 3 I-Am replies in response to a single Who-Is.
        let mock_port = find_free_port();

        let sock = tokio::net::UdpSocket::bind(format!("127.0.0.1:{mock_port}"))
            .await
            .unwrap();

        let _handle = tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            if let Ok((_, from)) = sock.recv_from(&mut buf).await {
                for instance in [100u32, 200, 300] {
                    let reply = encode_i_am(instance, 1476, 3, 8);
                    let _ = sock.send_to(&reply, from).await;
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let mut driver = BacnetDriver::new("multi-dev", "127.0.0.1", 0)
            .with_broadcast_port(mock_port)
            .with_discovery_timeout(std::time::Duration::from_millis(300));

        driver.open().await.expect("open should succeed");
        assert_eq!(driver.device_registry().len(), 3);
        assert!(driver.device_registry().get(100).is_some());
        assert!(driver.device_registry().get(200).is_some());
        assert!(driver.device_registry().get(300).is_some());

        driver.close().await;
    }

    #[tokio::test]
    async fn test_ping_fails_before_open() {
        let mut driver = BacnetDriver::new("ping-test", "127.0.0.1", 0).with_broadcast_port(47900);
        assert!(driver.ping().await.is_err(), "ping before open must fail");
    }

    #[tokio::test]
    async fn test_close_releases_socket_and_sets_status_down() {
        let mock_port = find_free_port();
        let _sock = tokio::net::UdpSocket::bind(format!("127.0.0.1:{mock_port}"))
            .await
            .unwrap();

        let mut driver = BacnetDriver::new("close-test", "127.0.0.1", 0)
            .with_broadcast_port(mock_port)
            .with_discovery_timeout(std::time::Duration::from_millis(20));

        driver.open().await.unwrap();
        // After open(), ping must succeed (socket is bound).
        assert!(driver.ping().await.is_ok(), "ping after open must succeed");

        driver.close().await;
        assert_eq!(*driver.status(), DriverStatus::Down);
        // After close(), ping must fail (socket dropped).
        assert!(driver.ping().await.is_err(), "ping after close must fail");
    }

    #[tokio::test]
    async fn test_device_registry_accessor_get_and_len() {
        // Verify that device_registry() returns &DeviceRegistry with working get/len.
        let mock_port = find_free_port();
        let _handle = spawn_mock_device(mock_port, 9999).await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let mut driver = BacnetDriver::new("registry-acc", "127.0.0.1", 0)
            .with_broadcast_port(mock_port)
            .with_discovery_timeout(std::time::Duration::from_millis(200));

        driver.open().await.unwrap();

        let reg: &discovery::DeviceRegistry = driver.device_registry();
        assert_eq!(reg.len(), 1);
        assert!(reg.get(9999).is_some());
        assert!(reg.get(0).is_none());

        driver.close().await;
    }
}
