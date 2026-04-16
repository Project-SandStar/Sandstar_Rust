//! MQTT pub/sub driver (Phase M2 — value cache + sync_cur).
//!
//! Implements the `AsyncDriver` lifecycle for an MQTT broker connection:
//! connecting via `rumqttc`, spawning the event-loop task, subscribing to
//! configured topics, and cleanly disconnecting on close.
//!
//! **Scope:**
//! - M1 (complete): connect / disconnect / ping / learn.
//! - M2 (this file): value cache populated by the event-loop task,
//!   `sync_cur()` returns fresh cached values or `CommFault` on stale/absent.
//! - `write()` is M3 (publish to configured topic).
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
};

// ── Config ─────────────────────────────────────────────────

/// Top-level MQTT driver configuration (deserialized from JSON).
///
/// Mirrors the schema documented in `docs/IMPLEMENTATION_PLAN_MQTT.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default)]
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

// ── Value cache ────────────────────────────────────────────

/// A single cached value with the timestamp of the last update.
#[derive(Debug, Clone)]
struct MqttCacheEntry {
    value: f64,
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
        self.entries.insert(
            topic.into(),
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
        }
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

#[async_trait]
impl AsyncDriver for MqttDriver {
    fn driver_type(&self) -> &'static str {
        "mqtt"
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn status(&self) -> &DriverStatus {
        &self.status
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

        // Spawn the event loop task. Incoming publishes are parsed and
        // pushed into the shared value cache.
        let task = tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Incoming::ConnAck(ack))) => {
                        tracing::debug!(
                            driver = %driver_id,
                            ?ack,
                            "mqtt connected"
                        );
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
                            }
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
                        tracing::warn!(
                            driver = %driver_id,
                            error = %e,
                            "mqtt event loop error"
                        );
                        break;
                    }
                }
            }
        });

        // Subscribe to every configured subscribe_topic.
        for object in &self.config.objects {
            if let Some(topic) = object.subscribe_topic.as_ref() {
                let qos = qos_from_u8(object.qos);
                if let Err(e) = client.subscribe(topic.clone(), qos).await {
                    self.status = DriverStatus::Fault(format!("subscribe failed: {e}"));
                    task.abort();
                    return Err(DriverError::CommFault(format!(
                        "mqtt subscribe '{topic}': {e}"
                    )));
                }
            }
        }

        self.client = Some(client);
        self.event_loop_task = Some(task);
        self.status = DriverStatus::Ok;

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
        self.status = DriverStatus::Down;
    }

    async fn ping(&mut self) -> Result<DriverMeta, DriverError> {
        match &self.event_loop_task {
            Some(task) if !task.is_finished() => Ok(DriverMeta {
                model: Some(format!("MQTT {}:{}", self.config.host, self.config.port)),
                ..Default::default()
            }),
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

    async fn sync_cur(
        &mut self,
        points: &[DriverPointRef],
    ) -> Vec<(u32, Result<f64, DriverError>)> {
        let mut results = Vec::with_capacity(points.len());
        for point in points {
            let obj = match self.objects.get(&point.point_id) {
                Some(o) => o,
                None => {
                    results.push((
                        point.point_id,
                        Err(DriverError::ConfigFault(format!(
                            "no mqtt object for point {}",
                            point.point_id
                        ))),
                    ));
                    continue;
                }
            };

            let topic = match obj.subscribe_topic.as_ref() {
                Some(t) => t,
                None => {
                    results.push((
                        point.point_id,
                        Err(DriverError::ConfigFault("no subscribe_topic".into())),
                    ));
                    continue;
                }
            };

            let cached = {
                let guard = match self.cache.lock() {
                    Ok(g) => g,
                    Err(_) => {
                        results.push((
                            point.point_id,
                            Err(DriverError::Internal("mqtt cache mutex poisoned".into())),
                        ));
                        continue;
                    }
                };
                guard.get(topic, MQTT_CACHE_MAX_AGE)
            };

            match cached {
                Some(entry) => results.push((point.point_id, Ok(entry.value))),
                None => results.push((
                    point.point_id,
                    Err(DriverError::CommFault("no cached value or stale".into())),
                )),
            }
        }
        results
    }

    async fn write(&mut self, _writes: &[(u32, f64)]) -> Vec<(u32, Result<(), DriverError>)> {
        // M3 will publish to the configured publish_topic.
        Vec::new()
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
    async fn write_empty_in_m2() {
        let mut d = MqttDriver::new("mq", "h", 1883);
        assert!(d.write(&[]).await.is_empty());
    }

    #[tokio::test]
    async fn sync_empty_points_returns_empty() {
        let mut d = MqttDriver::new("mq", "h", 1883);
        assert!(d.sync_cur(&[]).await.is_empty());
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
        assert_eq!(entry.value, 42.5);
        assert_eq!(cache.len(), 1);
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
        let results = d.sync_cur(&refs).await;
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
        let results = d.sync_cur(&refs).await;
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
        let results = d.sync_cur(&refs).await;
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
        // Populate the cache directly — tests can reach private fields
        // because they live in the same module.
        d.cache.lock().unwrap().update("t/12", 73.25);
        let refs = vec![DriverPointRef {
            point_id: 12,
            address: "12".into(),
        }];
        let results = d.sync_cur(&refs).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 12);
        assert_eq!(results[0].1.as_ref().unwrap(), &73.25);
    }
}
