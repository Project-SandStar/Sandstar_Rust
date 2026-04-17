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
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::UdpSocket;

use super::async_driver::AsyncDriver;
use super::{
    DriverError, DriverMeta, DriverPointRef, DriverStatus, LearnGrid, LearnPoint, PollMode,
    SyncContext, WriteContext,
};

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
    /// Optional BBMD (Broadcast Management Device) address for multi-subnet support.
    /// Format: "host:port" (e.g. "192.168.2.1:47808").
    /// When set, the driver registers as a foreign device with this BBMD during open().
    #[serde(default)]
    pub bbmd: Option<String>,
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

/// State of a single COV subscription tracked by the driver (Phase B8).
///
/// The `lifetime` field drives Phase B8.2 renewal logic.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct CovSubscription {
    /// Sandstar point_id this subscription covers.
    point_id: u32,
    /// BACnet subscriber-process-identifier we issued.
    process_id: u32,
    /// Object we subscribed to.
    object_type: u16,
    object_instance: u32,
    /// Device instance the subscription is with.
    device_id: u32,
    /// Lifetime seconds we requested. `None` = indefinite.
    lifetime: Option<u32>,
    /// When the subscription was last established or renewed.
    subscribed_at: std::time::Instant,
}

/// Cached COV (Change of Value) entry for a BACnet object (Phase B8.1).
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct CovCacheEntry {
    /// The latest property value from a COV notification.
    value: value::BacnetValue,
    /// When this cache entry was last updated.
    updated_at: std::time::Instant,
    /// The subscriber process ID that triggered this update.
    process_id: u32,
}

/// Cache of latest COV values, keyed by (object_type, instance).
///
/// Updated as a side-effect when inline recv loops encounter COV
/// notification packets. Used by `sync_cur()` to return cached values
/// for COV-subscribed points instead of making network reads.
#[derive(Debug, Default)]
struct CovCache {
    entries: HashMap<(u16, u32), CovCacheEntry>,
}

impl CovCache {
    fn new() -> Self {
        Self::default()
    }

    /// Update the cache from a COV notification.
    /// Stores the PresentValue (property 85) if present in the notification's value list.
    fn update(&mut self, notification: &frame::CovNotification) {
        for pv in &notification.values {
            if pv.property_id == 85 {
                // PresentValue
                self.entries.insert(
                    (
                        notification.monitored_object_type,
                        notification.monitored_object_instance,
                    ),
                    CovCacheEntry {
                        value: pv.value.clone(),
                        updated_at: std::time::Instant::now(),
                        process_id: notification.subscriber_process_id,
                    },
                );
            }
        }
    }

    /// Look up a cached value. Returns None if not present or older than `max_age`.
    fn get(
        &self,
        object_type: u16,
        instance: u32,
        max_age: std::time::Duration,
    ) -> Option<&CovCacheEntry> {
        self.entries
            .get(&(object_type, instance))
            .filter(|e| e.updated_at.elapsed() < max_age)
    }

    /// Remove cache entries for a specific object.
    fn remove(&mut self, object_type: u16, instance: u32) {
        self.entries.remove(&(object_type, instance));
    }
}

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
    /// In-flight BACnet transaction table — invoke ID allocation and response routing.
    transactions: transaction::TransactionTable,
    /// How long to wait for I-Am responses after sending Who-Is.
    /// Default: 2 seconds. Set to 50–200 ms in tests.
    discovery_timeout: std::time::Duration,
    /// Port to send Who-Is broadcast to (defaults to `self.port`).
    /// In tests, set this to the mock device's port while `port` is 0
    /// (letting the OS assign an ephemeral bind port).
    broadcast_port: u16,
    /// Active COV subscriptions indexed by subscriber-process-identifier.
    cov_subscriptions: HashMap<u32, CovSubscription>,
    /// Monotonically-increasing process_id counter for new subscriptions.
    next_process_id: u32,
    /// Cache of latest COV values (Phase B8.1).
    cov_cache: CovCache,
    /// BBMD address to register with. `None` = local broadcast only.
    bbmd_addr: Option<std::net::SocketAddr>,
    /// Whether we're registered as a foreign device.
    is_foreign_device: bool,
    /// How long after `subscribed_at` to renew a COV subscription.
    /// Default: 240s (80% of the default 300s lifetime).
    renewal_interval: std::time::Duration,
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
            transactions: transaction::TransactionTable::new(),
            discovery_timeout: std::time::Duration::from_secs(2),
            cov_subscriptions: HashMap::new(),
            next_process_id: 1,
            cov_cache: CovCache::new(),
            bbmd_addr: None,
            is_foreign_device: false,
            renewal_interval: std::time::Duration::from_secs(240),
        }
    }

    /// Override the COV subscription renewal interval (useful in tests).
    ///
    /// Default: 240s (80% of the 300s lifetime issued by `on_watch`).
    pub fn with_renewal_interval(mut self, interval: std::time::Duration) -> Self {
        self.renewal_interval = interval;
        self
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

    /// Set the BBMD address (useful in tests).
    pub fn with_bbmd_addr(mut self, addr: std::net::SocketAddr) -> Self {
        self.bbmd_addr = Some(addr);
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
        if let Some(ref bbmd_str) = config.bbmd {
            driver.bbmd_addr = bbmd_str.parse().ok();
            if driver.bbmd_addr.is_none() {
                tracing::warn!(bbmd = %bbmd_str, "invalid BBMD address, ignoring");
            }
        }
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
}

// ── DriverLoader impl (Phase 12.0A) ────────────────────────

/// [`DriverLoader`] specialization for BACnet.
///
/// Used by `rest/mod.rs` to wire `SANDSTAR_BACNET_CONFIGS` into the
/// generic loader. See [`crate::drivers::loader::load_drivers`] for the
/// shared flow.
pub struct BacnetLoader;

impl crate::drivers::loader::DriverLoader for BacnetLoader {
    const ENV_VAR: &'static str = "SANDSTAR_BACNET_CONFIGS";
    const DRIVER_TYPE: &'static str = "bacnet";
    const LABEL: &'static str = "BACnet";

    type Config = BacnetConfig;

    fn config_id(config: &Self::Config) -> String {
        config.id.clone()
    }

    fn config_point_ids(config: &Self::Config) -> Vec<u32> {
        config.objects.iter().map(|o| o.point_id).collect()
    }

    fn build_driver(config: Self::Config) -> Box<dyn super::async_driver::AsyncDriver> {
        Box::new(BacnetDriver::from_config(config))
    }
}

impl BacnetDriver {
    /// Return a reference to the device registry (used in tests and Phase B2).
    pub fn device_registry(&self) -> &discovery::DeviceRegistry {
        &self.device_registry
    }
}

// ── Free helpers ───────────────────────────────────────────

/// Extract the invoke ID from an APDU if it carries one.
///
/// WhoIs and IAm are unconfirmed broadcast PDUs and carry no invoke ID.
fn apdu_invoke_id(apdu: &frame::Apdu) -> Option<u8> {
    match apdu {
        frame::Apdu::ReadPropertyRequest { invoke_id, .. } => Some(*invoke_id),
        frame::Apdu::ReadPropertyAck { invoke_id, .. } => Some(*invoke_id),
        frame::Apdu::WritePropertyRequest { invoke_id, .. } => Some(*invoke_id),
        frame::Apdu::SimpleAck { invoke_id, .. } => Some(*invoke_id),
        frame::Apdu::Error { invoke_id, .. } => Some(*invoke_id),
        frame::Apdu::Other { invoke_id, .. } => Some(*invoke_id),
        frame::Apdu::ReadPropertyMultipleRequest { invoke_id, .. } => Some(*invoke_id),
        frame::Apdu::ReadPropertyMultipleAck { invoke_id, .. } => Some(*invoke_id),
        frame::Apdu::ConfirmedCovNotification { invoke_id, .. } => Some(*invoke_id),
        frame::Apdu::WhoIs { .. }
        | frame::Apdu::IAm { .. }
        | frame::Apdu::UnconfirmedCovNotification { .. } => None,
    }
}

/// Send a Confirmed-Request and wait for a matching response, with retries.
///
/// - Allocates an invoke ID from `transactions`
/// - Sends `request_bytes` to `device_addr`
/// - Loops `recv_from` on `socket`, dispatching any matching response
/// - Returns the matched [`frame::Apdu`] or [`BacnetError::Timeout`]
#[allow(dead_code)]
async fn bacnet_transact(
    socket: &UdpSocket,
    transactions: &mut transaction::TransactionTable,
    device_addr: SocketAddr,
    request_bytes: &[u8],
    per_attempt_timeout: Duration,
    max_retries: u32,
) -> Result<frame::Apdu, BacnetError> {
    let (invoke_id, mut rx) = transactions
        .allocate()
        .ok_or_else(|| BacnetError::MalformedFrame("no invoke IDs available".into()))?;

    let mut buf = [0u8; 1500];

    for attempt in 0..=max_retries {
        // (Re-)send on every attempt.
        socket.send_to(request_bytes, device_addr).await?;

        let deadline = tokio::time::Instant::now() + per_attempt_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break; // timeout for this attempt — retry
            }

            match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
                Err(_elapsed) => break, // per-attempt timeout expired
                Ok(Err(_io)) => break,  // IO error — retry
                Ok(Ok((n, _src))) => {
                    // Decode and dispatch to the correct waiter.
                    if let Ok((_npdu, apdu)) = frame::decode_packet(&buf[..n]) {
                        if let Some(id) = apdu_invoke_id(&apdu) {
                            transactions.dispatch(id, apdu);
                        }
                    }

                    // Check whether OUR response arrived.
                    if let Ok(result) = rx.try_recv() {
                        return result;
                    }
                }
            }
        }

        // Last attempt — don't restart the loop.
        if attempt == max_retries {
            break;
        }
    }

    // All retries exhausted.
    transactions.timeout(invoke_id);
    Err(BacnetError::Timeout(max_retries))
}

/// Shared send/recv/retry/dispatch loop used by all ReadProperty /
/// WriteProperty / SubscribeCOV / ReadPropertyMultiple methods.
///
/// Allocates an invoke ID from `transactions`, asks the caller to build
/// the request bytes with that ID, then runs the retry loop:
/// - `max_retries + 1` send attempts, each with `per_attempt_timeout`
/// - every incoming packet is decoded; COV notifications update `cov_cache`
///   as a side-effect; other APDUs are dispatched to the transaction table
/// - returns as soon as OUR response arrives (matched by invoke ID)
///
/// Returns the decoded Apdu on success. Caller does method-specific
/// matching (Ack vs Error vs other) and produces the final typed result.
async fn bacnet_transact_inner<F>(
    socket: &UdpSocket,
    transactions: &mut transaction::TransactionTable,
    cov_cache: &mut CovCache,
    device_addr: SocketAddr,
    build_request: F,
    per_attempt_timeout: Duration,
    max_retries: u32,
) -> Result<frame::Apdu, DriverError>
where
    F: FnOnce(u8) -> Vec<u8>,
{
    let (invoke_id, rx) = transactions
        .allocate()
        .ok_or_else(|| DriverError::CommFault("bacnet: no invoke IDs available".into()))?;
    let request = build_request(invoke_id);
    let mut rx = rx;
    let mut buf = [0u8; 1500];

    'retry: for attempt in 0..=max_retries {
        if let Err(e) = socket.send_to(&request, device_addr).await {
            return Err(DriverError::CommFault(format!("bacnet send: {e}")));
        }
        let deadline = tokio::time::Instant::now() + per_attempt_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                if attempt < max_retries {
                    continue 'retry;
                } else {
                    break 'retry;
                }
            }
            match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
                Err(_) => {
                    if attempt < max_retries {
                        continue 'retry;
                    } else {
                        break 'retry;
                    }
                }
                Ok(Err(e)) => return Err(DriverError::CommFault(format!("bacnet recv: {e}"))),
                Ok(Ok((n, _src))) => {
                    if let Ok((_npdu, apdu)) = frame::decode_packet(&buf[..n]) {
                        // Side-effect: COV notifications update the cache (Phase B8.1)
                        match &apdu {
                            frame::Apdu::UnconfirmedCovNotification { notification }
                            | frame::Apdu::ConfirmedCovNotification { notification, .. } => {
                                cov_cache.update(notification);
                            }
                            _ => {}
                        }
                        // Dispatch responses to waiters
                        if let Some(id) = apdu_invoke_id(&apdu) {
                            transactions.dispatch(id, apdu);
                        }
                    }
                    if let Ok(result) = rx.try_recv() {
                        return match result {
                            Ok(apdu) => Ok(apdu),
                            Err(e) => Err(DriverError::CommFault(e.to_string())),
                        };
                    }
                }
            }
        }
    }

    // All retries exhausted
    transactions.timeout(invoke_id);
    Err(DriverError::CommFault(format!(
        "bacnet: timeout after {max_retries} retries"
    )))
}

// ── Per-point read helper ──────────────────────────────────

impl BacnetDriver {
    /// Read the Present_Value property (property 85) from a BACnet object.
    ///
    /// Applies `obj.scale` and `obj.offset` to the raw device value before
    /// returning.
    async fn read_present_value(
        &mut self,
        device_addr: SocketAddr,
        obj: &object::BacnetObject,
    ) -> Result<f64, DriverError> {
        // Split borrows: `socket` borrows self.socket; `transactions` and
        // `cov_cache` borrow other fields. Different fields => NLL allows it.
        let socket = self
            .socket
            .as_ref()
            .ok_or_else(|| DriverError::CommFault("bacnet: not connected".into()))?;

        let object_type = obj.object_type;
        let instance = obj.instance;
        let apdu = bacnet_transact_inner(
            socket,
            &mut self.transactions,
            &mut self.cov_cache,
            device_addr,
            |invoke_id| frame::encode_read_property(invoke_id, object_type, instance, 85, None),
            Duration::from_secs(3),
            3,
        )
        .await?;

        match apdu {
            frame::Apdu::ReadPropertyAck { value, .. } => {
                let raw = value.to_f64().ok_or_else(|| {
                    DriverError::CommFault("bacnet: non-numeric present value".into())
                })?;
                Ok(raw * obj.scale + obj.offset)
            }
            frame::Apdu::Error {
                error_class,
                error_code,
                ..
            } => Err(DriverError::RemoteStatus(format!(
                "BACnet error class={error_class} code={error_code}"
            ))),
            other => Err(DriverError::CommFault(format!(
                "bacnet: unexpected response: {other:?}"
            ))),
        }
    }

    /// Send a ReadProperty request and return the decoded `BacnetValue`.
    ///
    /// Generic version of `read_present_value()` — reads any property from
    /// any object on any device.
    async fn read_property_generic(
        &mut self,
        device_addr: std::net::SocketAddr,
        object_type: u16,
        object_instance: u32,
        property_id: u32,
    ) -> Result<value::BacnetValue, BacnetError> {
        let socket = self.socket.as_ref().ok_or_else(|| {
            BacnetError::Io(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "not connected",
            ))
        })?;

        let (invoke_id, rx) = self
            .transactions
            .allocate()
            .ok_or_else(|| BacnetError::MalformedFrame("no invoke IDs available".into()))?;

        let request =
            frame::encode_read_property(invoke_id, object_type, object_instance, property_id, None);

        let per_attempt = std::time::Duration::from_secs(3);
        let max_retries = 3u32;
        let mut buf = [0u8; 1500];
        let mut rx = rx;

        'retry: for attempt in 0..=max_retries {
            socket.send_to(&request, device_addr).await?;

            let deadline = tokio::time::Instant::now() + per_attempt;
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    if attempt < max_retries {
                        continue 'retry;
                    } else {
                        break 'retry;
                    }
                }
                match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
                    Err(_) => {
                        if attempt < max_retries {
                            continue 'retry;
                        } else {
                            break 'retry;
                        }
                    }
                    Ok(Err(e)) => return Err(BacnetError::Io(e)),
                    Ok(Ok((n, _src))) => {
                        if let Ok((_npdu, apdu)) = frame::decode_packet(&buf[..n]) {
                            // Side-effect: process COV notifications (Phase B8.1)
                            match &apdu {
                                frame::Apdu::UnconfirmedCovNotification { notification }
                                | frame::Apdu::ConfirmedCovNotification { notification, .. } => {
                                    self.cov_cache.update(notification);
                                }
                                _ => {}
                            }
                            // Dispatch to transaction waiters
                            if let Some(id) = apdu_invoke_id(&apdu) {
                                self.transactions.dispatch(id, apdu);
                            }
                        }
                        if let Ok(result) = rx.try_recv() {
                            return match result {
                                Ok(frame::Apdu::ReadPropertyAck { value, .. }) => Ok(value),
                                Ok(frame::Apdu::Error {
                                    error_class,
                                    error_code,
                                    ..
                                }) => Err(BacnetError::RemoteError {
                                    class: error_class,
                                    code: error_code,
                                }),
                                Ok(_) => {
                                    Err(BacnetError::MalformedFrame("unexpected APDU type".into()))
                                }
                                Err(e) => Err(e),
                            };
                        }
                    }
                }
            }
        }

        self.transactions.timeout(invoke_id);
        Err(BacnetError::Timeout(max_retries))
    }

    /// Send a ReadPropertyMultiple request and parse the response.
    ///
    /// Returns a Vec of the SAME length as `specs`, in the same order.
    /// Each element is either:
    ///   - `Ok(BacnetValue)` — successful read
    ///   - `Err(DriverError::RemoteStatus)` — per-property error from device
    ///
    /// Returns `Err(BacnetError)` for transport failures (timeout, malformed
    /// frame) that affect the whole batch — the caller should decide whether
    /// to fall back to individual reads.
    async fn read_properties_multiple(
        &mut self,
        device_addr: std::net::SocketAddr,
        specs: &[frame::RpmRequestSpec],
    ) -> Result<Vec<Result<value::BacnetValue, DriverError>>, BacnetError> {
        // NLL split borrow: `socket` borrows self.socket (immutable) and
        // `transactions` borrows self.transactions mutably — different fields.
        let socket = self.socket.as_ref().ok_or_else(|| {
            BacnetError::Io(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "not connected",
            ))
        })?;

        let (invoke_id, rx) = self
            .transactions
            .allocate()
            .ok_or_else(|| BacnetError::MalformedFrame("no invoke IDs available".into()))?;

        let request = frame::encode_read_property_multiple(invoke_id, specs);

        let per_attempt = Duration::from_secs(3);
        let max_retries = 3u32;
        let mut buf = [0u8; 1500];
        let mut rx = rx;

        'retry: for attempt in 0..=max_retries {
            socket.send_to(&request, device_addr).await?;

            let deadline = tokio::time::Instant::now() + per_attempt;
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    if attempt < max_retries {
                        continue 'retry;
                    } else {
                        break 'retry;
                    }
                }
                match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
                    Err(_) => {
                        if attempt < max_retries {
                            continue 'retry;
                        } else {
                            break 'retry;
                        }
                    }
                    Ok(Err(e)) => return Err(BacnetError::Io(e)),
                    Ok(Ok((n, _src))) => {
                        if let Ok((_npdu, apdu)) = frame::decode_packet(&buf[..n]) {
                            // Side-effect: process COV notifications (Phase B8.1)
                            match &apdu {
                                frame::Apdu::UnconfirmedCovNotification { notification }
                                | frame::Apdu::ConfirmedCovNotification { notification, .. } => {
                                    self.cov_cache.update(notification);
                                }
                                _ => {}
                            }
                            // Dispatch to transaction waiters
                            if let Some(id) = apdu_invoke_id(&apdu) {
                                self.transactions.dispatch(id, apdu);
                            }
                        }
                        if let Ok(result) = rx.try_recv() {
                            return match result {
                                Ok(frame::Apdu::ReadPropertyMultipleAck { results, .. }) => {
                                    let mut out: Vec<Result<value::BacnetValue, DriverError>> =
                                        Vec::with_capacity(specs.len());
                                    for spec in specs {
                                        let matched = results.iter().find(|r| {
                                            r.object_type == spec.object_type
                                                && r.instance == spec.instance
                                                && r.property_id == spec.property_id
                                        });
                                        match matched {
                                            Some(r) => match &r.value {
                                                Ok(v) => out.push(Ok(v.clone())),
                                                Err((cls, code)) => out.push(Err(
                                                    DriverError::RemoteStatus(format!(
                                                        "BACnet error class={cls} code={code}"
                                                    )),
                                                )),
                                            },
                                            None => out.push(Err(DriverError::CommFault(
                                                "BACnet RPM: missing result for spec".into(),
                                            ))),
                                        }
                                    }
                                    Ok(out)
                                }
                                Ok(frame::Apdu::Error {
                                    error_class,
                                    error_code,
                                    ..
                                }) => Err(BacnetError::RemoteError {
                                    class: error_class,
                                    code: error_code,
                                }),
                                Ok(other) => Err(BacnetError::MalformedFrame(format!(
                                    "unexpected APDU type: {other:?}"
                                ))),
                                Err(e) => Err(e),
                            };
                        }
                    }
                }
            }
        }

        self.transactions.timeout(invoke_id);
        Err(BacnetError::Timeout(max_retries))
    }

    /// Read the Device object-list property (property 76) from a device.
    ///
    /// Returns a list of `(object_type, instance)` pairs for all objects
    /// on the device. Device objects (type 8) are filtered out.
    async fn read_object_list(
        &mut self,
        device_addr: std::net::SocketAddr,
        device_instance: u32,
    ) -> Result<Vec<(u16, u32)>, BacnetError> {
        let val = self
            .read_property_generic(device_addr, 8, device_instance, 76)
            .await?;

        let items: Vec<value::BacnetValue> = match val {
            value::BacnetValue::Array(items) => items,
            single => vec![single],
        };

        let mut out = Vec::with_capacity(items.len());
        for item in items {
            if let value::BacnetValue::ObjectId {
                object_type,
                instance,
            } = item
            {
                // Skip the Device object itself (type 8) — not a data point.
                if object_type != 8 {
                    out.push((object_type, instance));
                }
            }
        }
        Ok(out)
    }

    /// Read the object-name property (property 77) from a BACnet object.
    ///
    /// Returns the name as a `String`. Falls back to `"type-instance"` format
    /// on any error (caller uses `unwrap_or_else`).
    async fn read_object_name(
        &mut self,
        device_addr: std::net::SocketAddr,
        object_type: u16,
        object_instance: u32,
    ) -> Result<String, BacnetError> {
        let val = self
            .read_property_generic(device_addr, object_type, object_instance, 77)
            .await?;
        match val {
            value::BacnetValue::CharacterString(s) => Ok(s),
            other => Err(BacnetError::MalformedFrame(format!(
                "expected CharacterString for object-name, got {other:?}"
            ))),
        }
    }

    /// Write a property-value to a BACnet object.
    ///
    /// Mirrors [`Self::read_present_value`]: sends a WriteProperty request,
    /// retries up to three times (3 second per-attempt timeout), and
    /// dispatches incoming frames to the transaction table until a matching
    /// response arrives.
    ///
    /// Returns:
    /// * `Ok(())` when the device sends a Simple-ACK.
    /// * [`DriverError::RemoteStatus`] on Error PDU.
    /// * [`DriverError::CommFault`] on timeout, socket error, or unexpected
    ///   response type.
    async fn write_property(
        &mut self,
        device_addr: std::net::SocketAddr,
        object_type: u16,
        object_instance: u32,
        property_id: u32,
        value: value::BacnetValue,
        priority: Option<u8>,
    ) -> Result<(), DriverError> {
        // NLL split borrow of self.socket / self.transactions (different fields).
        let socket = self
            .socket
            .as_ref()
            .ok_or_else(|| DriverError::CommFault("bacnet: not connected".into()))?;

        let apdu = bacnet_transact_inner(
            socket,
            &mut self.transactions,
            &mut self.cov_cache,
            device_addr,
            |invoke_id| {
                frame::encode_write_property(
                    invoke_id,
                    object_type,
                    object_instance,
                    property_id,
                    &value,
                    None, // array_index
                    priority,
                )
            },
            Duration::from_secs(3),
            3,
        )
        .await?;

        match apdu {
            frame::Apdu::SimpleAck { .. } => Ok(()),
            frame::Apdu::Error {
                error_class,
                error_code,
                ..
            } => Err(DriverError::RemoteStatus(format!(
                "BACnet error class={error_class} code={error_code}"
            ))),
            other => Err(DriverError::CommFault(format!(
                "bacnet: unexpected response: {other:?}"
            ))),
        }
    }

    // ── Phase B8.1: COV notification handling ───────────────

    /// Process a COV notification: update the cache.
    /// Called from test code; recv loops call `self.cov_cache.update()` directly
    /// to satisfy the borrow checker (socket held immutably while cache updates).
    #[allow(dead_code)]
    fn handle_cov_notification(&mut self, notification: &frame::CovNotification) {
        tracing::debug!(
            process_id = notification.subscriber_process_id,
            device = notification.initiating_device_instance,
            object_type = notification.monitored_object_type,
            instance = notification.monitored_object_instance,
            values = notification.values.len(),
            "BACnet COV notification received"
        );
        self.cov_cache.update(notification);
    }

    // ── Phase B10: BBMD / Foreign Device Registration ──────

    /// Register as a foreign device with the configured BBMD.
    ///
    /// Sends Register-Foreign-Device with the given TTL and waits for a
    /// BVLL-Result response. Returns `Ok(())` on success (result code 0),
    /// `Err` on failure or timeout.
    async fn register_foreign_device(
        &self,
        socket: &tokio::net::UdpSocket,
        bbmd_addr: std::net::SocketAddr,
        ttl_seconds: u16,
    ) -> Result<(), BacnetError> {
        let request = frame::encode_register_foreign_device(ttl_seconds);

        // Send + wait for BVLL-Result (no invoke_id — this is BVLL-level, not APDU-level)
        let mut buf = [0u8; 1500];
        let timeout = std::time::Duration::from_secs(5);

        for attempt in 0..3u32 {
            socket.send_to(&request, bbmd_addr).await?;

            match tokio::time::timeout(timeout, socket.recv_from(&mut buf)).await {
                Ok(Ok((n, _addr))) => match frame::decode_bvll_result(&buf[..n]) {
                    Ok(0) => {
                        tracing::info!(bbmd = %bbmd_addr, ttl = ttl_seconds, "registered as foreign device");
                        return Ok(());
                    }
                    Ok(code) => {
                        return Err(BacnetError::MalformedFrame(format!(
                            "BBMD registration NAK: result code 0x{code:04X}"
                        )));
                    }
                    Err(_) => {
                        // Not a BVLL-Result — might be another packet, ignore and retry
                        if attempt < 2 {
                            continue;
                        }
                    }
                },
                _ => {
                    if attempt < 2 {
                        continue;
                    }
                }
            }
        }

        Err(BacnetError::Timeout(3))
    }

    // ── Phase B8: COV subscription management ──────────────

    /// Allocate a new subscriber-process-identifier for a subscription.
    ///
    /// Wraps on overflow and skips 0 (reserved).
    fn allocate_process_id(&mut self) -> u32 {
        let id = self.next_process_id;
        self.next_process_id = self.next_process_id.wrapping_add(1);
        if self.next_process_id == 0 {
            self.next_process_id = 1; // 0 is reserved
        }
        id
    }

    /// Send a SubscribeCOV-Request and await the Simple-ACK.
    ///
    /// Returns `Ok(())` on Simple-ACK, [`DriverError::RemoteStatus`] on
    /// Error PDU, and [`DriverError::CommFault`] on timeout / socket error.
    ///
    /// Does NOT track the subscription in `cov_subscriptions` — caller's
    /// responsibility.
    async fn subscribe_cov(
        &mut self,
        device_addr: std::net::SocketAddr,
        process_id: u32,
        object_type: u16,
        object_instance: u32,
        issue_confirmed: bool,
        lifetime: Option<u32>,
    ) -> Result<(), DriverError> {
        // NLL split borrow of self.socket / self.transactions (different fields).
        let socket = self
            .socket
            .as_ref()
            .ok_or_else(|| DriverError::CommFault("bacnet: not connected".into()))?;

        let apdu = bacnet_transact_inner(
            socket,
            &mut self.transactions,
            &mut self.cov_cache,
            device_addr,
            |invoke_id| {
                frame::encode_subscribe_cov(
                    invoke_id,
                    process_id,
                    object_type,
                    object_instance,
                    Some(issue_confirmed),
                    lifetime,
                )
            },
            Duration::from_secs(3),
            3,
        )
        .await?;

        match apdu {
            frame::Apdu::SimpleAck { .. } => Ok(()),
            frame::Apdu::Error {
                error_class,
                error_code,
                ..
            } => Err(DriverError::RemoteStatus(format!(
                "BACnet SubscribeCOV error class={error_class} code={error_code}"
            ))),
            other => Err(DriverError::CommFault(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    /// Send a SubscribeCOV-Request in CANCEL form (no confirmed/lifetime tags).
    ///
    /// Response handling mirrors [`Self::subscribe_cov`].
    async fn unsubscribe_cov(
        &mut self,
        device_addr: std::net::SocketAddr,
        process_id: u32,
        object_type: u16,
        object_instance: u32,
    ) -> Result<(), DriverError> {
        // NLL split borrow of self.socket / self.transactions (different fields).
        let socket = self
            .socket
            .as_ref()
            .ok_or_else(|| DriverError::CommFault("bacnet: not connected".into()))?;

        let apdu = bacnet_transact_inner(
            socket,
            &mut self.transactions,
            &mut self.cov_cache,
            device_addr,
            |invoke_id| {
                // Cancel form: None for issue_confirmed and None for lifetime.
                frame::encode_subscribe_cov(
                    invoke_id,
                    process_id,
                    object_type,
                    object_instance,
                    None,
                    None,
                )
            },
            Duration::from_secs(3),
            3,
        )
        .await?;

        match apdu {
            frame::Apdu::SimpleAck { .. } => Ok(()),
            frame::Apdu::Error {
                error_class,
                error_code,
                ..
            } => Err(DriverError::RemoteStatus(format!(
                "BACnet SubscribeCOV error class={error_class} code={error_code}"
            ))),
            other => Err(DriverError::CommFault(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    // ── Phase B8.2: COV subscription renewal ──────────────

    /// Renew any COV subscriptions whose `subscribed_at + renewal_interval`
    /// has passed. Called at the start of each `sync_cur()`.
    ///
    /// Failed renewals are logged but the entry stays in the table for retry.
    async fn renew_due_subscriptions(&mut self) {
        let now = std::time::Instant::now();
        // Snapshot entries that are due (drop borrow before calling subscribe_cov).
        let mut due: Vec<(u32, std::net::SocketAddr, u16, u32, Option<u32>)> = Vec::new();

        for (process_id, sub) in &self.cov_subscriptions {
            if now.saturating_duration_since(sub.subscribed_at) < self.renewal_interval {
                continue;
            }
            let Some(device) = self.device_registry.get(sub.device_id) else {
                tracing::debug!(
                    process_id = *process_id,
                    device_id = sub.device_id,
                    "COV renewal: device not in registry, skipping"
                );
                continue;
            };
            due.push((
                *process_id,
                device.addr,
                sub.object_type,
                sub.object_instance,
                sub.lifetime,
            ));
        }

        for (process_id, device_addr, object_type, object_instance, lifetime) in due {
            match self
                .subscribe_cov(
                    device_addr,
                    process_id,
                    object_type,
                    object_instance,
                    false,
                    lifetime,
                )
                .await
            {
                Ok(()) => {
                    if let Some(sub) = self.cov_subscriptions.get_mut(&process_id) {
                        sub.subscribed_at = std::time::Instant::now();
                    }
                    tracing::info!(process_id, "COV subscription renewed");
                }
                Err(e) => {
                    tracing::warn!(
                        process_id,
                        error = %e,
                        "COV renewal failed — will retry next cycle"
                    );
                }
            }
        }
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

        // 2. Register as foreign device with BBMD (if configured).
        if let Some(bbmd) = self.bbmd_addr {
            match self.register_foreign_device(&socket, bbmd, 300).await {
                Ok(()) => {
                    self.is_foreign_device = true;
                }
                Err(e) => {
                    tracing::warn!(
                        driver = %self.id,
                        bbmd = %bbmd,
                        error = %e,
                        "BBMD registration failed — discovery will be local-broadcast only"
                    );
                    // Continue with local broadcast — non-fatal
                }
            }
        }

        // 3. Send Who-Is broadcast.
        let bcast: SocketAddr = format!("{}:{}", self.broadcast_addr, self.broadcast_port)
            .parse()
            .map_err(|e: std::net::AddrParseError| {
                DriverError::ConfigFault(format!("invalid broadcast addr: {e}"))
            })?;

        // Always send local broadcast
        if let Err(e) = discovery::send_who_is(&socket, bcast).await {
            tracing::warn!(driver = %self.id, "bacnet who-is send failed: {e}");
            // Non-fatal: we still listen for any I-Am packets that arrive.
        }

        // Additionally distribute via BBMD for remote subnets
        if self.is_foreign_device {
            if let Some(bbmd) = self.bbmd_addr {
                let who_is_apdu = [0x10, frame::SVC_UNCONFIRMED_WHOIS];
                let npdu_apdu = [&[0x01u8, 0x00][..], &who_is_apdu[..]].concat();
                let distribute = frame::encode_distribute_broadcast_to_network(&npdu_apdu);
                if let Err(e) = socket.send_to(&distribute, bbmd).await {
                    tracing::warn!(driver = %self.id, "bbmd distribute who-is failed: {e}");
                }
            }
        }

        // 4. Collect I-Am responses during the discovery window.
        let devices = discovery::collect_i_am(&socket, self.discovery_timeout).await;
        let n = devices.len();
        tracing::info!(driver = %self.id, devices = n, "BACnet discovery complete");

        // 5. Update registry.
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
        use std::collections::HashMap;

        // Clone device list to avoid holding a borrow across awaits.
        let devices: Vec<DeviceInfo> = self.device_registry.all().into_iter().cloned().collect();

        let mut grid = Vec::new();

        for device in devices {
            tracing::debug!(
                driver = %self.id,
                device = device.instance,
                "BACnet learn: reading object-list"
            );

            let object_list = match self.read_object_list(device.addr, device.instance).await {
                Ok(list) => list,
                Err(e) => {
                    tracing::warn!(
                        driver = %self.id,
                        device = device.instance,
                        error = %e,
                        "BACnet learn: could not read object-list"
                    );
                    continue;
                }
            };

            for (obj_type, instance) in object_list {
                // Determine point kind from BACnet object type:
                //   0=AI 1=AO 2=AV → Number
                //   3=BI 4=BO 5=BV → Bool
                //   everything else → skip
                let kind = match obj_type {
                    0..=2 => "Number".to_string(),
                    3..=5 => "Bool".to_string(),
                    _ => continue,
                };

                // Try to read the object name; fall back to a generated name.
                let name = self
                    .read_object_name(device.addr, obj_type, instance)
                    .await
                    .unwrap_or_else(|_| format!("{obj_type}-{instance}"));

                let mut tags = HashMap::new();
                tags.insert("deviceId".to_string(), device.instance.to_string());
                tags.insert("objectType".to_string(), obj_type.to_string());
                tags.insert("instance".to_string(), instance.to_string());

                grid.push(LearnPoint {
                    name: format!("{}-{name}", device.instance),
                    address: format!("{}:{}:{}", device.instance, obj_type, instance),
                    kind,
                    unit: None,
                    tags,
                });
            }
        }

        Ok(grid)
    }

    async fn sync_cur(&mut self, points: &[DriverPointRef], ctx: &mut SyncContext) {
        use std::collections::HashMap;

        // Renew any COV subscriptions due for refresh before reading points.
        self.renew_due_subscriptions().await;

        // Group points by device_id. Points whose object isn't configured get
        // a ConfigFault immediately. Points whose device isn't in the registry
        // get a CommFault.
        let mut by_device: HashMap<u32, Vec<(u32, object::BacnetObject)>> = HashMap::new();
        let mut results: Vec<(u32, Result<f64, DriverError>)> = Vec::with_capacity(points.len());

        for pt in points {
            match self.objects.get(&pt.point_id).cloned() {
                Some(obj) => {
                    by_device
                        .entry(obj.device_id)
                        .or_default()
                        .push((pt.point_id, obj));
                }
                None => {
                    results.push((
                        pt.point_id,
                        Err(DriverError::ConfigFault(format!(
                            "no BACnet object configured for point {}",
                            pt.point_id
                        ))),
                    ));
                }
            }
        }

        for (device_id, group) in by_device {
            let device_addr = match self.device_registry.get(device_id) {
                Some(d) => d.addr,
                None => {
                    for (pid, _) in &group {
                        results.push((
                            *pid,
                            Err(DriverError::CommFault(format!(
                                "BACnet device {device_id} not in registry"
                            ))),
                        ));
                    }
                    continue;
                }
            };

            // Phase B8.1: check COV cache before making network reads
            let mut uncached_group: Vec<(u32, object::BacnetObject)> = Vec::new();
            for (pid, obj) in group {
                if let Some(entry) = self.cov_cache.get(
                    obj.object_type,
                    obj.instance,
                    std::time::Duration::from_secs(600),
                ) {
                    // Cache hit — use the cached value
                    let raw = entry.value.to_f64().ok_or_else(|| {
                        DriverError::CommFault("bacnet: non-numeric COV value".into())
                    });
                    let final_val = raw.map(|r| r * obj.scale + obj.offset);
                    results.push((pid, final_val));
                } else {
                    uncached_group.push((pid, obj));
                }
            }
            let group = uncached_group;
            if group.is_empty() {
                continue;
            }

            if group.len() == 1 {
                // Single point — use the simpler read_present_value path.
                let (pid, obj) = &group[0];
                let res = self.read_present_value(device_addr, obj).await;
                results.push((*pid, res));
            } else {
                // Multiple points — try RPM, fall back to individual reads on error.
                let specs: Vec<frame::RpmRequestSpec> = group
                    .iter()
                    .map(|(_, obj)| frame::RpmRequestSpec {
                        object_type: obj.object_type,
                        instance: obj.instance,
                        property_id: 85, // PresentValue
                        array_index: None,
                    })
                    .collect();

                match self.read_properties_multiple(device_addr, &specs).await {
                    Ok(values) => {
                        for ((pid, obj), val_res) in group.iter().zip(values.into_iter()) {
                            let final_res = match val_res {
                                Ok(v) => v
                                    .to_f64()
                                    .ok_or_else(|| {
                                        DriverError::CommFault("bacnet: non-numeric value".into())
                                    })
                                    .map(|raw| raw * obj.scale + obj.offset),
                                Err(e) => Err(e),
                            };
                            results.push((*pid, final_res));
                        }
                    }
                    Err(e) => {
                        // RPM not supported or failed — fall back to individual reads.
                        tracing::debug!(
                            error = %e,
                            "BACnet RPM failed, falling back to individual ReadProperty"
                        );
                        for (pid, obj) in &group {
                            let res = self.read_present_value(device_addr, obj).await;
                            results.push((*pid, res));
                        }
                    }
                }
            }
        }

        for (pid, res) in results {
            match res {
                Ok(v) => ctx.update_cur_ok(pid, v),
                Err(e) => ctx.update_cur_err(pid, e),
            }
        }
    }

    async fn write(&mut self, writes: &[(u32, f64)], ctx: &mut WriteContext) {
        let mut results = Vec::with_capacity(writes.len());

        for &(point_id, val) in writes {
            // Look up the BacnetObject config for this Sandstar point.
            let obj = match self.objects.get(&point_id).cloned() {
                Some(o) => o,
                None => {
                    results.push((
                        point_id,
                        Err(DriverError::ConfigFault(format!(
                            "no BACnet object configured for point {point_id}"
                        ))),
                    ));
                    continue;
                }
            };

            // Look up the device's UDP address.
            let device_addr = match self.device_registry.get(obj.device_id) {
                Some(d) => d.addr,
                None => {
                    results.push((
                        point_id,
                        Err(DriverError::CommFault(format!(
                            "BACnet device {} not in registry",
                            obj.device_id
                        ))),
                    ));
                    continue;
                }
            };

            // Invert the sync_cur conversion: val = raw * scale + offset,
            // so raw = (val - offset) / scale.
            let raw = if obj.scale != 0.0 {
                (val - obj.offset) / obj.scale
            } else {
                val
            };

            // Choose the BACnet application tag based on object type:
            //   0=AI, 1=AO, 2=AV → Real
            //   3=BI, 4=BO, 5=BV → Enumerated (0=inactive, 1=active)
            let bv = match obj.object_type {
                0..=2 => value::BacnetValue::Real(raw as f32),
                3..=5 => value::BacnetValue::Enumerated(if raw != 0.0 { 1 } else { 0 }),
                _ => {
                    results.push((
                        point_id,
                        Err(DriverError::ConfigFault(format!(
                            "unsupported object type {} for write",
                            obj.object_type
                        ))),
                    ));
                    continue;
                }
            };

            // Priority 16 = lowest; a higher-priority command automatically
            // wins via BACnet's priority-array mechanism.
            let result = self
                .write_property(device_addr, obj.object_type, obj.instance, 85, bv, Some(16))
                .await;
            results.push((point_id, result));
        }

        for (pid, res) in results {
            match res {
                Ok(()) => ctx.update_write_ok(pid),
                Err(e) => ctx.update_write_err(pid, e),
            }
        }
    }

    fn poll_mode(&self) -> PollMode {
        PollMode::Buckets
    }

    /// Subscribe to COV notifications for the given points (Phase B8).
    async fn on_watch(&mut self, points: &[DriverPointRef]) -> Result<(), DriverError> {
        for pt in points {
            // Skip if already subscribed for this point.
            if self
                .cov_subscriptions
                .values()
                .any(|s| s.point_id == pt.point_id)
            {
                continue;
            }

            // Look up the object config.
            let obj = match self.objects.get(&pt.point_id).cloned() {
                Some(o) => o,
                None => {
                    tracing::warn!(
                        point_id = pt.point_id,
                        "on_watch: no BACnet object configured"
                    );
                    continue;
                }
            };

            // Look up the device address.
            let device_addr = match self.device_registry.get(obj.device_id) {
                Some(d) => d.addr,
                None => {
                    tracing::warn!(
                        point_id = pt.point_id,
                        device_id = obj.device_id,
                        "on_watch: BACnet device not in registry"
                    );
                    continue;
                }
            };

            // Allocate process_id and subscribe.
            let process_id = self.allocate_process_id();
            let lifetime = Some(300u32); // 5 minutes — renewal is future Phase B8.1.

            match self
                .subscribe_cov(
                    device_addr,
                    process_id,
                    obj.object_type,
                    obj.instance,
                    false, // unconfirmed notifications
                    lifetime,
                )
                .await
            {
                Ok(()) => {
                    self.cov_subscriptions.insert(
                        process_id,
                        CovSubscription {
                            point_id: pt.point_id,
                            process_id,
                            object_type: obj.object_type,
                            object_instance: obj.instance,
                            device_id: obj.device_id,
                            lifetime,
                            subscribed_at: std::time::Instant::now(),
                        },
                    );
                    tracing::info!(point_id = pt.point_id, process_id, "BACnet COV subscribed");
                }
                Err(e) => {
                    tracing::warn!(
                        point_id = pt.point_id,
                        process_id,
                        error = %e,
                        "BACnet COV subscribe failed"
                    );
                }
            }
        }
        Ok(())
    }

    /// Cancel COV subscriptions for the given points (Phase B8).
    async fn on_unwatch(&mut self, points: &[DriverPointRef]) -> Result<(), DriverError> {
        for pt in points {
            let sub = self
                .cov_subscriptions
                .values()
                .find(|s| s.point_id == pt.point_id)
                .cloned();

            let sub = match sub {
                Some(s) => s,
                None => continue,
            };

            let device_addr = match self.device_registry.get(sub.device_id) {
                Some(d) => d.addr,
                None => {
                    self.cov_subscriptions.remove(&sub.process_id);
                    self.cov_cache.remove(sub.object_type, sub.object_instance);
                    continue;
                }
            };

            match self
                .unsubscribe_cov(
                    device_addr,
                    sub.process_id,
                    sub.object_type,
                    sub.object_instance,
                )
                .await
            {
                Ok(()) => {
                    self.cov_subscriptions.remove(&sub.process_id);
                    self.cov_cache.remove(sub.object_type, sub.object_instance);
                    tracing::info!(
                        point_id = pt.point_id,
                        process_id = sub.process_id,
                        "BACnet COV unsubscribed"
                    );
                }
                Err(e) => {
                    self.cov_subscriptions.remove(&sub.process_id);
                    self.cov_cache.remove(sub.object_type, sub.object_instance);
                    tracing::warn!(
                        point_id = pt.point_id,
                        process_id = sub.process_id,
                        error = %e,
                        "BACnet COV unsubscribe request failed (local state cleared anyway)"
                    );
                }
            }
        }
        Ok(())
    }
}

// ── Test-only helpers ──────────────────────────────────────

#[cfg(test)]
impl BacnetDriver {
    /// Test helper: call `sync_cur` and collect results as a Vec for the
    /// pre-Phase-12.0C return-tuple style used throughout existing tests.
    async fn sync_cur_vec(
        &mut self,
        points: &[DriverPointRef],
    ) -> Vec<(u32, Result<f64, DriverError>)> {
        let mut ctx = SyncContext::new();
        <Self as AsyncDriver>::sync_cur(self, points, &mut ctx).await;
        ctx.into_results()
    }

    /// Test helper: call `write` and collect results as a Vec.
    async fn write_vec(
        &mut self,
        writes: &[(u32, f64)],
    ) -> Vec<(u32, Result<(), DriverError>)> {
        let mut ctx = WriteContext::new();
        <Self as AsyncDriver>::write(self, writes, &mut ctx).await;
        ctx.into_results()
    }
}

// ── Environment config loader ──────────────────────────────

/// Parse BACnet driver configs from `SANDSTAR_BACNET_CONFIGS` env var.
///
/// Returns an empty vec if the env var is not set.
/// Returns an error if the JSON is malformed.
pub fn load_bacnet_drivers_from_env() -> Result<Vec<BacnetDriver>, String> {
    let json_str = match std::env::var("SANDSTAR_BACNET_CONFIGS") {
        Ok(s) => s,
        Err(_) => return Ok(vec![]),
    };
    let configs: Vec<BacnetConfig> = serde_json::from_str(&json_str)
        .map_err(|e| format!("SANDSTAR_BACNET_CONFIGS parse error: {e}"))?;
    Ok(configs.into_iter().map(BacnetDriver::from_config).collect())
}

// ── End-to-end integration test (Phase B5) ────────────────
#[cfg(test)]
mod e2e_test;

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
    async fn bacnet_learn_empty_registry_returns_empty_grid() {
        // With no discovered devices, learn() must return Ok([]).
        let mut d = BacnetDriver::new("bac-2", "255.255.255.255", DEFAULT_BACNET_PORT);
        let grid = d.learn(None).await.expect("learn should succeed");
        assert!(grid.is_empty(), "expected empty grid, got {:?}", grid);
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
        let mut sctx = SyncContext::new();
        d.sync_cur(&[], &mut sctx).await;
        assert!(sctx.results().is_empty());
        let mut wctx = WriteContext::new();
        d.write(&[], &mut wctx).await;
        assert!(wctx.results().is_empty());
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
            bbmd: None,
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
            bbmd: None,
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

// ── Phase B3 unit tests ────────────────────────────────────

#[cfg(test)]
mod sync_cur_unit_tests {
    use super::*;

    fn make_driver() -> BacnetDriver {
        BacnetDriver::new("b3-test", "127.0.0.1", 0)
    }

    fn make_point(point_id: u32) -> DriverPointRef {
        DriverPointRef {
            point_id,
            address: String::new(),
        }
    }

    fn insert_object(driver: &mut BacnetDriver, point_id: u32, device_id: u32) {
        driver.add_object(
            point_id,
            object::BacnetObject {
                device_id,
                object_type: 0, // Analog Input
                instance: 1,
                scale: 1.0,
                offset: 0.0,
                unit: None,
            },
        );
    }

    fn insert_device(driver: &mut BacnetDriver, device_id: u32, port: u16) {
        driver.device_registry.insert(DeviceInfo {
            instance: device_id,
            addr: format!("127.0.0.1:{port}").parse().unwrap(),
            max_apdu: 1476,
            vendor_id: 8,
            segmentation: 3,
        });
    }

    // ── Test 1 ─────────────────────────────────────────────

    #[tokio::test]
    async fn sync_cur_empty_points_returns_empty() {
        let mut d = make_driver();
        let result = d.sync_cur_vec(&[]).await;
        assert!(result.is_empty(), "empty slice must produce empty result");
    }

    // ── Test 2 ─────────────────────────────────────────────

    #[tokio::test]
    async fn sync_cur_unknown_point_returns_config_fault() {
        let mut d = make_driver();
        // No objects registered — any point_id must yield ConfigFault.
        let pts = [make_point(9999)];
        let result = d.sync_cur_vec(&pts).await;
        assert_eq!(result.len(), 1);
        let (id, err) = &result[0];
        assert_eq!(*id, 9999);
        assert!(
            matches!(err, Err(DriverError::ConfigFault(_))),
            "expected ConfigFault, got {err:?}"
        );
    }

    // ── Test 3 ─────────────────────────────────────────────

    #[tokio::test]
    async fn sync_cur_device_not_in_registry_returns_comm_fault() {
        let mut d = make_driver();
        // Object is registered but its device is NOT in the registry.
        insert_object(&mut d, 100, 42);
        let pts = [make_point(100)];
        let result = d.sync_cur_vec(&pts).await;
        assert_eq!(result.len(), 1);
        let (id, err) = &result[0];
        assert_eq!(*id, 100);
        assert!(
            matches!(err, Err(DriverError::CommFault(_))),
            "expected CommFault, got {err:?}"
        );
    }

    // ── Test 4 ─────────────────────────────────────────────

    #[test]
    fn apdu_invoke_id_extracts_correctly() {
        // IAm and WhoIs carry no invoke ID.
        let iam = frame::Apdu::IAm {
            device_instance: 1,
            max_apdu: 1476,
            segmentation: 3,
            vendor_id: 8,
        };
        assert_eq!(apdu_invoke_id(&iam), None);

        let whois = frame::Apdu::WhoIs {
            low_limit: None,
            high_limit: None,
        };
        assert_eq!(apdu_invoke_id(&whois), None);

        // Confirmed PDUs carry an invoke ID.
        let ack = frame::Apdu::ReadPropertyAck {
            invoke_id: 42,
            object_type: 0,
            instance: 1,
            property_id: 85,
            value: value::BacnetValue::Real(23.5),
        };
        assert_eq!(apdu_invoke_id(&ack), Some(42));

        let err = frame::Apdu::Error {
            invoke_id: 7,
            service_choice: 0x0C,
            error_class: 2,
            error_code: 31,
        };
        assert_eq!(apdu_invoke_id(&err), Some(7));

        let other = frame::Apdu::Other {
            pdu_type: 0x30,
            invoke_id: 5,
            data: vec![],
        };
        assert_eq!(apdu_invoke_id(&other), Some(5));
    }

    // ── Test 5 ─────────────────────────────────────────────

    #[tokio::test]
    async fn read_present_value_applies_scale_and_offset() {
        use std::net::TcpListener;

        // Find a free port via TCP probe.
        fn find_free_port() -> u16 {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        }

        let mock_port = find_free_port();

        // Spawn a mock UDP server that replies with a ReadPropertyAck.
        tokio::spawn(async move {
            let sock = tokio::net::UdpSocket::bind(format!("127.0.0.1:{mock_port}"))
                .await
                .expect("mock bind");
            let mut buf = [0u8; 1500];
            if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                // Parse the incoming request to extract invoke_id (byte 6, index 2 of APDU).
                // BVLL(4) + NPDU(2) = 6 bytes offset; APDU[2] = invoke_id
                let invoke_id = if n >= 9 { buf[8] } else { 0 };
                // Build a ReadPropertyAck with Real(23.5) at invoke_id.
                let ack = frame::encode_read_property_ack(
                    invoke_id,
                    0,  // Analog Input
                    1,  // instance
                    85, // Present_Value
                    &value::BacnetValue::Real(23.5),
                );
                let _ = sock.send_to(&ack, from).await;
            }
        });

        // Give mock time to bind.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Build driver with mock device and open a socket.
        let mut driver = BacnetDriver::new("b3-rpv", "127.0.0.1", 0)
            .with_broadcast_port(mock_port)
            .with_discovery_timeout(Duration::from_millis(30));

        // Manually insert a device at mock_port without going through open()
        // discovery (which would consume the one Who-Is packet the mock sends).
        // Instead open with a throw-away mock listener to satisfy bind, then
        // replace the registry entry.
        let throwaway = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let throwaway_port = throwaway.local_addr().unwrap().port();
        driver.socket = Some(
            tokio::net::UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("driver socket bind"),
        );
        driver.status = DriverStatus::Ok;
        let _ = throwaway_port;

        // Register a device pointing at the mock.
        driver.device_registry.insert(DeviceInfo {
            instance: 42,
            addr: format!("127.0.0.1:{mock_port}").parse().unwrap(),
            max_apdu: 1476,
            vendor_id: 8,
            segmentation: 3,
        });

        // Register object: scale=2.0, offset=1.0  → expected = 23.5 * 2.0 + 1.0 = 48.0
        driver.add_object(
            200,
            object::BacnetObject {
                device_id: 42,
                object_type: 0,
                instance: 1,
                scale: 2.0,
                offset: 1.0,
                unit: None,
            },
        );

        let pts = [DriverPointRef {
            point_id: 200,
            address: String::new(),
        }];
        let results = driver.sync_cur_vec(&pts).await;
        assert_eq!(results.len(), 1);
        let (id, val) = &results[0];
        assert_eq!(*id, 200);
        match val {
            Ok(v) => {
                let expected = 23.5f64 * 2.0 + 1.0;
                assert!((v - expected).abs() < 0.001, "expected {expected}, got {v}");
            }
            Err(e) => panic!("expected Ok value, got Err: {e:?}"),
        }
    }
}

// ── Phase B9: ReadPropertyMultiple tests ──────────────────

#[cfg(test)]
mod rpm_sync_tests {
    use super::*;
    use std::net::TcpListener;

    fn make_driver() -> BacnetDriver {
        BacnetDriver::new("b9-test", "127.0.0.1", 0)
    }

    fn make_point(point_id: u32) -> DriverPointRef {
        DriverPointRef {
            point_id,
            address: String::new(),
        }
    }

    fn insert_object(driver: &mut BacnetDriver, point_id: u32, device_id: u32, instance: u32) {
        driver.add_object(
            point_id,
            object::BacnetObject {
                device_id,
                object_type: 0, // Analog Input
                instance,
                scale: 1.0,
                offset: 0.0,
                unit: None,
            },
        );
    }

    fn insert_device(driver: &mut BacnetDriver, device_id: u32, port: u16) {
        driver.device_registry.insert(DeviceInfo {
            instance: device_id,
            addr: format!("127.0.0.1:{port}").parse().unwrap(),
            max_apdu: 1476,
            vendor_id: 8,
            segmentation: 3,
        });
    }

    fn find_free_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    /// Parse invoke_id from an APDU byte slice that starts at BVLL byte 0.
    /// For Confirmed-Request: byte offset 8 (BVLL 4 + NPDU 2 + APDU hdr 2).
    fn parse_invoke_id_confirmed(buf: &[u8]) -> u8 {
        if buf.len() >= 9 {
            buf[8]
        } else {
            0
        }
    }

    /// Parse service choice from a Confirmed-Request APDU.
    fn parse_service_choice(buf: &[u8]) -> u8 {
        if buf.len() >= 10 {
            buf[9]
        } else {
            0
        }
    }

    // ── Test 1: single point path ──────────────────────────

    #[tokio::test]
    async fn sync_cur_single_point_uses_read_present_value() {
        // Single configured point with no registered device — should yield
        // a CommFault (device not in registry) but preserve the point_id.
        let mut d = make_driver();
        insert_object(&mut d, 500, 42, 1);
        let pts = [make_point(500)];
        let result = d.sync_cur_vec(&pts).await;
        assert_eq!(result.len(), 1);
        let (id, err) = &result[0];
        assert_eq!(*id, 500);
        assert!(
            matches!(err, Err(DriverError::CommFault(_))),
            "expected CommFault (device not in registry), got {err:?}"
        );
    }

    // ── Test 2: grouping by device ─────────────────────────

    #[tokio::test]
    async fn sync_cur_groups_points_by_device() {
        // 3 points: 2 on device 10, 1 on device 20 — no devices registered,
        // so each group emits CommFault. We verify every point_id is present
        // in the results regardless of grouping order.
        let mut d = make_driver();
        insert_object(&mut d, 601, 10, 1);
        insert_object(&mut d, 602, 10, 2);
        insert_object(&mut d, 603, 20, 5);

        let pts = [make_point(601), make_point(602), make_point(603)];
        let result = d.sync_cur_vec(&pts).await;
        assert_eq!(result.len(), 3);
        let ids: Vec<u32> = result.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&601));
        assert!(ids.contains(&602));
        assert!(ids.contains(&603));
        for (_, r) in &result {
            assert!(
                matches!(r, Err(DriverError::CommFault(_))),
                "expected CommFault, got {r:?}"
            );
        }
    }

    // ── Test 3: unknown point still returns ConfigFault ────

    #[tokio::test]
    async fn sync_cur_unknown_point_still_returns_config_fault() {
        let mut d = make_driver();
        let pts = [make_point(9001)];
        let result = d.sync_cur_vec(&pts).await;
        assert_eq!(result.len(), 1);
        let (id, err) = &result[0];
        assert_eq!(*id, 9001);
        assert!(
            matches!(err, Err(DriverError::ConfigFault(_))),
            "expected ConfigFault, got {err:?}"
        );
    }

    // ── Test 4: device not in registry → CommFault for group ──

    #[tokio::test]
    async fn sync_cur_device_not_in_registry_returns_comm_fault_for_group() {
        let mut d = make_driver();
        // Two points both mapped to device_id=42, no device registered.
        insert_object(&mut d, 701, 42, 1);
        insert_object(&mut d, 702, 42, 2);

        let pts = [make_point(701), make_point(702)];
        let result = d.sync_cur_vec(&pts).await;
        assert_eq!(result.len(), 2);
        for (_, r) in &result {
            assert!(
                matches!(r, Err(DriverError::CommFault(_))),
                "expected CommFault, got {r:?}"
            );
        }
        let ids: Vec<u32> = result.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&701));
        assert!(ids.contains(&702));
    }

    // ── Test 5: mock RPM happy-path ────────────────────────

    #[tokio::test]
    async fn read_properties_multiple_mock_success() {
        let mock_port = find_free_port();

        // Spawn a mock UDP server that replies to a single RPM request with
        // an RPM-ACK containing two AI values: AI-0 -> 10.0, AI-1 -> 20.0.
        tokio::spawn(async move {
            let sock = tokio::net::UdpSocket::bind(format!("127.0.0.1:{mock_port}"))
                .await
                .expect("mock bind");
            let mut buf = [0u8; 1500];
            if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let invoke_id = parse_invoke_id_confirmed(&buf[..n]);
                let results = vec![
                    frame::RpmResult {
                        object_type: 0,
                        instance: 0,
                        property_id: 85,
                        array_index: None,
                        value: Ok(value::BacnetValue::Real(10.0)),
                    },
                    frame::RpmResult {
                        object_type: 0,
                        instance: 1,
                        property_id: 85,
                        array_index: None,
                        value: Ok(value::BacnetValue::Real(20.0)),
                    },
                ];
                let ack = frame::encode_read_property_multiple_ack(invoke_id, &results);
                let _ = sock.send_to(&ack, from).await;
            }
        });

        // Give the mock time to bind.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Build driver with a real socket and point it at the mock.
        let mut driver = make_driver();
        driver.socket = Some(
            tokio::net::UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("driver socket bind"),
        );
        driver.status = DriverStatus::Ok;
        insert_device(&mut driver, 42, mock_port);

        let device_addr = driver.device_registry.get(42).unwrap().addr;
        let specs = vec![
            frame::RpmRequestSpec {
                object_type: 0,
                instance: 0,
                property_id: 85,
                array_index: None,
            },
            frame::RpmRequestSpec {
                object_type: 0,
                instance: 1,
                property_id: 85,
                array_index: None,
            },
        ];

        let result = driver
            .read_properties_multiple(device_addr, &specs)
            .await
            .expect("rpm should succeed");
        assert_eq!(result.len(), 2);
        match &result[0] {
            Ok(value::BacnetValue::Real(v)) => assert!((v - 10.0).abs() < 0.001),
            other => panic!("expected Real(10.0), got {other:?}"),
        }
        match &result[1] {
            Ok(value::BacnetValue::Real(v)) => assert!((v - 20.0).abs() < 0.001),
            other => panic!("expected Real(20.0), got {other:?}"),
        }
    }

    // ── Test 6: mock replies with Error PDU ────────────────

    #[tokio::test]
    async fn read_properties_multiple_mock_batch_error_pdu() {
        let mock_port = find_free_port();

        tokio::spawn(async move {
            let sock = tokio::net::UdpSocket::bind(format!("127.0.0.1:{mock_port}"))
                .await
                .expect("mock bind");
            let mut buf = [0u8; 1500];
            if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let invoke_id = parse_invoke_id_confirmed(&buf[..n]);
                let err = frame::encode_error_pdu(
                    invoke_id,
                    frame::SVC_CONFIRMED_READ_PROPERTY_MULTIPLE,
                    2,
                    31,
                );
                let _ = sock.send_to(&err, from).await;
            }
        });

        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut driver = make_driver();
        driver.socket = Some(
            tokio::net::UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("driver socket bind"),
        );
        driver.status = DriverStatus::Ok;
        insert_device(&mut driver, 42, mock_port);

        let device_addr = driver.device_registry.get(42).unwrap().addr;
        let specs = vec![frame::RpmRequestSpec {
            object_type: 0,
            instance: 0,
            property_id: 85,
            array_index: None,
        }];

        let result = driver.read_properties_multiple(device_addr, &specs).await;
        match result {
            Err(BacnetError::RemoteError { class, code }) => {
                assert_eq!(class, 2);
                assert_eq!(code, 31);
            }
            other => panic!("expected RemoteError, got {other:?}"),
        }
    }

    // ── Test 7: sync_cur fallback to individual reads ─────

    #[tokio::test]
    async fn sync_cur_falls_back_to_individual_reads_on_rpm_error() {
        let mock_port = find_free_port();

        // Mock responds to:
        //   1. RPM request → Error PDU (service = 0x0E)
        //   2. First ReadProperty request → ReadPropertyAck(Real 30.0)
        //   3. Second ReadProperty request → ReadPropertyAck(Real 40.0)
        tokio::spawn(async move {
            let sock = tokio::net::UdpSocket::bind(format!("127.0.0.1:{mock_port}"))
                .await
                .expect("mock bind");
            let mut buf = [0u8; 1500];

            // 1: RPM request → Error PDU
            if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let invoke_id = parse_invoke_id_confirmed(&buf[..n]);
                let svc = parse_service_choice(&buf[..n]);
                // Only respond with Error if it's actually an RPM request.
                if svc == frame::SVC_CONFIRMED_READ_PROPERTY_MULTIPLE {
                    let err = frame::encode_error_pdu(invoke_id, svc, 2, 31);
                    let _ = sock.send_to(&err, from).await;
                }
            }

            // 2 & 3: two ReadProperty requests → acks with real values.
            for value_f in [30.0f32, 40.0f32] {
                if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                    let invoke_id = parse_invoke_id_confirmed(&buf[..n]);
                    // Read object_type (bytes 11..13) and instance from the request
                    // to mirror it back correctly. Instance is encoded in the
                    // low 22 bits of the 4-byte ObjectId at offset 11.
                    let (object_type, instance) = if n >= 15 && buf[10] == 0x0C {
                        let raw = u32::from_be_bytes([buf[11], buf[12], buf[13], buf[14]]);
                        (((raw >> 22) & 0x3FF) as u16, raw & 0x003F_FFFF)
                    } else {
                        (0, 0)
                    };
                    let ack = frame::encode_read_property_ack(
                        invoke_id,
                        object_type,
                        instance,
                        85,
                        &value::BacnetValue::Real(value_f),
                    );
                    let _ = sock.send_to(&ack, from).await;
                }
            }
        });

        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut driver = make_driver();
        driver.socket = Some(
            tokio::net::UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("driver socket bind"),
        );
        driver.status = DriverStatus::Ok;
        insert_device(&mut driver, 42, mock_port);
        // Two points on the same device → triggers the RPM path.
        insert_object(&mut driver, 801, 42, 0);
        insert_object(&mut driver, 802, 42, 1);

        let pts = [make_point(801), make_point(802)];
        let results = driver.sync_cur_vec(&pts).await;
        assert_eq!(results.len(), 2);
        // Build a map so we don't depend on iteration order (HashMap).
        let map: std::collections::HashMap<u32, &Result<f64, DriverError>> =
            results.iter().map(|(id, r)| (*id, r)).collect();

        let r801 = map.get(&801).expect("801 missing").as_ref();
        let r802 = map.get(&802).expect("802 missing").as_ref();

        match r801 {
            Ok(v) => assert!(
                (*v - 30.0).abs() < 0.001 || (*v - 40.0).abs() < 0.001,
                "expected 30 or 40, got {v}"
            ),
            Err(e) => panic!("expected Ok for 801, got {e:?}"),
        }
        match r802 {
            Ok(v) => assert!(
                (*v - 30.0).abs() < 0.001 || (*v - 40.0).abs() < 0.001,
                "expected 30 or 40, got {v}"
            ),
            Err(e) => panic!("expected Ok for 802, got {e:?}"),
        }
    }
}

// ── Integration tests ──────────────────────────────────────

#[cfg(test)]
mod discovery_integration {
    use super::*;

    // ── Helpers ────────────────────────────────────────────

    /// Bind a UDP socket on an OS-assigned free port (127.0.0.1). Returns
    /// both the bound socket AND the port number it got, so callers don't
    /// race with other tests.
    ///
    /// This replaces an earlier `find_free_port() -> u16` helper that
    /// bound TCP-0, dropped it, then asked tests to re-bind UDP. Under
    /// parallel workspace load that opened a TOCTOU window where a
    /// competing test could grab the same port before this test's UDP
    /// bind — the fix keeps the socket alive through the test lifetime.
    async fn bind_mock_udp() -> (tokio::net::UdpSocket, u16) {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind UDP 127.0.0.1:0");
        let port = sock.local_addr().expect("local_addr").port();
        (sock, port)
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

    /// Spawn a UDP task on an OS-assigned free port that waits for ONE
    /// packet (the Who-Is), then replies with an I-Am from
    /// `device_instance`. Returns the bound port so the caller can point
    /// the driver's `with_broadcast_port` at it.
    async fn spawn_mock_device(device_instance: u32) -> (tokio::task::JoinHandle<()>, u16) {
        let (sock, port) = bind_mock_udp().await;
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            if let Ok((_, from)) = sock.recv_from(&mut buf).await {
                let reply = encode_i_am(device_instance, 1476, 3, 8);
                let _ = sock.send_to(&reply, from).await;
            }
        });
        (handle, port)
    }

    // ── Tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_open_with_no_response_succeeds_but_registry_empty() {
        // Listen on a mock port but never respond — driver should still return Ok.
        let (_sock, mock_port) = bind_mock_udp().await;

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
        let device_instance = 12345u32;
        let (_handle, mock_port) = spawn_mock_device(device_instance).await;

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
        let (sock, mock_port) = bind_mock_udp().await;

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
        let (_sock, mock_port) = bind_mock_udp().await;

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
        let (_handle, mock_port) = spawn_mock_device(9999).await;

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

// ── Phase B4 learn() tests ─────────────────────────────────

#[cfg(test)]
mod learn_tests {
    use super::*;

    // ── Test 1 ─────────────────────────────────────────────

    /// learn() with no devices in registry returns empty grid immediately.
    #[tokio::test]
    async fn learn_with_no_devices_returns_empty_grid() {
        let mut driver = BacnetDriver::new("b4-empty", "127.0.0.1", 0);
        let grid = driver.learn(None).await.expect("learn should succeed");
        assert!(grid.is_empty(), "expected empty grid, got {grid:?}");
    }

    // ── Test 2 ─────────────────────────────────────────────

    /// Unit test for ObjectId filtering logic used by read_object_list.
    /// Verifies that Device objects (type 8) are excluded and data objects
    /// (types 0-5) are kept.
    #[test]
    fn read_object_list_parses_array_of_object_ids() {
        let items = vec![
            value::BacnetValue::ObjectId {
                object_type: 0,
                instance: 1,
            },
            value::BacnetValue::ObjectId {
                object_type: 8,
                instance: 99,
            }, // Device — should be skipped
            value::BacnetValue::ObjectId {
                object_type: 3,
                instance: 0,
            },
        ];
        let filtered: Vec<(u16, u32)> = items
            .into_iter()
            .filter_map(|v| match v {
                value::BacnetValue::ObjectId {
                    object_type,
                    instance,
                } if object_type != 8 => Some((object_type, instance)),
                _ => None,
            })
            .collect();
        assert_eq!(filtered, vec![(0, 1), (3, 0)]);
    }

    // ── Test 3 ─────────────────────────────────────────────

    /// Integration test: mock device responds to object-list and name queries.
    ///
    /// The mock handles (in order):
    ///   1. Who-Is → I-Am (device 1001)
    ///   2. ReadProperty(Device 1001, ObjectList=76) → [AI-0, AO-1, BI-0]
    ///   3. ReadProperty(AI 0, ObjectName=77) → "TempSensor"
    ///   4. ReadProperty(AO 1, ObjectName=77) → "Valve"
    ///   5. ReadProperty(BI 0, ObjectName=77) → "MotionSensor"
    #[tokio::test]
    async fn learn_integration_with_mock_device() {
        use std::net::TcpListener;

        fn find_free_port() -> u16 {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        }

        let mock_port = find_free_port();
        let device_instance = 1001u32;

        // Spawn mock that handles Who-Is → I-Am, then object-list and name reads.
        tokio::spawn(async move {
            let sock = tokio::net::UdpSocket::bind(format!("127.0.0.1:{mock_port}"))
                .await
                .expect("mock bind");
            let mut buf = [0u8; 1500];

            // 1. Respond to Who-Is with I-Am
            if let Ok((_, from)) = sock.recv_from(&mut buf).await {
                let obj_id_val: u32 = (8u32 << 22) | (device_instance & 0x3F_FFFF);
                let obj_id = obj_id_val.to_be_bytes();
                let mut apdu: Vec<u8> = vec![
                    0x10, 0x00, 0xC4, obj_id[0], obj_id[1], obj_id[2], obj_id[3], 0x22, 0x05,
                    0xC4, // max-apdu = 1476
                    0x91, 0x03, // segmentation = 3
                    0x21, 0x08, // vendor-id = 8
                ];
                let total_len = (4u16 + 2 + apdu.len() as u16).to_be_bytes();
                let mut frame = vec![0x81, 0x0A, total_len[0], total_len[1], 0x01, 0x00];
                frame.append(&mut apdu);
                let _ = sock.send_to(&frame, from).await;
            }

            // Helper to extract invoke_id from a received ReadProperty request.
            // Packet layout: BVLL(4) + NPDU(2) + APDU; APDU[2] = invoke_id.
            let get_invoke_id = |data: &[u8], n: usize| -> u8 {
                if n >= 9 {
                    data[8]
                } else {
                    0
                }
            };

            // 2. Object-list request → respond with AI-0, AO-1, BI-0
            if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let invoke_id = get_invoke_id(&buf, n);
                let object_list_values = vec![
                    value::BacnetValue::ObjectId {
                        object_type: 0,
                        instance: 0,
                    }, // AI-0
                    value::BacnetValue::ObjectId {
                        object_type: 1,
                        instance: 1,
                    }, // AO-1
                    value::BacnetValue::ObjectId {
                        object_type: 3,
                        instance: 0,
                    }, // BI-0
                ];
                let ack = frame::encode_read_property_ack_multi(
                    invoke_id,
                    8, // Device object
                    device_instance,
                    76, // ObjectList property
                    &object_list_values,
                );
                let _ = sock.send_to(&ack, from).await;
            }

            // Name requests: AI-0 → "TempSensor", AO-1 → "Valve", BI-0 → "MotionSensor"
            // Use encode_read_property_ack_multi which fully handles CharacterString.
            let names = ["TempSensor", "Valve", "MotionSensor"];
            let obj_types = [0u16, 1u16, 3u16];
            let instances = [0u32, 1u32, 0u32];
            for i in 0..3 {
                if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                    let invoke_id = get_invoke_id(&buf, n);
                    let name_val = value::BacnetValue::CharacterString(names[i].to_string());
                    let ack = frame::encode_read_property_ack_multi(
                        invoke_id,
                        obj_types[i],
                        instances[i],
                        77,
                        &[name_val],
                    );
                    let _ = sock.send_to(&ack, from).await;
                }
            }
        });

        // Give mock time to bind.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let mut driver = BacnetDriver::new("b4-integ", "127.0.0.1", 0)
            .with_broadcast_port(mock_port)
            .with_discovery_timeout(std::time::Duration::from_millis(200));

        driver.open().await.expect("open should succeed");
        assert_eq!(
            driver.device_registry().len(),
            1,
            "should have discovered 1 device"
        );

        let grid = driver.learn(None).await.expect("learn should succeed");
        assert_eq!(grid.len(), 3, "expected 3 points, got {grid:?}");

        // Check names/kinds
        let names_found: Vec<_> = grid.iter().map(|p| p.name.as_str()).collect();
        assert!(
            names_found.contains(&"1001-TempSensor"),
            "missing TempSensor: {names_found:?}"
        );
        assert!(
            names_found.contains(&"1001-Valve"),
            "missing Valve: {names_found:?}"
        );
        assert!(
            names_found.contains(&"1001-MotionSensor"),
            "missing MotionSensor: {names_found:?}"
        );

        // Check kinds
        let temp = grid.iter().find(|p| p.name == "1001-TempSensor").unwrap();
        assert_eq!(temp.kind, "Number");
        let valve = grid.iter().find(|p| p.name == "1001-Valve").unwrap();
        assert_eq!(valve.kind, "Number");
        let motion = grid.iter().find(|p| p.name == "1001-MotionSensor").unwrap();
        assert_eq!(motion.kind, "Bool");

        driver.close().await;
    }

    // ── Test 4 ─────────────────────────────────────────────

    /// Verify object type filtering: types 0-5 → included, others → skipped.
    #[test]
    fn learn_skips_device_objects_and_unknown_types() {
        // Simulate the filtering logic used in learn()
        let candidates: &[(u16, &str)] = &[
            (0, "Number"), // AI
            (1, "Number"), // AO
            (2, "Number"), // AV
            (3, "Bool"),   // BI
            (4, "Bool"),   // BO
            (5, "Bool"),   // BV
            (6, "skip"),   // Multi-state input
            (7, "skip"),   // Multi-state output
            (8, "skip"),   // Device
            (20, "skip"),  // Trendlog
        ];

        let included: Vec<(u16, &str)> = candidates
            .iter()
            .filter_map(|(obj_type, expected_kind)| {
                let kind = match *obj_type {
                    0..=2 => "Number",
                    3..=5 => "Bool",
                    _ => return None,
                };
                assert_eq!(kind, *expected_kind, "wrong kind for type {obj_type}");
                Some((*obj_type, kind))
            })
            .collect();

        assert_eq!(
            included.len(),
            6,
            "expected 6 included types, got {included:?}"
        );
        for (ot, k) in &included {
            match ot {
                0..=2 => assert_eq!(*k, "Number"),
                3..=5 => assert_eq!(*k, "Bool"),
                _ => panic!("unexpected type {ot} in included"),
            }
        }
    }
}

// ── Phase B5 config loader tests ───────────────────────────

#[cfg(test)]
mod config_tests {
    use super::*;
    use std::sync::Mutex;

    /// Mutex that serialises all tests that mutate `SANDSTAR_BACNET_CONFIGS`.
    /// Tests 1-7 don't touch the env var and can run freely in parallel.
    /// Tests 8-10 acquire this lock so they don't race against each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // ── Test 1 ─────────────────────────────────────────────

    #[test]
    fn bacnet_config_deserializes_from_json_minimal() {
        let json = r#"{
            "id": "bac-1",
            "objects": []
        }"#;
        let config: BacnetConfig = serde_json::from_str(json).expect("should parse");
        assert_eq!(config.id, "bac-1");
        assert!(config.port.is_none());
        assert!(config.broadcast.is_none());
        assert!(config.objects.is_empty());
    }

    // ── Test 2 ─────────────────────────────────────────────

    #[test]
    fn bacnet_config_deserializes_from_json_full() {
        let json = r#"{
            "id": "bac-full",
            "port": 47808,
            "broadcast": "192.168.1.255",
            "objects": [
                {
                    "point_id": 1001,
                    "device_id": 42,
                    "object_type": 0,
                    "instance": 5,
                    "unit": "degF",
                    "scale": 1.8,
                    "offset": 32.0
                }
            ]
        }"#;
        let config: BacnetConfig = serde_json::from_str(json).expect("should parse");
        assert_eq!(config.port, Some(47808));
        assert_eq!(config.broadcast.as_deref(), Some("192.168.1.255"));
        assert_eq!(config.objects.len(), 1);
        let obj = &config.objects[0];
        assert_eq!(obj.point_id, 1001);
        assert_eq!(obj.device_id, 42);
        assert_eq!(obj.object_type, 0);
        assert_eq!(obj.instance, 5);
        assert_eq!(obj.unit.as_deref(), Some("degF"));
        assert_eq!(obj.scale, Some(1.8));
        assert_eq!(obj.offset, Some(32.0));
    }

    // ── Test 3 ─────────────────────────────────────────────

    #[test]
    fn bacnet_config_array_deserializes() {
        let json = r#"[
            {"id": "bac-a", "objects": []},
            {"id": "bac-b", "port": 47809, "objects": []}
        ]"#;
        let configs: Vec<BacnetConfig> = serde_json::from_str(json).expect("should parse array");
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].id, "bac-a");
        assert_eq!(configs[1].port, Some(47809));
    }

    // ── Test 4 ─────────────────────────────────────────────

    #[test]
    fn from_config_applies_defaults() {
        let config = BacnetConfig {
            id: "bac-defaults".into(),
            port: None,
            broadcast: None,
            objects: vec![],
            bbmd: None,
        };
        let driver = BacnetDriver::from_config(config);
        assert_eq!(driver.id(), "bac-defaults");
        assert!(driver.objects().is_empty());
        // Verify default port and broadcast via the private fields accessible
        // inside the same file's test modules.
        assert_eq!(driver.port, DEFAULT_BACNET_PORT);
        assert_eq!(driver.broadcast_addr, "255.255.255.255");
    }

    // ── Test 5 ─────────────────────────────────────────────

    #[test]
    fn from_config_with_objects_sets_scale_offset() {
        let config = BacnetConfig {
            id: "bac-objs".into(),
            port: Some(47808),
            broadcast: Some("255.255.255.255".into()),
            objects: vec![
                BacnetObjectConfig {
                    point_id: 500,
                    device_id: 10,
                    object_type: 0,
                    instance: 1,
                    unit: Some("psi".into()),
                    scale: Some(0.5),
                    offset: Some(-10.0),
                },
                BacnetObjectConfig {
                    point_id: 501,
                    device_id: 10,
                    object_type: 2,
                    instance: 0,
                    unit: None,
                    scale: None,
                    offset: None,
                },
            ],
            bbmd: None,
        };
        let driver = BacnetDriver::from_config(config);
        let objects = driver.objects();
        assert_eq!(objects.len(), 2);
        let obj500 = objects.get(&500).unwrap();
        assert_eq!(obj500.scale, 0.5);
        assert_eq!(obj500.offset, -10.0);
        assert_eq!(obj500.unit.as_deref(), Some("psi"));
        let obj501 = objects.get(&501).unwrap();
        assert_eq!(obj501.scale, 1.0, "default scale");
        assert_eq!(obj501.offset, 0.0, "default offset");
        assert!(obj501.unit.is_none());
    }

    // ── Test 6 ─────────────────────────────────────────────

    #[test]
    fn bacnet_config_invalid_json_fails_gracefully() {
        let bad_json = r#"{"id": "bac-bad", "port": "not-a-number", "objects": []}"#;
        let result = serde_json::from_str::<BacnetConfig>(bad_json);
        assert!(
            result.is_err(),
            "invalid port type should fail to deserialize"
        );
    }

    // ── Test 7 ─────────────────────────────────────────────

    #[test]
    fn bacnet_object_config_missing_optional_fields() {
        let json = r#"{
            "point_id": 9,
            "device_id": 1,
            "object_type": 3,
            "instance": 0
        }"#;
        let obj: BacnetObjectConfig =
            serde_json::from_str(json).expect("minimal object should parse");
        assert_eq!(obj.point_id, 9);
        assert!(obj.unit.is_none());
        assert!(obj.scale.is_none());
        assert!(obj.offset.is_none());
    }

    // ── Test 8 ─────────────────────────────────────────────

    #[test]
    fn load_bacnet_drivers_from_env_empty_when_not_set() {
        // Hold ENV_LOCK so tests 8/9/10 don't race on SANDSTAR_BACNET_CONFIGS.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // SAFETY: serialised by ENV_LOCK; no other test mutates this var concurrently.
        unsafe {
            std::env::remove_var("SANDSTAR_BACNET_CONFIGS");
        }
        let drivers = load_bacnet_drivers_from_env().expect("should succeed when env var absent");
        assert!(drivers.is_empty());
    }

    // ── Test 9 ─────────────────────────────────────────────

    #[test]
    fn load_bacnet_drivers_from_env_creates_drivers() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // SAFETY: serialised by ENV_LOCK.
        unsafe {
            std::env::set_var(
                "SANDSTAR_BACNET_CONFIGS",
                r#"[
                    {"id":"bac-env-1","port":47808,"broadcast":"255.255.255.255","objects":[]},
                    {"id":"bac-env-2","port":47809,"broadcast":"192.168.0.255","objects":[]}
                ]"#,
            );
        }
        let drivers = load_bacnet_drivers_from_env().expect("should parse");
        assert_eq!(drivers.len(), 2);
        assert_eq!(drivers[0].id(), "bac-env-1");
        assert_eq!(drivers[1].id(), "bac-env-2");
        unsafe {
            std::env::remove_var("SANDSTAR_BACNET_CONFIGS");
        }
    }

    // ── Test 10 ────────────────────────────────────────────

    #[test]
    fn load_bacnet_drivers_from_env_returns_error_on_bad_json() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // SAFETY: serialised by ENV_LOCK.
        unsafe {
            std::env::set_var("SANDSTAR_BACNET_CONFIGS", "not valid json {{{");
        }
        let result = load_bacnet_drivers_from_env();
        assert!(result.is_err());
        // Use .err().expect() instead of .unwrap_err() — the latter requires T: Debug,
        // but BacnetDriver does not derive Debug.
        let err = result.err().expect("expected Err");
        assert!(
            err.contains("parse error"),
            "error should mention parse error, got: {err}"
        );
        unsafe {
            std::env::remove_var("SANDSTAR_BACNET_CONFIGS");
        }
    }
}

// ── Phase B7 write() tests ────────────────────────────────

#[cfg(test)]
mod write_tests {
    use super::*;

    fn make_driver() -> BacnetDriver {
        BacnetDriver::new("b7-test", "127.0.0.1", 0)
    }

    /// Build a minimal driver with an open socket bound to an OS-assigned port
    /// and a registered device pointing at `mock_addr`.
    async fn make_ready_driver(mock_addr: std::net::SocketAddr) -> BacnetDriver {
        let mut d = make_driver();
        d.socket = Some(
            tokio::net::UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("bind driver socket"),
        );
        d.status = DriverStatus::Ok;
        d.device_registry.insert(DeviceInfo {
            instance: 42,
            addr: mock_addr,
            max_apdu: 1476,
            vendor_id: 999,
            segmentation: 3,
        });
        d
    }

    // ── Test 1 ─────────────────────────────────────────────

    #[tokio::test]
    async fn write_empty_returns_empty() {
        let mut d = make_driver();
        let result = d.write_vec(&[]).await;
        assert!(result.is_empty());
    }

    // ── Test 2 ─────────────────────────────────────────────

    #[tokio::test]
    async fn write_unknown_point_returns_config_fault() {
        let mut d = make_driver();
        let result = d.write_vec(&[(999, 1.0)]).await;
        assert_eq!(result.len(), 1);
        let (id, err) = &result[0];
        assert_eq!(*id, 999);
        assert!(
            matches!(err, Err(DriverError::ConfigFault(_))),
            "expected ConfigFault, got {err:?}"
        );
    }

    // ── Test 3 ─────────────────────────────────────────────

    #[tokio::test]
    async fn write_device_not_in_registry_returns_comm_fault() {
        let mut d = make_driver();
        d.add_object(
            100,
            object::BacnetObject {
                device_id: 42,
                object_type: 1, // Analog Output
                instance: 5,
                scale: 1.0,
                offset: 0.0,
                unit: None,
            },
        );
        // Note: registry is empty — device 42 is unknown.
        let result = d.write_vec(&[(100, 50.0)]).await;
        assert_eq!(result.len(), 1);
        let (id, err) = &result[0];
        assert_eq!(*id, 100);
        assert!(
            matches!(err, Err(DriverError::CommFault(_))),
            "expected CommFault, got {err:?}"
        );
    }

    // ── Test 4 ─────────────────────────────────────────────

    #[tokio::test]
    async fn write_object_type_out_of_range_returns_config_fault() {
        let mut d = make_driver();
        d.add_object(
            200,
            object::BacnetObject {
                device_id: 42,
                object_type: 99, // unsupported
                instance: 1,
                scale: 1.0,
                offset: 0.0,
                unit: None,
            },
        );
        d.device_registry.insert(DeviceInfo {
            instance: 42,
            addr: "127.0.0.1:47808".parse().unwrap(),
            max_apdu: 1476,
            vendor_id: 8,
            segmentation: 3,
        });
        let result = d.write_vec(&[(200, 1.0)]).await;
        assert_eq!(result.len(), 1);
        let (_id, err) = &result[0];
        match err {
            Err(DriverError::ConfigFault(msg)) => {
                assert!(
                    msg.contains("unsupported object type"),
                    "msg should mention unsupported object type, got: {msg}"
                );
            }
            other => panic!("expected ConfigFault, got {other:?}"),
        }
    }

    // ── Test 5 ─────────────────────────────────────────────

    #[tokio::test]
    async fn write_property_direct_mock_socket_success() {
        // Bind the mock UDP socket BEFORE spawning the task to eliminate any
        // TOCTOU race between `find_free_port` and the task's bind.
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("mock bind");
        let mock_addr: std::net::SocketAddr = sock.local_addr().unwrap();

        // Mock UDP responder: reads incoming WriteProperty, extracts the
        // invoke_id from byte [8], replies with a Simple-ACK.
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let invoke_id = if n >= 9 { buf[8] } else { 0 };
                // Sanity-check that it's a WriteProperty confirmed-request.
                // Frame layout: [BVLL(4) .. NPDU(2) .. APDU]
                //   APDU[0]=0x00 (Confirmed-Req), APDU[1]=max-seg/max-apdu,
                //   APDU[2]=invoke_id, APDU[3]=service_choice(0x0F)
                if n >= 10 {
                    assert_eq!(buf[9], 0x0F, "service choice should be WriteProperty");
                }
                let reply = frame::encode_simple_ack(invoke_id, 0x0F);
                let _ = sock.send_to(&reply, from).await;
            }
        });

        let mut driver = make_ready_driver(mock_addr).await;

        // Call write_property directly — we want to isolate the write path
        // from the public write() dispatcher.
        let result = driver
            .write_property(
                mock_addr,
                1, // Analog Output
                5,
                85, // Present_Value
                value::BacnetValue::Real(50.0),
                Some(16),
            )
            .await;

        assert!(result.is_ok(), "expected Ok(()), got {result:?}");
    }

    // ── Test 6 ─────────────────────────────────────────────

    #[tokio::test]
    async fn write_property_direct_mock_error_pdu() {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("mock bind");
        let mock_addr: std::net::SocketAddr = sock.local_addr().unwrap();

        // Mock UDP responder: replies with an Error PDU instead of Simple-ACK.
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let invoke_id = if n >= 9 { buf[8] } else { 0 };

                // Build an Error PDU:
                //   [0x50, invoke_id, service_choice,
                //    0x91, class, 0x91, code]
                // wrapped in BVLL(4)+NPDU(2).
                let apdu: Vec<u8> = vec![
                    0x50, invoke_id, 0x0F, // service choice = WriteProperty
                    0x91, 0x02, // error class = 2 (object)
                    0x91, 0x1F, // error code = 31 (write-access-denied)
                ];
                let total_len = (4u16 + 2 + apdu.len() as u16).to_be_bytes();
                let mut frame = vec![0x81, 0x0A, total_len[0], total_len[1], 0x01, 0x00];
                frame.extend_from_slice(&apdu);
                let _ = sock.send_to(&frame, from).await;
            }
        });

        let mut driver = make_ready_driver(mock_addr).await;

        let result = driver
            .write_property(
                mock_addr,
                1,
                5,
                85,
                value::BacnetValue::Real(50.0),
                Some(16),
            )
            .await;

        match result {
            Err(DriverError::RemoteStatus(msg)) => {
                assert!(
                    msg.contains("class=2") && msg.contains("code=31"),
                    "error message should mention class/code, got: {msg}"
                );
            }
            other => panic!("expected RemoteStatus, got {other:?}"),
        }
    }
}

// ── Phase B8: COV subscription tests ──────────────────────

#[cfg(test)]
mod cov_tests {
    use super::*;

    fn make_driver() -> BacnetDriver {
        BacnetDriver::new("b8-test", "127.0.0.1", 0)
    }

    /// Build a driver with an open socket and a registered device pointing
    /// at `mock_addr` (device_id = 42).
    async fn make_ready_driver(mock_addr: std::net::SocketAddr) -> BacnetDriver {
        let mut d = make_driver();
        d.socket = Some(
            tokio::net::UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("bind driver socket"),
        );
        d.status = DriverStatus::Ok;
        d.device_registry.insert(DeviceInfo {
            instance: 42,
            addr: mock_addr,
            max_apdu: 1476,
            vendor_id: 999,
            segmentation: 3,
        });
        d
    }

    // ── Test 1 ─────────────────────────────────────────────

    #[test]
    fn allocate_process_id_increments() {
        let mut d = make_driver();
        assert_eq!(d.allocate_process_id(), 1);
        assert_eq!(d.allocate_process_id(), 2);
        assert_eq!(d.allocate_process_id(), 3);
    }

    // ── Test 2 ─────────────────────────────────────────────

    #[test]
    fn allocate_process_id_skips_zero_on_wrap() {
        let mut d = make_driver();
        d.next_process_id = u32::MAX;
        assert_eq!(d.allocate_process_id(), u32::MAX);
        // Wrap: next would be 0, which is reserved — must be bumped to 1.
        assert_eq!(d.allocate_process_id(), 1);
    }

    // ── Test 3 ─────────────────────────────────────────────

    #[tokio::test]
    async fn on_watch_unknown_point_is_noop() {
        let mut d = make_driver();
        let refs = vec![DriverPointRef {
            point_id: 9999,
            address: String::new(),
        }];
        let result = d.on_watch(&refs).await;
        assert!(result.is_ok());
        assert!(d.cov_subscriptions.is_empty());
    }

    // ── Test 4 ─────────────────────────────────────────────

    #[tokio::test]
    async fn on_watch_device_not_in_registry_is_noop() {
        let mut d = make_driver();
        d.add_object(
            8001,
            object::BacnetObject {
                device_id: 77,
                object_type: 0,
                instance: 1,
                scale: 1.0,
                offset: 0.0,
                unit: None,
            },
        );
        let refs = vec![DriverPointRef {
            point_id: 8001,
            address: String::new(),
        }];
        let result = d.on_watch(&refs).await;
        assert!(result.is_ok());
        assert!(d.cov_subscriptions.is_empty());
    }

    // ── Test 5 ─────────────────────────────────────────────

    #[tokio::test]
    async fn on_unwatch_without_subscription_is_noop() {
        let mut d = make_driver();
        let refs = vec![DriverPointRef {
            point_id: 1234,
            address: String::new(),
        }];
        let result = d.on_unwatch(&refs).await;
        assert!(result.is_ok());
        assert!(d.cov_subscriptions.is_empty());
    }

    // ── Test 6 ─────────────────────────────────────────────

    #[tokio::test]
    async fn subscribe_cov_mock_socket_success() {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("mock bind");
        let mock_addr: std::net::SocketAddr = sock.local_addr().unwrap();

        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let invoke_id = if n >= 9 { buf[8] } else { 0 };
                if n >= 10 {
                    assert_eq!(buf[9], 0x05, "service choice should be SubscribeCOV");
                }
                let reply = frame::encode_simple_ack(invoke_id, 0x05);
                let _ = sock.send_to(&reply, from).await;
            }
        });

        let mut driver = make_ready_driver(mock_addr).await;

        let result = driver
            .subscribe_cov(mock_addr, 7, 0, 1, false, Some(300))
            .await;

        assert!(result.is_ok(), "expected Ok(()), got {result:?}");
    }

    // ── Test 7 ─────────────────────────────────────────────

    #[tokio::test]
    async fn subscribe_cov_mock_error_pdu_returns_remote_status() {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("mock bind");
        let mock_addr: std::net::SocketAddr = sock.local_addr().unwrap();

        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let invoke_id = if n >= 9 { buf[8] } else { 0 };
                let apdu: Vec<u8> = vec![
                    0x50, invoke_id, 0x05, // service choice = SubscribeCOV
                    0x91, 0x02, // error class = 2
                    0x91, 0x05, // error code = 5
                ];
                let total_len = (4u16 + 2 + apdu.len() as u16).to_be_bytes();
                let mut pkt = vec![0x81, 0x0A, total_len[0], total_len[1], 0x01, 0x00];
                pkt.extend_from_slice(&apdu);
                let _ = sock.send_to(&pkt, from).await;
            }
        });

        let mut driver = make_ready_driver(mock_addr).await;

        let result = driver
            .subscribe_cov(mock_addr, 7, 0, 1, false, Some(300))
            .await;

        match result {
            Err(DriverError::RemoteStatus(msg)) => {
                assert!(
                    msg.contains("class=2") && msg.contains("code=5"),
                    "error message should mention class=2 code=5, got: {msg}"
                );
            }
            other => panic!("expected RemoteStatus, got {other:?}"),
        }
    }

    // ── Test 8 ─────────────────────────────────────────────

    #[tokio::test]
    async fn on_watch_happy_path_tracks_subscription() {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("mock bind");
        let mock_addr: std::net::SocketAddr = sock.local_addr().unwrap();

        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let invoke_id = if n >= 9 { buf[8] } else { 0 };
                let reply = frame::encode_simple_ack(invoke_id, 0x05);
                let _ = sock.send_to(&reply, from).await;
            }
        });

        let mut driver = make_ready_driver(mock_addr).await;
        driver.add_object(
            8001,
            object::BacnetObject {
                device_id: 42,
                object_type: 0,
                instance: 1,
                scale: 1.0,
                offset: 0.0,
                unit: None,
            },
        );

        let refs = vec![DriverPointRef {
            point_id: 8001,
            address: String::new(),
        }];
        let result = driver.on_watch(&refs).await;
        assert!(result.is_ok());
        assert_eq!(driver.cov_subscriptions.len(), 1);
        let sub = driver.cov_subscriptions.values().next().unwrap();
        assert_eq!(sub.point_id, 8001);
        assert_eq!(sub.object_type, 0);
        assert_eq!(sub.object_instance, 1);
        assert_ne!(sub.process_id, 0);
    }

    // ── Test 9 ─────────────────────────────────────────────

    #[tokio::test]
    async fn on_watch_idempotent_for_same_point() {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("mock bind");
        let mock_addr: std::net::SocketAddr = sock.local_addr().unwrap();

        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let invoke_id = if n >= 9 { buf[8] } else { 0 };
                let reply = frame::encode_simple_ack(invoke_id, 0x05);
                let _ = sock.send_to(&reply, from).await;
            }
        });

        let mut driver = make_ready_driver(mock_addr).await;
        driver.add_object(
            8001,
            object::BacnetObject {
                device_id: 42,
                object_type: 0,
                instance: 1,
                scale: 1.0,
                offset: 0.0,
                unit: None,
            },
        );

        let refs = vec![DriverPointRef {
            point_id: 8001,
            address: String::new(),
        }];
        assert!(driver.on_watch(&refs).await.is_ok());
        assert_eq!(driver.cov_subscriptions.len(), 1);

        // Second call — idempotent.
        assert!(driver.on_watch(&refs).await.is_ok());
        assert_eq!(driver.cov_subscriptions.len(), 1);
    }

    // ── Test 10 ────────────────────────────────────────────

    #[tokio::test]
    async fn on_unwatch_removes_local_state_even_if_cancel_fails() {
        let mut driver = make_driver();
        driver.socket = Some(
            tokio::net::UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("bind driver socket"),
        );
        driver.status = DriverStatus::Ok;

        // Inject a tracked subscription whose device is NOT in the registry.
        driver.cov_subscriptions.insert(
            42,
            CovSubscription {
                point_id: 8001,
                process_id: 42,
                object_type: 0,
                object_instance: 1,
                device_id: 999, // not in registry
                lifetime: Some(300),
                subscribed_at: std::time::Instant::now(),
            },
        );

        let refs = vec![DriverPointRef {
            point_id: 8001,
            address: String::new(),
        }];
        let result = driver.on_unwatch(&refs).await;
        assert!(result.is_ok());
        assert!(driver.cov_subscriptions.is_empty());
    }
}

// ── Phase B8.1: COV cache tests ──────────────────────────

#[cfg(test)]
mod cov_cache_tests {
    use super::*;

    #[test]
    fn cov_cache_insert_and_get() {
        let mut cache = CovCache::new();
        let notification = frame::CovNotification {
            subscriber_process_id: 1,
            initiating_device_instance: 100,
            monitored_object_type: 0,
            monitored_object_instance: 1,
            time_remaining: Some(300),
            values: vec![frame::CovPropertyValue {
                property_id: 85,
                array_index: None,
                value: value::BacnetValue::Real(42.0),
            }],
        };
        cache.update(&notification);

        let entry = cache
            .get(0, 1, std::time::Duration::from_secs(600))
            .expect("cache hit");
        assert_eq!(entry.value, value::BacnetValue::Real(42.0));
        assert_eq!(entry.process_id, 1);
    }

    #[test]
    fn cov_cache_expired_returns_none() {
        let mut cache = CovCache::new();
        // Insert an entry with a manually backdated timestamp
        cache.entries.insert(
            (0, 1),
            CovCacheEntry {
                value: value::BacnetValue::Real(42.0),
                updated_at: std::time::Instant::now() - std::time::Duration::from_secs(601),
                process_id: 1,
            },
        );

        let entry = cache.get(0, 1, std::time::Duration::from_secs(600));
        assert!(entry.is_none(), "expired entry should return None");
    }

    #[test]
    fn cov_cache_remove_works() {
        let mut cache = CovCache::new();
        let notification = frame::CovNotification {
            subscriber_process_id: 1,
            initiating_device_instance: 100,
            monitored_object_type: 0,
            monitored_object_instance: 1,
            time_remaining: Some(300),
            values: vec![frame::CovPropertyValue {
                property_id: 85,
                array_index: None,
                value: value::BacnetValue::Real(42.0),
            }],
        };
        cache.update(&notification);
        assert!(cache
            .get(0, 1, std::time::Duration::from_secs(600))
            .is_some());

        cache.remove(0, 1);
        assert!(cache
            .get(0, 1, std::time::Duration::from_secs(600))
            .is_none());
    }

    #[test]
    fn cov_cache_update_from_notification() {
        let mut cache = CovCache::new();
        let notification = frame::CovNotification {
            subscriber_process_id: 7,
            initiating_device_instance: 200,
            monitored_object_type: 2, // AV
            monitored_object_instance: 10,
            time_remaining: Some(300),
            values: vec![
                frame::CovPropertyValue {
                    property_id: 111, // StatusFlags
                    array_index: None,
                    value: value::BacnetValue::Unsigned(0),
                },
                frame::CovPropertyValue {
                    property_id: 85, // PresentValue
                    array_index: None,
                    value: value::BacnetValue::Real(42.0),
                },
            ],
        };
        cache.update(&notification);

        let entry = cache
            .get(2, 10, std::time::Duration::from_secs(600))
            .expect("cache hit for AV:10");
        assert_eq!(entry.value, value::BacnetValue::Real(42.0));
        assert_eq!(entry.process_id, 7);
    }

    #[test]
    fn cov_cache_ignores_non_present_value() {
        let mut cache = CovCache::new();
        let notification = frame::CovNotification {
            subscriber_process_id: 1,
            initiating_device_instance: 100,
            monitored_object_type: 0,
            monitored_object_instance: 5,
            time_remaining: Some(300),
            values: vec![frame::CovPropertyValue {
                property_id: 77, // ObjectName
                array_index: None,
                value: value::BacnetValue::CharacterString("test".into()),
            }],
        };
        cache.update(&notification);

        assert!(
            cache
                .get(0, 5, std::time::Duration::from_secs(600))
                .is_none(),
            "non-PresentValue property should not populate cache"
        );
    }

    #[tokio::test]
    async fn handle_cov_notification_updates_cache() {
        let mut driver = BacnetDriver::new("test-bac", "255.255.255.255", 0);
        let notification = frame::CovNotification {
            subscriber_process_id: 3,
            initiating_device_instance: 100,
            monitored_object_type: 0,
            monitored_object_instance: 1,
            time_remaining: Some(300),
            values: vec![frame::CovPropertyValue {
                property_id: 85,
                array_index: None,
                value: value::BacnetValue::Real(72.5),
            }],
        };
        driver.handle_cov_notification(&notification);

        let entry = driver
            .cov_cache
            .get(0, 1, std::time::Duration::from_secs(600))
            .expect("cache hit after handle_cov_notification");
        assert_eq!(entry.value, value::BacnetValue::Real(72.5));
        assert_eq!(entry.process_id, 3);
    }
}

// ── Phase B10: BBMD tests ─────────────────────────────────

#[cfg(test)]
mod bbmd_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    /// Encode a minimal I-Am response for test mocks.
    fn encode_i_am(
        device_instance: u32,
        max_apdu: u16,
        segmentation: u8,
        vendor_id: u16,
    ) -> Vec<u8> {
        let obj_id_val: u32 = (8u32 << 22) | (device_instance & 0x3F_FFFF);
        let obj_id = obj_id_val.to_be_bytes();
        let mut apdu: Vec<u8> = vec![
            0x10, 0x00, // Unconfirmed-Request, I-Am
            0xC4, obj_id[0], obj_id[1], obj_id[2], obj_id[3], // Object ID
        ];
        if max_apdu <= 0xFF {
            apdu.extend_from_slice(&[0x21, max_apdu as u8]);
        } else {
            apdu.extend_from_slice(&[0x22, (max_apdu >> 8) as u8, (max_apdu & 0xFF) as u8]);
        }
        apdu.extend_from_slice(&[0x91, segmentation]); // Segmentation
        if vendor_id <= 0xFF {
            apdu.extend_from_slice(&[0x21, vendor_id as u8]);
        } else {
            apdu.extend_from_slice(&[0x22, (vendor_id >> 8) as u8, (vendor_id & 0xFF) as u8]);
        }
        let total_len = 4u16 + 2 + apdu.len() as u16;
        let mut pkt = vec![
            0x81,
            0x0A, // BVLL unicast
            (total_len >> 8) as u8,
            (total_len & 0xFF) as u8,
            0x01,
            0x00, // NPDU
        ];
        pkt.extend_from_slice(&apdu);
        pkt
    }

    // ── Test 1 ─────────────────────────────────────────────

    #[test]
    fn bbmd_addr_none_by_default() {
        let d = BacnetDriver::new("bac-1", "255.255.255.255", DEFAULT_BACNET_PORT);
        assert!(d.bbmd_addr.is_none());
        assert!(!d.is_foreign_device);
    }

    // ── Test 2 ─────────────────────────────────────────────

    #[test]
    fn from_config_with_bbmd() {
        let config = BacnetConfig {
            id: "bac-bbmd".into(),
            port: Some(47808),
            broadcast: Some("255.255.255.255".into()),
            objects: vec![],
            bbmd: Some("192.168.2.1:47808".into()),
        };
        let d = BacnetDriver::from_config(config);
        let addr = d.bbmd_addr.expect("bbmd_addr should be Some");
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::new(192, 168, 2, 1)));
        assert_eq!(addr.port(), 47808);
        assert!(!d.is_foreign_device, "not registered until open()");
    }

    // ── Test 3 ─────────────────────────────────────────────

    #[test]
    fn from_config_with_invalid_bbmd_logs_warning() {
        let config = BacnetConfig {
            id: "bac-bad-bbmd".into(),
            port: None,
            broadcast: None,
            objects: vec![],
            bbmd: Some("not-an-address".into()),
        };
        let d = BacnetDriver::from_config(config);
        assert!(
            d.bbmd_addr.is_none(),
            "invalid bbmd should be silently ignored"
        );
    }

    // ── Test 4 ─────────────────────────────────────────────

    #[tokio::test]
    async fn register_foreign_device_mock_success() {
        // Spawn mock BBMD that replies with BVLL-Result success.
        let mock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let mock_addr = mock.local_addr().unwrap();

        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            if let Ok((n, from)) = mock.recv_from(&mut buf).await {
                // Verify it's a Register-Foreign-Device (0x81 0x05)
                assert!(n >= 6);
                assert_eq!(buf[0], 0x81);
                assert_eq!(buf[1], frame::BVLL_REGISTER_FOREIGN_DEVICE);
                // Reply with BVLL-Result success (result code 0x0000)
                let reply = [0x81u8, 0x00, 0x00, 0x06, 0x00, 0x00];
                let _ = mock.send_to(&reply, from).await;
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let driver = BacnetDriver::new("bac-test", "127.0.0.1", 0);
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind driver");

        let result = driver
            .register_foreign_device(&socket, mock_addr, 300)
            .await;
        assert!(
            result.is_ok(),
            "registration should succeed, got {result:?}"
        );
    }

    // ── Test 5 ─────────────────────────────────────────────

    #[tokio::test]
    async fn register_foreign_device_mock_nak() {
        // Spawn mock BBMD that replies with NAK (result code 0x0030).
        let mock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let mock_addr = mock.local_addr().unwrap();

        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            if let Ok((_n, from)) = mock.recv_from(&mut buf).await {
                // Reply with BVLL-Result NAK (result code 0x0030)
                let reply = [0x81u8, 0x00, 0x00, 0x06, 0x00, 0x30];
                let _ = mock.send_to(&reply, from).await;
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let driver = BacnetDriver::new("bac-test", "127.0.0.1", 0);
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind driver");

        let result = driver
            .register_foreign_device(&socket, mock_addr, 300)
            .await;
        assert!(result.is_err(), "NAK should result in error");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("NAK"),
            "error should mention NAK, got: {err_msg}"
        );
    }

    // ── Test 6 ─────────────────────────────────────────────

    #[tokio::test]
    async fn open_with_bbmd_registers_and_discovers() {
        // 1. Bind mock that handles Register-Foreign-Device and Who-Is -> I-Am.
        let mock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let mock_addr = mock.local_addr().unwrap();

        tokio::spawn(async move {
            let mut buf = [0u8; 1500];

            // Exchange 1: Register-Foreign-Device -> BVLL-Result success
            if let Ok((n, from)) = mock.recv_from(&mut buf).await {
                if n >= 2 && buf[0] == 0x81 && buf[1] == frame::BVLL_REGISTER_FOREIGN_DEVICE {
                    let reply = [0x81u8, 0x00, 0x00, 0x06, 0x00, 0x00];
                    let _ = mock.send_to(&reply, from).await;
                }
            }

            // Exchange 2: Who-Is (local broadcast) -> I-Am
            if let Ok((_n, from)) = mock.recv_from(&mut buf).await {
                let i_am = encode_i_am(12345, 1476, 3, 999);
                let _ = mock.send_to(&i_am, from).await;
            }

            // Exchange 3: Distribute-Broadcast Who-Is (via BBMD) -- just consume it
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                mock.recv_from(&mut buf),
            )
            .await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // 2. Configure driver with BBMD pointing to our mock.
        let mut driver = BacnetDriver::new("bac-bbmd-e2e", "127.0.0.1", 0)
            .with_bbmd_addr(mock_addr)
            .with_broadcast_port(mock_addr.port())
            .with_discovery_timeout(std::time::Duration::from_millis(200));

        // 3. Open -- should register as foreign device + discover the mock device.
        let meta = driver.open().await.expect("open should succeed");
        assert!(
            driver.is_foreign_device,
            "should be registered as foreign device"
        );
        assert!(
            driver.device_registry().len() >= 1,
            "should have discovered at least one device"
        );
        assert!(
            meta.model.as_deref().unwrap_or("").contains("BACnet/IP"),
            "meta should mention BACnet/IP"
        );
    }
}

// ── Phase B8.2: COV renewal tests ───────────────────────────
#[cfg(test)]
mod renewal_tests {
    use super::*;

    fn make_driver() -> BacnetDriver {
        BacnetDriver::new("b8-2-test", "127.0.0.1", 0)
    }

    // ── Test 1 ─────────────────────────────────────────────

    #[test]
    fn renewal_interval_default_is_240s() {
        let d = BacnetDriver::new("bac", "127.0.0.1", 47808);
        assert_eq!(d.renewal_interval, std::time::Duration::from_secs(240));
    }

    // ── Test 2 ─────────────────────────────────────────────

    #[test]
    fn with_renewal_interval_builder_works() {
        let d = BacnetDriver::new("bac", "127.0.0.1", 47808)
            .with_renewal_interval(std::time::Duration::from_secs(60));
        assert_eq!(d.renewal_interval, std::time::Duration::from_secs(60));
    }

    // ── Test 3 ─────────────────────────────────────────────

    #[tokio::test]
    async fn renew_due_subscriptions_empty_is_noop() {
        let mut d = make_driver();
        d.renew_due_subscriptions().await;
        assert!(d.cov_subscriptions.is_empty());
    }

    // ── Test 4 ─────────────────────────────────────────────

    #[tokio::test]
    async fn renew_due_subscriptions_skips_fresh_entries() {
        let mut d = make_driver().with_renewal_interval(std::time::Duration::from_secs(60));
        let original = std::time::Instant::now();
        d.cov_subscriptions.insert(
            1,
            CovSubscription {
                point_id: 8001,
                process_id: 1,
                object_type: 0,
                object_instance: 1,
                device_id: 42,
                lifetime: Some(300),
                subscribed_at: original,
            },
        );

        d.renew_due_subscriptions().await;

        // Entry still present, timestamp unchanged (fresh: elapsed < 60s).
        let sub = d
            .cov_subscriptions
            .get(&1)
            .expect("subscription should remain");
        assert_eq!(sub.subscribed_at, original);
    }

    // ── Test 5 ─────────────────────────────────────────────

    #[tokio::test]
    async fn renew_due_subscriptions_device_not_in_registry_skipped() {
        let mut d = make_driver().with_renewal_interval(std::time::Duration::from_millis(1));
        // Stale subscription — use now() as baseline and sleep past 1ms interval.
        let stale = std::time::Instant::now();
        d.cov_subscriptions.insert(
            1,
            CovSubscription {
                point_id: 8001,
                process_id: 1,
                object_type: 0,
                object_instance: 1,
                device_id: 99, // NOT in registry
                lifetime: Some(300),
                subscribed_at: stale,
            },
        );

        // Give the 1ms interval a chance to lapse.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Should not panic, subscription should still be present.
        d.renew_due_subscriptions().await;
        assert!(d.cov_subscriptions.contains_key(&1));
        // Timestamp unchanged because renewal was skipped.
        assert_eq!(d.cov_subscriptions.get(&1).unwrap().subscribed_at, stale);
    }

    // ── Test 6 ─────────────────────────────────────────────

    #[tokio::test]
    async fn renew_due_subscriptions_renews_against_mock_socket() {
        // Spawn a mock device that ACKs SubscribeCOV.
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("mock bind");
        let mock_addr: std::net::SocketAddr = sock.local_addr().unwrap();

        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            if let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let invoke_id = if n >= 9 { buf[8] } else { 0 };
                let reply = frame::encode_simple_ack(invoke_id, 0x05);
                let _ = sock.send_to(&reply, from).await;
            }
        });

        let mut d = make_driver().with_renewal_interval(std::time::Duration::from_millis(1));
        d.socket = Some(
            tokio::net::UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("bind driver socket"),
        );
        d.status = DriverStatus::Ok;
        d.device_registry.insert(DeviceInfo {
            instance: 42,
            addr: mock_addr,
            max_apdu: 1476,
            vendor_id: 999,
            segmentation: 3,
        });

        // Stale subscription — capture now() as baseline, then sleep past 1ms interval.
        let stale = std::time::Instant::now();
        d.cov_subscriptions.insert(
            7,
            CovSubscription {
                point_id: 8001,
                process_id: 7,
                object_type: 0,
                object_instance: 1,
                device_id: 42,
                lifetime: Some(300),
                subscribed_at: stale,
            },
        );

        // Let the 1ms lapse.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        d.renew_due_subscriptions().await;

        let sub = d
            .cov_subscriptions
            .get(&7)
            .expect("subscription should still exist after renewal");
        assert!(
            sub.subscribed_at > stale,
            "subscribed_at should have advanced after successful renewal"
        );
    }
}
