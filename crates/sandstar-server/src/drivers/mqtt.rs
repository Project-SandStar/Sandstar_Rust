//! MQTT pub/sub driver (Phase M2 — value cache + sync_cur).
//!
//! Implements the `AsyncDriver` lifecycle for an MQTT broker connection:
//! connecting via `rumqttc`, spawning the event-loop task, subscribing to
//! configured topics, and cleanly disconnecting on close.
//!
//! **Scope:**
//! - M1 (complete): connect / disconnect / ping / learn.
//! - M2 (complete): value cache populated by the event-loop task,
//!   `sync_cur()` returns fresh cached values or `CommFault` on stale/absent.
//! - M3 (this file): `write()` publishes to the configured `publish_topic`.
//! - Server wiring + E2E is M4.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use super::async_driver::AsyncDriver;
use super::{
    DriverError, DriverMeta, DriverPointRef, DriverStatus, LearnGrid, LearnPoint, PollMode,
    SyncContext, WriteContext,
};

// ── Config ─────────────────────────────────────────────────

/// Top-level MQTT driver configuration (deserialized from JSON).
///
/// Mirrors the schema documented in `docs/IMPLEMENTATION_PLAN_MQTT.md`.
///
/// `Debug` is written by hand so that the password never reaches a log.
#[derive(Clone, Serialize, Deserialize)]
pub struct MqttConfig {
    /// Unique driver instance identifier.
    pub id: String,
    /// MQTT broker hostname.
    pub host: String,
    /// Broker port. Defaults to 1883 (plain) when unset.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Client identifier registered with the broker.
    pub client_id: String,
    /// Optional username for plain auth.
    #[serde(default)]
    pub username: Option<String>,
    /// Optional password for plain auth.
    #[serde(default, deserialize_with = "crate::alerts::de_secret_opt")]
    pub password: Option<String>,
    /// Whether to use TLS. Defaults to false. (Not wired in M1.)
    #[serde(default)]
    pub tls: bool,
    /// Keep-alive interval in seconds. Defaults to 60.
    #[serde(default = "default_keep_alive")]
    pub keep_alive_secs: u16,
    /// List of MQTT objects to bind to Sandstar points.
    #[serde(default)]
    pub objects: Vec<MqttObjectConfig>,
}

impl std::fmt::Debug for MqttConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MqttConfig")
            .field("id", &self.id)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("client_id", &self.client_id)
            .field("username", &self.username)
            .field(
                "password",
                &crate::alerts::redacted(self.password.as_deref().is_some_and(|p| !p.is_empty())),
            )
            .field("tls", &self.tls)
            .field("keep_alive_secs", &self.keep_alive_secs)
            .field("objects", &self.objects)
            .finish()
    }
}

fn default_port() -> u16 {
    1883
}

fn default_keep_alive() -> u16 {
    60
}

fn default_qos() -> u8 {
    1
}

/// Per-point MQTT configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MqttObjectConfig {
    /// Sandstar channel ID this object maps to.
    pub point_id: u32,
    /// Topic to subscribe to for reads (receiving values from broker).
    #[serde(default)]
    pub subscribe_topic: Option<String>,
    /// Topic to publish to for writes (sending values to broker).
    #[serde(default)]
    pub publish_topic: Option<String>,
    /// RFC 6901 JSON Pointer into message payload.
    /// `None` means payload is a plain number.
    #[serde(default)]
    pub value_path: Option<String>,
    /// QoS level (0 or 1). Defaults to 1. QoS 2 is out of scope and
    /// falls back to 1.
    #[serde(default = "default_qos")]
    pub qos: u8,
}

/// Map a `u8` QoS code to a rumqttc [`QoS`] enum.
///
/// - `0` -> AtMostOnce
/// - `1` -> AtLeastOnce
/// - `2+` -> AtLeastOnce (QoS 2 is disallowed per the plan)
fn qos_from_u8(qos: u8) -> QoS {
    match qos {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        _ => QoS::AtLeastOnce,
    }
}

/// Maximum age for a cached value before `sync_cur()` treats it as stale.
const MQTT_CACHE_MAX_AGE: Duration = Duration::from_secs(600);

/// First wait before the event loop tries the broker again after an
/// error. Doubles on each error in a row (R-25, S-51).
pub const MQTT_RECONNECT_MIN: Duration = Duration::from_secs(1);

/// Longest wait between two connection attempts.
pub const MQTT_RECONNECT_MAX: Duration = Duration::from_secs(60);

/// How long `open()` waits for the first CONNACK before it returns with
/// the driver still down (the event loop keeps trying).
pub const MQTT_OPEN_WAIT: Duration = Duration::from_secs(2);

/// The error of reads and writes while the broker is not connected.
const NOT_CONNECTED: &str = "mqtt broker not connected";

/// Wait before the next connection attempt: 1 s, 2 s, 4 s, ... 60 s.
#[derive(Debug)]
struct ReconnectDelay {
    next: Duration,
}

impl ReconnectDelay {
    fn new() -> Self {
        Self {
            next: MQTT_RECONNECT_MIN,
        }
    }

    /// The wait for this attempt; the following one is twice as long.
    fn next(&mut self) -> Duration {
        let d = self.next;
        self.next = self.next.saturating_mul(2).min(MQTT_RECONNECT_MAX);
        d
    }

    /// Connected: start over.
    fn reset(&mut self) {
        self.next = MQTT_RECONNECT_MIN;
    }
}

// ── Value cache ────────────────────────────────────────────

/// The latest payload of a topic with the time it arrived.
///
/// `value` is `Err` when that payload was not a usable reading (not a
/// number, or NaN/±inf). The point then reports a fault instead of the
/// last good value (R-49).
#[derive(Debug, Clone)]
struct MqttCacheEntry {
    value: Result<f64, String>,
    updated_at: Instant,
}

/// Cache of latest MQTT-reported values, keyed by subscribe topic.
///
/// Populated as a side-effect by the event-loop task when `Publish`
/// packets arrive. `sync_cur()` reads from here without touching the
/// network.
#[derive(Debug, Default)]
struct MqttValueCache {
    entries: HashMap<String, MqttCacheEntry>,
}

impl MqttValueCache {
    fn new() -> Self {
        Self::default()
    }

    fn update(&mut self, topic: impl Into<String>, value: f64) {
        self.insert(topic.into(), Ok(value));
    }

    /// Record that the latest payload of a topic was rejected.
    fn reject(&mut self, topic: impl Into<String>, reason: String) {
        self.insert(topic.into(), Err(reason));
    }

    fn insert(&mut self, topic: String, value: Result<f64, String>) {
        self.entries.insert(
            topic,
            MqttCacheEntry {
                value,
                updated_at: Instant::now(),
            },
        );
    }

    /// Look up a cached entry. Returns `None` if absent or older than `max_age`.
    fn get(&self, topic: &str, max_age: Duration) -> Option<MqttCacheEntry> {
        self.entries
            .get(topic)
            .filter(|e| e.updated_at.elapsed() < max_age)
            .cloned()
    }

    /// Forget every value: after a reconnect only what the broker sends
    /// again counts as a reading.
    fn clear(&mut self) {
        self.entries.clear();
    }

    /// Drop the cached entry for a topic, if any.
    /// Exposed for unit tests and for future use by `close()` / unsubscribe.
    #[allow(dead_code)]
    fn remove(&mut self, topic: &str) {
        self.entries.remove(topic);
    }

    /// Number of cached entries. Exposed for tests.
    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Parse a raw MQTT payload into a floating-point value.
///
/// - If `value_path` is `None`, the payload is parsed directly as an `f64`
///   (leading/trailing whitespace is trimmed).
/// - If `value_path` is `Some(pointer)`, the payload is parsed as JSON and
///   the pointer is resolved via RFC 6901. The resulting `serde_json::Value`
///   is coerced to `f64` (numbers, booleans, or numeric strings).
///
/// Extracted as a free function so it can be unit-tested independently of the
/// event-loop task.
fn extract_value(payload_str: &str, value_path: Option<&str>) -> Result<f64, String> {
    // `str::parse::<f64>` accepts "nan", "inf" and "infinity"; a payload
    // like that is not a reading (R-49).
    let value = extract_raw_value(payload_str, value_path)?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("value {value} is not finite"))
    }
}

fn extract_raw_value(payload_str: &str, value_path: Option<&str>) -> Result<f64, String> {
    match value_path {
        None => payload_str.trim().parse::<f64>().map_err(|e| e.to_string()),
        Some(pointer) => {
            let v: serde_json::Value =
                serde_json::from_str(payload_str).map_err(|e| format!("json parse: {e}"))?;
            let target = v
                .pointer(pointer)
                .ok_or_else(|| format!("path {pointer} not found"))?;
            target
                .as_f64()
                .or_else(|| target.as_i64().map(|i| i as f64))
                .or_else(|| target.as_u64().map(|u| u as f64))
                .or_else(|| target.as_bool().map(|b| if b { 1.0 } else { 0.0 }))
                .or_else(|| target.as_str().and_then(|s| s.parse::<f64>().ok()))
                .ok_or_else(|| format!("value at {pointer} is not numeric"))
        }
    }
}

/// Build the payload bytes for a write.
///
/// If `use_json_envelope` is true, emit a compact JSON object `{"value": N}`;
/// otherwise emit the number's `Display` form.
///
/// Phase M3 note: we always use the literal key `"value"` in the JSON
/// envelope — we do NOT mirror the read-side `value_path` into a nested
/// write shape. If downstream consumers need a different write shape
/// (e.g. `{"data":{"value":N}}` to match a `/data/value` read path),
/// that's an M4+ concern. The rule here is: if the read side uses any
/// JSON pointer path, the write side also uses JSON (flat `{"value":N}`);
/// otherwise the payload is a bare number.
fn build_publish_payload(value: f64, use_json_envelope: bool) -> Vec<u8> {
    if use_json_envelope {
        serde_json::json!({ "value": value })
            .to_string()
            .into_bytes()
    } else {
        value.to_string().into_bytes()
    }
}

// ── Driver ────────────────────────────────────────────────

/// MQTT pub/sub driver.
///
/// Wraps a `rumqttc::AsyncClient` plus a tokio task that drives the event
/// loop. The event-loop task decodes incoming publishes and pushes their
/// values into a shared `MqttValueCache`, which `sync_cur()` reads from.
pub struct MqttDriver {
    id: String,
    status: DriverStatus,
    config: MqttConfig,
    client: Option<AsyncClient>,
    event_loop_task: Option<JoinHandle<()>>,
    /// Point-id-keyed lookup of MQTT object config (used by sync_cur/write).
    objects: HashMap<u32, MqttObjectConfig>,
    /// Shared value cache populated by the event-loop task.
    cache: Arc<Mutex<MqttValueCache>>,
    /// Whether the broker connection is up, as reported by the event-loop
    /// task. `None` until `open()` and after `close()`.
    connected: Option<tokio::sync::watch::Receiver<bool>>,
}

impl MqttDriver {
    /// Simple constructor primarily for tests: builds a driver with no
    /// objects configured.
    pub fn new(id: impl Into<String>, host: impl Into<String>, port: u16) -> Self {
        let id: String = id.into();
        let host: String = host.into();
        let config = MqttConfig {
            id: id.clone(),
            host,
            port,
            client_id: id.clone(),
            username: None,
            password: None,
            tls: false,
            keep_alive_secs: default_keep_alive(),
            objects: Vec::new(),
        };
        Self::from_config(config)
    }

    /// Main factory: build a driver from a parsed [`MqttConfig`].
    pub fn from_config(config: MqttConfig) -> Self {
        let id = config.id.clone();
        let objects: HashMap<u32, MqttObjectConfig> = config
            .objects
            .iter()
            .map(|o| (o.point_id, o.clone()))
            .collect();
        Self {
            id,
            status: DriverStatus::Pending,
            config,
            client: None,
            event_loop_task: None,
            objects,
            cache: Arc::new(Mutex::new(MqttValueCache::new())),
            connected: None,
        }
    }

    /// Whether the broker connection is up right now.
    fn is_connected(&self) -> bool {
        self.connected.as_ref().is_some_and(|rx| *rx.borrow())
    }

    /// Bring `status` in line with the connection: `Ok` while connected,
    /// `Down` while the event loop is reconnecting. A driver that was
    /// never opened, was closed or faulted keeps its status.
    fn refresh_status(&mut self) {
        if self.connected.is_none() {
            return;
        }
        self.status = if self.is_connected() {
            DriverStatus::Ok
        } else {
            DriverStatus::Down
        };
    }

    /// Immutable view of the parsed config.
    pub fn config(&self) -> &MqttConfig {
        &self.config
    }

    /// Immutable view of the point-id -> object map.
    pub fn objects(&self) -> &HashMap<u32, MqttObjectConfig> {
        &self.objects
    }
}

// ── DriverLoader impl (Phase 12.0A) ────────────────────────

/// [`DriverLoader`] specialization for MQTT.
///
/// Used by `rest/mod.rs` to wire `SANDSTAR_MQTT_CONFIGS` into the generic
/// loader. See [`crate::drivers::loader::load_drivers`] for the shared flow.
pub struct MqttLoader;

impl crate::drivers::loader::DriverLoader for MqttLoader {
    const ENV_VAR: &'static str = "SANDSTAR_MQTT_CONFIGS";
    const DRIVER_TYPE: &'static str = "mqtt";
    const LABEL: &'static str = "MQTT";

    type Config = MqttConfig;

    fn config_id(config: &Self::Config) -> String {
        config.id.clone()
    }

    fn config_point_ids(config: &Self::Config) -> Vec<u32> {
        config.objects.iter().map(|o| o.point_id).collect()
    }

    fn build_driver(config: Self::Config) -> Box<dyn super::async_driver::AsyncDriver> {
        Box::new(MqttDriver::from_config(config))
    }
}

#[async_trait]
impl AsyncDriver for MqttDriver {
    fn driver_type(&self) -> &'static str {
        "mqtt"
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn status(&self) -> &DriverStatus {
        // While open, the status is the connection state as the event loop
        // reports it, not what the last poll saw (S-51).
        match &self.connected {
            Some(rx) if *rx.borrow() => &DriverStatus::Ok,
            Some(_) => &DriverStatus::Down,
            None => &self.status,
        }
    }

    fn poll_mode(&self) -> PollMode {
        // MQTT is event-driven, not polled. The driver pushes updates
        // into the cache when messages arrive from the broker.
        PollMode::Manual
    }

    async fn open(&mut self) -> Result<DriverMeta, DriverError> {
        // Tear down any previous client before reconnecting.
        if let Some(client) = self.client.take() {
            let _ = client.disconnect().await;
        }
        if let Some(task) = self.event_loop_task.take() {
            task.abort();
        }
        self.connected = None;

        // A config that asks for TLS must not connect in plain text.
        if self.config.tls {
            let msg = "mqtt TLS is not supported in this build";
            self.status = DriverStatus::Fault(msg.into());
            return Err(DriverError::ConfigFault(msg.into()));
        }

        // Build MqttOptions from config.
        let mut options = MqttOptions::new(
            self.config.client_id.clone(),
            self.config.host.clone(),
            self.config.port,
        );
        options.set_keep_alive(Duration::from_secs(self.config.keep_alive_secs as u64));
        if let (Some(u), Some(p)) = (self.config.username.as_ref(), self.config.password.as_ref()) {
            options.set_credentials(u.clone(), p.clone());
        }

        let (client, mut eventloop) = AsyncClient::new(options, 64);
        let driver_id = self.id.clone();

        // Pre-compute the topic -> object map so the event-loop task can
        // look up `value_path` / QoS without locking the driver itself.
        let topic_map: Arc<HashMap<String, MqttObjectConfig>> = Arc::new(
            self.objects
                .values()
                .filter_map(|obj| {
                    obj.subscribe_topic
                        .as_ref()
                        .map(|t| (t.clone(), obj.clone()))
                })
                .collect(),
        );
        let cache = Arc::clone(&self.cache);
        let task_topic_map = Arc::clone(&topic_map);
        let (connected_tx, mut connected_rx) = tokio::sync::watch::channel(false);
        let task_client = client.clone();
        let subscriptions: Vec<(String, QoS)> = self
            .config
            .objects
            .iter()
            .filter_map(|o| {
                o.subscribe_topic
                    .as_ref()
                    .map(|t| (t.clone(), qos_from_u8(o.qos)))
            })
            .collect();

        // Spawn the event loop task. Incoming publishes are parsed and
        // pushed into the shared value cache. After an error the loop
        // waits and polls again: rumqttc reconnects on the next poll.
        let task = tokio::spawn(async move {
            let mut delay = ReconnectDelay::new();
            // Whether the current outage has been logged at warn.
            let mut outage_logged = false;
            // Subscribes of the latest connection; replaced on reconnect.
            let mut subscriber: Option<JoinHandle<()>> = None;
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Incoming::ConnAck(ack))) => {
                        tracing::info!(
                            driver = %driver_id,
                            ?ack,
                            "mqtt connected"
                        );
                        delay.reset();
                        outage_logged = false;
                        // Values from before the outage are not readings.
                        cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
                        // The broker forgot the subscriptions (clean
                        // session). Subscribe from a separate task: this
                        // loop must keep polling to drain the request queue.
                        if let Some(old) = subscriber.take() {
                            old.abort();
                        }
                        let client = task_client.clone();
                        let topics = subscriptions.clone();
                        let id = driver_id.clone();
                        subscriber = Some(tokio::spawn(async move {
                            for (topic, qos) in topics {
                                if let Err(e) = client.subscribe(topic.clone(), qos).await {
                                    tracing::warn!(
                                        driver = %id,
                                        topic = %topic,
                                        error = %e,
                                        "mqtt subscribe failed"
                                    );
                                }
                            }
                        }));
                        connected_tx.send_replace(true);
                    }
                    Ok(Event::Incoming(Incoming::Publish(p))) => {
                        let payload_str = match std::str::from_utf8(&p.payload) {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::warn!(
                                    driver = %driver_id,
                                    topic = %p.topic,
                                    error = %e,
                                    "MQTT payload not valid UTF-8"
                                );
                                continue;
                            }
                        };

                        let value_path = task_topic_map
                            .get(&p.topic)
                            .and_then(|obj| obj.value_path.clone());

                        match extract_value(payload_str, value_path.as_deref()) {
                            Ok(value) => {
                                if let Ok(mut guard) = cache.lock() {
                                    guard.update(p.topic.clone(), value);
                                }
                                tracing::debug!(
                                    driver = %driver_id,
                                    topic = %p.topic,
                                    value,
                                    "mqtt cache updated"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    driver = %driver_id,
                                    topic = %p.topic,
                                    error = %e,
                                    "MQTT value parse failed"
                                );
                                // The last good value must not stand in
                                // for a payload we could not use.
                                if let Ok(mut guard) = cache.lock() {
                                    guard.reject(p.topic.clone(), e);
                                }
                            }
                        }
                    }
                    Ok(Event::Incoming(Incoming::SubAck(ack))) => {
                        let refused = ack
                            .return_codes
                            .iter()
                            .filter(|c| matches!(c, rumqttc::SubscribeReasonCode::Failure))
                            .count();
                        if refused > 0 {
                            tracing::warn!(
                                driver = %driver_id,
                                refused,
                                "mqtt broker refused a subscription; its topic will not deliver"
                            );
                        }
                    }
                    Ok(ev) => {
                        tracing::debug!(
                            driver = %driver_id,
                            event = ?ev,
                            "mqtt event"
                        );
                    }
                    Err(e) => {
                        connected_tx.send_replace(false);
                        if let Some(old) = subscriber.take() {
                            old.abort();
                        }
                        // rumqttc keeps unacknowledged and unsent requests
                        // and replays them after the reconnect. A publish
                        // must not reach equipment that late, and the
                        // subscriptions are made again on CONNACK.
                        let dropped = eventloop.pending.len();
                        eventloop.pending.clear();
                        if dropped > 0 {
                            tracing::warn!(
                                driver = %driver_id,
                                dropped,
                                "mqtt requests not delivered before the disconnect were dropped"
                            );
                        }
                        let wait = delay.next();
                        if outage_logged {
                            tracing::debug!(
                                driver = %driver_id,
                                error = %e,
                                retry_in_secs = wait.as_secs(),
                                "mqtt still not connected"
                            );
                        } else {
                            outage_logged = true;
                            tracing::warn!(
                                driver = %driver_id,
                                error = %e,
                                retry_in_secs = wait.as_secs(),
                                "mqtt not connected; retrying"
                            );
                        }
                        tokio::time::sleep(wait).await;
                    }
                }
            }
        });

        // Give the first CONNACK a moment, so that a reachable broker
        // shows as Ok right away. If it does not come, the driver is Down
        // and the event loop keeps trying.
        let _ = tokio::time::timeout(MQTT_OPEN_WAIT, connected_rx.wait_for(|up| *up)).await;

        self.client = Some(client);
        self.event_loop_task = Some(task);
        self.connected = Some(connected_rx);
        self.refresh_status();

        Ok(DriverMeta {
            model: Some(format!("MQTT {}:{}", self.config.host, self.config.port)),
            ..Default::default()
        })
    }

    async fn close(&mut self) {
        if let Some(client) = self.client.take() {
            let _ = client.disconnect().await;
        }
        if let Some(task) = self.event_loop_task.take() {
            task.abort();
        }
        self.connected = None;
        self.status = DriverStatus::Down;
    }

    async fn ping(&mut self) -> Result<DriverMeta, DriverError> {
        self.refresh_status();
        match &self.event_loop_task {
            Some(task) if !task.is_finished() => {
                if !self.is_connected() {
                    return Err(DriverError::CommFault(NOT_CONNECTED.into()));
                }
                Ok(DriverMeta {
                    model: Some(format!("MQTT {}:{}", self.config.host, self.config.port)),
                    ..Default::default()
                })
            }
            _ => Err(DriverError::CommFault("mqtt event loop dead".into())),
        }
    }

    async fn learn(&mut self, _path: Option<&str>) -> Result<LearnGrid, DriverError> {
        let mut grid = Vec::with_capacity(self.config.objects.len());
        for obj in &self.config.objects {
            let name = obj
                .subscribe_topic
                .clone()
                .or_else(|| obj.publish_topic.clone())
                .unwrap_or_default();
            grid.push(LearnPoint {
                name,
                address: obj.point_id.to_string(),
                kind: "Number".to_string(),
                unit: None,
                tags: HashMap::new(),
            });
        }
        Ok(grid)
    }

    async fn sync_cur(&mut self, points: &[DriverPointRef], ctx: &mut SyncContext) {
        self.refresh_status();
        let connected = self.is_connected();
        for point in points {
            let obj = match self.objects.get(&point.point_id) {
                Some(o) => o,
                None => {
                    ctx.update_cur_err(
                        point.point_id,
                        DriverError::ConfigFault(format!(
                            "no mqtt object for point {}",
                            point.point_id
                        )),
                    );
                    continue;
                }
            };

            let topic = match obj.subscribe_topic.as_ref() {
                Some(t) => t,
                None => {
                    ctx.update_cur_err(
                        point.point_id,
                        DriverError::ConfigFault("no subscribe_topic".into()),
                    );
                    continue;
                }
            };

            // Not connected: nothing arrives, so a cached value is not a
            // reading (S-51).
            if !connected {
                ctx.update_cur_err(point.point_id, DriverError::CommFault(NOT_CONNECTED.into()));
                continue;
            }

            let cached = {
                let guard = match self.cache.lock() {
                    Ok(g) => g,
                    Err(_) => {
                        ctx.update_cur_err(
                            point.point_id,
                            DriverError::Internal("mqtt cache mutex poisoned".into()),
                        );
                        continue;
                    }
                };
                guard.get(topic, MQTT_CACHE_MAX_AGE)
            };

            match cached.map(|e| e.value) {
                Some(Ok(value)) => ctx.update_cur_ok(point.point_id, value),
                Some(Err(reason)) => ctx.update_cur_err(
                    point.point_id,
                    DriverError::RemoteStatus(format!("last payload rejected: {reason}")),
                ),
                None => ctx.update_cur_err(
                    point.point_id,
                    DriverError::CommFault("no cached value or stale".into()),
                ),
            }
        }
    }

    async fn write(&mut self, writes: &[(u32, f64)], ctx: &mut WriteContext) {
        self.refresh_status();
        for &(point_id, value) in writes {
            // 1. Look up per-point config.
            let obj = match self.objects.get(&point_id).cloned() {
                Some(o) => o,
                None => {
                    ctx.update_write_err(
                        point_id,
                        DriverError::ConfigFault(format!(
                            "no MQTT object configured for point {point_id}"
                        )),
                    );
                    continue;
                }
            };

            // 2. Require a publish_topic on this object.
            let topic = match obj.publish_topic.as_ref() {
                Some(t) => t.clone(),
                None => {
                    ctx.update_write_err(
                        point_id,
                        DriverError::ConfigFault(format!("no publish_topic for point {point_id}")),
                    );
                    continue;
                }
            };

            // 3. Build payload. Symmetric with the read side: if the
            //    object uses a JSON value_path for reads, we emit a JSON
            //    envelope for writes (flat `{"value": N}` — see
            //    `build_publish_payload` docs for the rationale).
            let use_json_envelope = obj.value_path.is_some();
            let payload = build_publish_payload(value, use_json_envelope);

            // 4. Map QoS.
            let qos = qos_from_u8(obj.qos);

            // 5. Require an open client.
            let client = match self.client.as_ref() {
                Some(c) => c,
                None => {
                    ctx.update_write_err(
                        point_id,
                        DriverError::CommFault("mqtt: not connected".into()),
                    );
                    continue;
                }
            };

            // A publish queued while disconnected would reach the
            // equipment late, after the reconnect (S-51).
            if !self.is_connected() {
                ctx.update_write_err(point_id, DriverError::CommFault(NOT_CONNECTED.into()));
                continue;
            }

            // 6. Publish (retain=false). Map errors to CommFault.
            match client
                .publish(topic, qos, false, payload)
                .await
                .map_err(|e| DriverError::CommFault(format!("mqtt publish: {e}")))
            {
                Ok(()) => ctx.update_write_ok(point_id),
                Err(e) => ctx.update_write_err(point_id, e),
            }
        }
    }
}

// ── Test-only helpers ──────────────────────────────────────

#[cfg(test)]
impl MqttDriver {
    /// Test helper: call `sync_cur` and collect results as a Vec.
    async fn sync_cur_vec(
        &mut self,
        points: &[DriverPointRef],
    ) -> Vec<(u32, Result<f64, DriverError>)> {
        let mut ctx = SyncContext::new();
        <Self as AsyncDriver>::sync_cur(self, points, &mut ctx).await;
        ctx.into_results()
    }

    /// Test helper: behave as if the broker connection were up, without a
    /// broker.
    fn mark_connected_for_test(&mut self) {
        let (_tx, rx) = tokio::sync::watch::channel(true);
        self.connected = Some(rx);
    }

    /// Test helper: call `write` and collect results as a Vec.
    async fn write_vec(&mut self, writes: &[(u32, f64)]) -> Vec<(u32, Result<(), DriverError>)> {
        let mut ctx = WriteContext::new();
        <Self as AsyncDriver>::write(self, writes, &mut ctx).await;
        ctx.into_results()
    }
}

// ── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_object(point_id: u32, topic: &str) -> MqttObjectConfig {
        MqttObjectConfig {
            point_id,
            subscribe_topic: Some(topic.to_string()),
            publish_topic: None,
            value_path: None,
            qos: 1,
        }
    }

    #[test]
    fn mqtt_driver_new_defaults() {
        let d = MqttDriver::new("id", "host", 1883);
        assert_eq!(*d.status(), DriverStatus::Pending);
        assert_eq!(d.driver_type(), "mqtt");
        assert_eq!(d.id(), "id");
        assert!(d.client.is_none());
        assert!(d.event_loop_task.is_none());
        assert_eq!(d.config().host, "host");
        assert_eq!(d.config().port, 1883);
        assert_eq!(d.config().keep_alive_secs, 60);
        assert!(d.objects().is_empty());
    }

    #[test]
    fn from_config_populates_objects() {
        let config = MqttConfig {
            id: "mq".into(),
            host: "h".into(),
            port: 1883,
            client_id: "cid".into(),
            username: None,
            password: None,
            tls: false,
            keep_alive_secs: 60,
            objects: vec![sample_object(100, "a/b"), sample_object(101, "a/c")],
        };
        let d = MqttDriver::from_config(config);
        assert_eq!(d.objects().len(), 2);
        assert!(d.objects().contains_key(&100));
        assert!(d.objects().contains_key(&101));
    }

    #[test]
    fn qos_parse() {
        assert_eq!(qos_from_u8(0), QoS::AtMostOnce);
        assert_eq!(qos_from_u8(1), QoS::AtLeastOnce);
        // QoS 2 is disallowed per plan and falls back to AtLeastOnce.
        assert_eq!(qos_from_u8(2), QoS::AtLeastOnce);
        assert_eq!(qos_from_u8(255), QoS::AtLeastOnce);
    }

    #[test]
    fn mqtt_config_deserializes_minimal() {
        let json = r#"{
            "id": "mq-1",
            "host": "broker",
            "client_id": "sandstar-1",
            "objects": []
        }"#;
        let cfg: MqttConfig = serde_json::from_str(json).expect("deserialize");
        assert_eq!(cfg.id, "mq-1");
        assert_eq!(cfg.host, "broker");
        assert_eq!(cfg.port, 1883); // default
        assert_eq!(cfg.client_id, "sandstar-1");
        assert!(cfg.username.is_none());
        assert!(cfg.password.is_none());
        assert!(!cfg.tls);
        assert_eq!(cfg.keep_alive_secs, 60);
        assert!(cfg.objects.is_empty());
    }

    #[test]
    fn mqtt_config_deserializes_full() {
        let json = r#"{
            "id": "mq-full",
            "host": "broker.example.com",
            "port": 8883,
            "client_id": "sandstar-full",
            "username": "u",
            "password": "p",
            "tls": true,
            "keep_alive_secs": 30,
            "objects": [
                {
                    "point_id": 103,
                    "subscribe_topic": "bldg/zone1/temp",
                    "publish_topic": "bldg/zone1/setpoint",
                    "value_path": "/value",
                    "qos": 1
                }
            ]
        }"#;
        let cfg: MqttConfig = serde_json::from_str(json).expect("deserialize");
        assert_eq!(cfg.id, "mq-full");
        assert_eq!(cfg.port, 8883);
        assert_eq!(cfg.username.as_deref(), Some("u"));
        assert_eq!(cfg.password.as_deref(), Some("p"));
        // Debug never shows the password (R-14, S-48).
        let mut cfg = cfg;
        cfg.password = Some("s3cret-pw".into());
        let shown = format!("{cfg:?}");
        assert!(shown.contains("<redacted>"));
        assert!(!shown.contains("s3cret-pw"));
        cfg.password = None;
        assert!(format!("{cfg:?}").contains("<none>"));
        assert!(cfg.tls);
        assert_eq!(cfg.keep_alive_secs, 30);
        assert_eq!(cfg.objects.len(), 1);
        let obj = &cfg.objects[0];
        assert_eq!(obj.point_id, 103);
        assert_eq!(obj.subscribe_topic.as_deref(), Some("bldg/zone1/temp"));
        assert_eq!(obj.publish_topic.as_deref(), Some("bldg/zone1/setpoint"));
        assert_eq!(obj.value_path.as_deref(), Some("/value"));
        assert_eq!(obj.qos, 1);
    }

    #[test]
    fn mqtt_object_config_qos_defaults_to_one() {
        let json = r#"{ "point_id": 7 }"#;
        let obj: MqttObjectConfig = serde_json::from_str(json).expect("deserialize");
        assert_eq!(obj.qos, 1);
        assert!(obj.subscribe_topic.is_none());
        assert!(obj.publish_topic.is_none());
        assert!(obj.value_path.is_none());
    }

    #[tokio::test]
    async fn learn_returns_one_point_per_object() {
        let config = MqttConfig {
            id: "mq".into(),
            host: "h".into(),
            port: 1883,
            client_id: "cid".into(),
            username: None,
            password: None,
            tls: false,
            keep_alive_secs: 60,
            objects: vec![
                sample_object(1, "t/1"),
                sample_object(2, "t/2"),
                sample_object(3, "t/3"),
            ],
        };
        let mut d = MqttDriver::from_config(config);
        let grid = d.learn(None).await.expect("learn ok");
        assert_eq!(grid.len(), 3);
        assert_eq!(grid[0].name, "t/1");
        assert_eq!(grid[0].address, "1");
        assert_eq!(grid[0].kind, "Number");
        assert!(grid[0].unit.is_none());
        assert!(grid[0].tags.is_empty());
        assert_eq!(grid[2].name, "t/3");
        assert_eq!(grid[2].address, "3");
    }

    #[tokio::test]
    async fn close_before_open_is_noop() {
        let mut d = MqttDriver::new("mq", "h", 1883);
        // Must not panic — no client/task yet.
        d.close().await;
        assert_eq!(*d.status(), DriverStatus::Down);
    }

    #[tokio::test]
    async fn sync_empty_points_returns_empty() {
        let mut d = MqttDriver::new("mq", "h", 1883);
        assert!(d.sync_cur_vec(&[]).await.is_empty());
    }

    #[test]
    fn poll_mode_is_manual() {
        let d = MqttDriver::new("mq", "h", 1883);
        assert_eq!(d.poll_mode(), PollMode::Manual);
    }

    #[tokio::test]
    async fn ping_without_open_returns_err() {
        let mut d = MqttDriver::new("mq", "h", 1883);
        assert!(d.ping().await.is_err());
    }

    // ── Value cache tests ──────────────────────────────────

    #[test]
    fn value_cache_insert_and_get() {
        let mut cache = MqttValueCache::new();
        cache.update("a/b", 42.5);
        let entry = cache
            .get("a/b", Duration::from_secs(60))
            .expect("should find entry");
        assert_eq!(entry.value, Ok(42.5));
        assert_eq!(cache.len(), 1);
    }

    #[tokio::test]
    async fn rejected_payload_replaces_the_last_good_value() {
        let config = MqttConfig {
            id: "mq".into(),
            host: "h".into(),
            port: 1883,
            client_id: "cid".into(),
            username: None,
            password: None,
            tls: false,
            keep_alive_secs: 60,
            objects: vec![sample_object(12, "t/12")],
        };
        let mut d = MqttDriver::from_config(config);
        d.mark_connected_for_test();
        let refs = vec![DriverPointRef {
            point_id: 12,
            address: "12".into(),
        }];
        d.cache.lock().unwrap().update("t/12", 21.5);
        assert_eq!(d.sync_cur_vec(&refs).await[0].1.as_ref().ok(), Some(&21.5));

        // A "nan" payload arrives: the point must not keep reporting 21.5.
        let reason = extract_value("nan", None).unwrap_err();
        d.cache.lock().unwrap().reject("t/12", reason);
        let results = d.sync_cur_vec(&refs).await;
        match &results[0].1 {
            Err(DriverError::RemoteStatus(msg)) => assert!(msg.contains("not finite"), "{msg}"),
            other => panic!("expected RemoteStatus, got {other:?}"),
        }

        // The next good payload is used again.
        d.cache.lock().unwrap().update("t/12", 22.0);
        assert_eq!(d.sync_cur_vec(&refs).await[0].1.as_ref().ok(), Some(&22.0));
    }

    #[tokio::test]
    async fn value_cache_stale_returns_none() {
        let mut cache = MqttValueCache::new();
        cache.update("a/b", 1.0);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(cache.get("a/b", Duration::from_millis(1)).is_none());
    }

    #[test]
    fn value_cache_remove_works() {
        let mut cache = MqttValueCache::new();
        cache.update("a/b", 1.0);
        cache.remove("a/b");
        assert!(cache.get("a/b", Duration::from_secs(60)).is_none());
        assert_eq!(cache.len(), 0);
    }

    // ── extract_value tests ────────────────────────────────

    #[test]
    fn extract_value_plain_number() {
        assert_eq!(extract_value("42.5", None).unwrap(), 42.5);
    }

    #[test]
    fn extract_value_plain_number_with_whitespace() {
        assert_eq!(extract_value("  42.5  \n", None).unwrap(), 42.5);
    }

    #[test]
    fn extract_value_plain_number_invalid() {
        assert!(extract_value("hello", None).is_err());
    }

    #[test]
    fn extract_value_rejects_non_finite_payloads() {
        for payload in [
            "nan",
            "NaN",
            "inf",
            "-inf",
            "Infinity",
            "-Infinity",
            "1e999",
        ] {
            let err = extract_value(payload, None).unwrap_err();
            assert!(err.contains("not finite"), "{payload}: {err}");
        }
        for payload in [r#"{"v":"nan"}"#, r#"{"v":"-inf"}"#] {
            let err = extract_value(payload, Some("/v")).unwrap_err();
            assert!(err.contains("not finite"), "{payload}: {err}");
        }
        // serde_json refuses an out-of-range number itself.
        let err = extract_value(r#"{"v":1e999}"#, Some("/v")).unwrap_err();
        assert!(err.contains("json parse"), "{err}");
        assert_eq!(extract_value("1.5", None).unwrap(), 1.5);
        assert_eq!(extract_value(r#"{"v":"2.5"}"#, Some("/v")).unwrap(), 2.5);
    }

    #[test]
    fn extract_value_json_nested() {
        let payload = r#"{"data":{"value":42.5}}"#;
        assert_eq!(extract_value(payload, Some("/data/value")).unwrap(), 42.5);
    }

    #[test]
    fn extract_value_json_missing_path() {
        let payload = r#"{"data":{"value":42.5}}"#;
        assert!(extract_value(payload, Some("/nope")).is_err());
    }

    #[test]
    fn extract_value_json_non_numeric() {
        let payload = r#"{"data":{"label":[1,2,3]}}"#;
        assert!(extract_value(payload, Some("/data/label")).is_err());
    }

    #[test]
    fn extract_value_json_integer_coerces_to_f64() {
        let payload = r#"{"v":42}"#;
        assert_eq!(extract_value(payload, Some("/v")).unwrap(), 42.0);
    }

    #[test]
    fn extract_value_json_bool_coerces() {
        assert_eq!(extract_value(r#"{"on":true}"#, Some("/on")).unwrap(), 1.0);
        assert_eq!(extract_value(r#"{"on":false}"#, Some("/on")).unwrap(), 0.0);
    }

    // ── sync_cur tests ─────────────────────────────────────

    #[tokio::test]
    async fn sync_cur_unknown_point_returns_config_fault() {
        let mut d = MqttDriver::new("mq", "h", 1883);
        let refs = vec![DriverPointRef {
            point_id: 9999,
            address: "9999".into(),
        }];
        let results = d.sync_cur_vec(&refs).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 9999);
        assert!(matches!(results[0].1, Err(DriverError::ConfigFault(_))));
    }

    #[tokio::test]
    async fn sync_cur_no_subscribe_topic_returns_config_fault() {
        let config = MqttConfig {
            id: "mq".into(),
            host: "h".into(),
            port: 1883,
            client_id: "cid".into(),
            username: None,
            password: None,
            tls: false,
            keep_alive_secs: 60,
            objects: vec![MqttObjectConfig {
                point_id: 10,
                subscribe_topic: None,
                publish_topic: Some("out/10".into()),
                value_path: None,
                qos: 1,
            }],
        };
        let mut d = MqttDriver::from_config(config);
        let refs = vec![DriverPointRef {
            point_id: 10,
            address: "10".into(),
        }];
        let results = d.sync_cur_vec(&refs).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 10);
        assert!(matches!(results[0].1, Err(DriverError::ConfigFault(_))));
    }

    #[tokio::test]
    async fn sync_cur_empty_cache_returns_comm_fault() {
        let config = MqttConfig {
            id: "mq".into(),
            host: "h".into(),
            port: 1883,
            client_id: "cid".into(),
            username: None,
            password: None,
            tls: false,
            keep_alive_secs: 60,
            objects: vec![sample_object(11, "t/11")],
        };
        let mut d = MqttDriver::from_config(config);
        let refs = vec![DriverPointRef {
            point_id: 11,
            address: "11".into(),
        }];
        let results = d.sync_cur_vec(&refs).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 11);
        assert!(matches!(results[0].1, Err(DriverError::CommFault(_))));
    }

    #[tokio::test]
    async fn sync_cur_fresh_cache_hit() {
        let config = MqttConfig {
            id: "mq".into(),
            host: "h".into(),
            port: 1883,
            client_id: "cid".into(),
            username: None,
            password: None,
            tls: false,
            keep_alive_secs: 60,
            objects: vec![sample_object(12, "t/12")],
        };
        let mut d = MqttDriver::from_config(config);
        d.mark_connected_for_test();
        // Populate the cache directly — tests can reach private fields
        // because they live in the same module.
        d.cache.lock().unwrap().update("t/12", 73.25);
        let refs = vec![DriverPointRef {
            point_id: 12,
            address: "12".into(),
        }];
        let results = d.sync_cur_vec(&refs).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 12);
        assert_eq!(results[0].1.as_ref().unwrap(), &73.25);
    }

    // ── build_publish_payload tests ────────────────────────

    #[test]
    fn build_publish_payload_plain() {
        assert_eq!(build_publish_payload(72.5, false), b"72.5".to_vec());
    }

    #[test]
    fn build_publish_payload_plain_integer_like() {
        // Rust's f64::Display strips the trailing zero for whole numbers,
        // so 42.0_f64.to_string() yields "42". Asserting the actual
        // Rust behaviour here so any regression from a future std change
        // shows up loudly.
        assert_eq!(build_publish_payload(42.0, false), b"42".to_vec());
    }

    #[test]
    fn build_publish_payload_json() {
        let bytes = build_publish_payload(72.5, true);
        let v: serde_json::Value =
            serde_json::from_slice(&bytes).expect("payload must be valid JSON");
        assert_eq!(v.get("value").and_then(|x| x.as_f64()), Some(72.5));
    }

    #[test]
    fn build_publish_payload_negative() {
        assert_eq!(build_publish_payload(-1.5, false), b"-1.5".to_vec());
    }

    // ── write() tests ──────────────────────────────────────

    #[tokio::test]
    async fn write_empty_returns_empty() {
        let mut d = MqttDriver::new("mq", "h", 1883);
        assert!(d.write_vec(&[]).await.is_empty());
    }

    #[tokio::test]
    async fn write_unknown_point_returns_config_fault() {
        let mut d = MqttDriver::new("mq", "h", 1883);
        let results = d.write_vec(&[(999, 1.0)]).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 999);
        match &results[0].1 {
            Err(DriverError::ConfigFault(msg)) => {
                assert!(
                    msg.contains("no MQTT object configured"),
                    "unexpected config-fault message: {msg}"
                );
            }
            other => panic!("expected ConfigFault, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_no_publish_topic_returns_config_fault() {
        let config = MqttConfig {
            id: "mq".into(),
            host: "h".into(),
            port: 1883,
            client_id: "cid".into(),
            username: None,
            password: None,
            tls: false,
            keep_alive_secs: 60,
            objects: vec![MqttObjectConfig {
                point_id: 20,
                subscribe_topic: Some("in/20".into()),
                publish_topic: None,
                value_path: None,
                qos: 1,
            }],
        };
        let mut d = MqttDriver::from_config(config);
        let results = d.write_vec(&[(20, 1.0)]).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 20);
        match &results[0].1 {
            Err(DriverError::ConfigFault(msg)) => {
                assert!(
                    msg.contains("no publish_topic"),
                    "unexpected config-fault message: {msg}"
                );
            }
            other => panic!("expected ConfigFault, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_not_connected_returns_comm_fault() {
        let config = MqttConfig {
            id: "mq".into(),
            host: "h".into(),
            port: 1883,
            client_id: "cid".into(),
            username: None,
            password: None,
            tls: false,
            keep_alive_secs: 60,
            objects: vec![MqttObjectConfig {
                point_id: 30,
                subscribe_topic: None,
                publish_topic: Some("out/30".into()),
                value_path: None,
                qos: 1,
            }],
        };
        let mut d = MqttDriver::from_config(config);
        // No open() called — client should be None.
        assert!(d.client.is_none());
        let results = d.write_vec(&[(30, 7.0)]).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 30);
        match &results[0].1 {
            Err(DriverError::CommFault(msg)) => {
                assert_eq!(msg, "mqtt: not connected");
            }
            other => panic!("expected CommFault, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_three_points_preserves_order() {
        // No objects configured — every write errors with ConfigFault —
        // but the point_ids must appear in the returned Vec in input order.
        let mut d = MqttDriver::new("mq", "h", 1883);
        let results = d.write_vec(&[(1, 1.0), (2, 2.0), (3, 3.0)]).await;
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, 1);
        assert_eq!(results[1].0, 2);
        assert_eq!(results[2].0, 3);
        for r in &results {
            assert!(matches!(r.1, Err(DriverError::ConfigFault(_))));
        }
    }

    #[test]
    fn a_password_of_the_wrong_type_is_not_quoted_in_the_error() {
        let json =
            serde_json::json!({"id": "m", "host": "h", "client_id": "c", "password": 987654321});
        let err = serde_json::from_value::<MqttConfig>(json)
            .unwrap_err()
            .to_string();
        assert!(!err.contains("987654321"), "{err}");
        assert!(err.contains("secret must be a string"));
    }

    // ── Reconnect and connection state (R-25, S-51) ─────────

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    enum BrokerCmd {
        /// Send a QoS 0 PUBLISH to the connected client.
        Publish(String, String),
        /// Close the current connection.
        Drop,
    }

    /// A minimal MQTT 3.1.1 broker: CONNACK, SUBACK, PUBACK, PINGRESP, and
    /// publishes on command. While `down` is set it closes every new
    /// connection at once.
    struct MockBroker {
        port: u16,
        cmd: tokio::sync::mpsc::Sender<BrokerCmd>,
        down: Arc<AtomicBool>,
        connects: Arc<AtomicUsize>,
        subscribed: Arc<Mutex<Vec<String>>>,
        published: Arc<Mutex<Vec<(String, String)>>>,
        /// Whether QoS 1 publishes are acknowledged (default: yes).
        ack_publishes: Arc<AtomicBool>,
    }

    fn remaining_length(mut n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut b = (n % 128) as u8;
            n /= 128;
            if n > 0 {
                b |= 0x80;
            }
            out.push(b);
            if n == 0 {
                return out;
            }
        }
    }

    /// Read one packet: (first header byte, body).
    async fn read_packet(s: &mut tokio::net::TcpStream) -> Option<(u8, Vec<u8>)> {
        let head = s.read_u8().await.ok()?;
        let (mut len, mut shift) = (0usize, 0u32);
        loop {
            let b = s.read_u8().await.ok()?;
            len |= ((b & 0x7f) as usize) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 21 {
                return None;
            }
        }
        let mut body = vec![0u8; len];
        s.read_exact(&mut body).await.ok()?;
        Some((head, body))
    }

    fn take_str(body: &[u8], at: usize) -> Option<(String, usize)> {
        let len = u16::from_be_bytes([*body.get(at)?, *body.get(at + 1)?]) as usize;
        let s = body.get(at + 2..at + 2 + len)?;
        Some((String::from_utf8_lossy(s).into_owned(), at + 2 + len))
    }

    impl MockBroker {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let (cmd, mut cmd_rx) = tokio::sync::mpsc::channel::<BrokerCmd>(16);
            let down = Arc::new(AtomicBool::new(false));
            let connects = Arc::new(AtomicUsize::new(0));
            let subscribed = Arc::new(Mutex::new(Vec::new()));
            let published = Arc::new(Mutex::new(Vec::new()));
            let ack_publishes = Arc::new(AtomicBool::new(true));
            let (d, c, s, p, ap) = (
                down.clone(),
                connects.clone(),
                subscribed.clone(),
                published.clone(),
                ack_publishes.clone(),
            );
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        return;
                    };
                    if d.load(Ordering::SeqCst) {
                        continue; // closes the connection
                    }
                    loop {
                        tokio::select! {
                            pkt = read_packet(&mut sock) => {
                                let Some((head, body)) = pkt else { break };
                                match head >> 4 {
                                    1 => {
                                        c.fetch_add(1, Ordering::SeqCst);
                                        let _ = sock.write_all(&[0x20, 0x02, 0x00, 0x00]).await;
                                    }
                                    8 => {
                                        let mut at = 2;
                                        let mut granted = Vec::new();
                                        while let Some((topic, next)) = take_str(&body, at) {
                                            s.lock().unwrap().push(topic);
                                            granted.push(*body.get(next).unwrap_or(&0));
                                            at = next + 1;
                                        }
                                        let mut ack = vec![0x90, (2 + granted.len()) as u8, body[0], body[1]];
                                        ack.extend(granted);
                                        let _ = sock.write_all(&ack).await;
                                    }
                                    3 => {
                                        let qos = (head >> 1) & 3;
                                        if let Some((topic, mut at)) = take_str(&body, 0) {
                                            if qos > 0 {
                                                if ap.load(Ordering::SeqCst) {
                                                    let _ = sock.write_all(&[0x40, 0x02, body[at], body[at + 1]]).await;
                                                }
                                                at += 2;
                                            }
                                            let payload = String::from_utf8_lossy(&body[at..]).into_owned();
                                            p.lock().unwrap().push((topic, payload));
                                        }
                                    }
                                    12 => { let _ = sock.write_all(&[0xD0, 0x00]).await; }
                                    14 => break,
                                    _ => {}
                                }
                            }
                            cmd = cmd_rx.recv() => match cmd {
                                Some(BrokerCmd::Publish(topic, payload)) => {
                                    let mut body = (topic.len() as u16).to_be_bytes().to_vec();
                                    body.extend(topic.as_bytes());
                                    body.extend(payload.as_bytes());
                                    let mut pkt = vec![0x30];
                                    pkt.extend(remaining_length(body.len()));
                                    pkt.extend(body);
                                    let _ = sock.write_all(&pkt).await;
                                }
                                Some(BrokerCmd::Drop) => break,
                                None => return,
                            }
                        }
                    }
                }
            });
            Self {
                port,
                cmd,
                down,
                connects,
                subscribed,
                published,
                ack_publishes,
            }
        }

        async fn publish(&self, topic: &str, payload: &str) {
            self.cmd
                .send(BrokerCmd::Publish(topic.into(), payload.into()))
                .await
                .unwrap();
        }
    }

    fn broker_driver(port: u16) -> MqttDriver {
        MqttDriver::from_config(MqttConfig {
            id: "mq".into(),
            host: "127.0.0.1".into(),
            port,
            client_id: "sandstar-test".into(),
            username: None,
            password: None,
            tls: false,
            keep_alive_secs: 60,
            objects: vec![MqttObjectConfig {
                point_id: 7,
                subscribe_topic: Some("t/7".into()),
                publish_topic: Some("t/7/set".into()),
                value_path: None,
                qos: 1,
            }],
        })
    }

    fn point7() -> Vec<DriverPointRef> {
        vec![DriverPointRef {
            point_id: 7,
            address: String::new(),
        }]
    }

    /// Wait (real time, at most `secs`) until `cond` holds.
    async fn until(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cond()
    }

    /// Poll `sync_cur` until point 7 reads `want`.
    async fn reads(d: &mut MqttDriver, want: f64, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if matches!(d.sync_cur_vec(&point7()).await[0].1, Ok(v) if v == want) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn the_reconnect_wait_doubles_up_to_a_minute_and_starts_over() {
        let mut d = ReconnectDelay::new();
        let waits: Vec<u64> = (0..9).map(|_| d.next().as_secs()).collect();
        assert_eq!(waits, vec![1, 2, 4, 8, 16, 32, 60, 60, 60]);
        d.reset();
        assert_eq!(d.next(), MQTT_RECONNECT_MIN);
    }

    #[tokio::test]
    async fn a_config_that_asks_for_tls_is_refused() {
        let mut cfg = broker_driver(1).config().clone();
        cfg.tls = true;
        let mut d = MqttDriver::from_config(cfg);
        let err = d.open().await.unwrap_err();
        assert!(matches!(err, DriverError::ConfigFault(m) if m.contains("TLS")));
        assert!(matches!(d.status(), DriverStatus::Fault(_)));
        assert!(d.client.is_none());
    }

    #[tokio::test]
    async fn open_without_a_broker_leaves_the_driver_down_and_retrying() {
        // Nothing listens on this port.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let mut d = broker_driver(port);
        d.open()
            .await
            .expect("open returns while the loop keeps trying");
        assert_eq!(d.status(), &DriverStatus::Down);
        assert!(matches!(
            &d.sync_cur_vec(&point7()).await[0].1,
            Err(DriverError::CommFault(m)) if m == NOT_CONNECTED
        ));
        assert!(d.ping().await.is_err());
        // The event loop is still alive.
        assert!(d.event_loop_task.as_ref().is_some_and(|t| !t.is_finished()));
        d.close().await;
    }

    #[tokio::test]
    async fn the_driver_reconnects_resubscribes_and_hides_nothing_meanwhile() {
        let broker = MockBroker::start().await;
        let mut d = broker_driver(broker.port);
        d.open().await.expect("open");
        assert_eq!(d.status(), &DriverStatus::Ok);
        assert!(until(3, || broker.subscribed.lock().unwrap().len() == 1).await);
        broker.publish("t/7", "21.5").await;
        assert!(reads(&mut d, 21.5, 3).await);

        // The broker goes away.
        broker.down.store(true, Ordering::SeqCst);
        broker.cmd.send(BrokerCmd::Drop).await.unwrap();
        assert!(until(3, || !d.is_connected()).await);
        // The status shows it before any poll.
        assert_eq!(d.status(), &DriverStatus::Down);
        // The value from before is not served.
        let res = d.sync_cur_vec(&point7()).await;
        assert!(
            matches!(&res[0].1, Err(DriverError::CommFault(m)) if m == NOT_CONNECTED),
            "{res:?}"
        );
        assert_eq!(d.status(), &DriverStatus::Down);
        // A write is refused, not queued.
        let w = d.write_vec(&[(7, 5.0)]).await;
        assert!(matches!(&w[0].1, Err(DriverError::CommFault(m)) if m == NOT_CONNECTED));

        // The broker comes back: reconnect and subscribe again.
        broker.down.store(false, Ordering::SeqCst);
        assert!(until(10, || d.is_connected()).await, "no reconnect");
        assert!(broker.connects.load(Ordering::SeqCst) >= 2);
        assert!(until(3, || broker.subscribed.lock().unwrap().len() == 2).await);
        // Nothing cached from before the outage.
        let res = d.sync_cur_vec(&point7()).await;
        assert!(
            matches!(&res[0].1, Err(DriverError::CommFault(_))),
            "{res:?}"
        );
        assert_eq!(d.status(), &DriverStatus::Ok);
        broker.publish("t/7", "22").await;
        assert!(reads(&mut d, 22.0, 3).await);

        // A write now goes out; the refused one never did.
        let w = d.write_vec(&[(7, 6.0)]).await;
        assert!(w[0].1.is_ok(), "{w:?}");
        assert!(until(3, || !broker.published.lock().unwrap().is_empty()).await);
        assert_eq!(
            *broker.published.lock().unwrap(),
            vec![("t/7/set".to_string(), "6".to_string())]
        );
        d.close().await;
    }

    #[tokio::test]
    async fn a_write_the_broker_never_acknowledged_is_not_sent_again_after_a_reconnect() {
        let broker = MockBroker::start().await;
        let mut d = broker_driver(broker.port);
        d.open().await.expect("open");
        assert!(until(3, || broker.subscribed.lock().unwrap().len() == 1).await);

        // The broker receives the publish but never acknowledges it, then
        // the connection drops.
        broker.ack_publishes.store(false, Ordering::SeqCst);
        let w = d.write_vec(&[(7, 5.0)]).await;
        assert!(w[0].1.is_ok(), "{w:?}");
        assert!(until(3, || broker.published.lock().unwrap().len() == 1).await);
        broker.cmd.send(BrokerCmd::Drop).await.unwrap();
        assert!(until(3, || !d.is_connected()).await);

        // After the reconnect the old publish is not delivered again.
        assert!(until(10, || d.is_connected()).await, "no reconnect");
        assert!(until(3, || broker.subscribed.lock().unwrap().len() == 2).await);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(broker.published.lock().unwrap().len(), 1);
        d.close().await;
    }
}
