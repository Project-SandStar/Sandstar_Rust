//! Cluster manager — manages peer connections and delta distribution.
//!
//! The `ClusterManager` is the central coordinator for roxWarp clustering:
//! - Maintains the `DeltaEngine` that tracks local point state
//! - Feeds engine channel values into the delta engine
//! - Spawns outbound connection tasks for configured peers
//! - Provides cluster status for diagnostics

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::rest::EngineHandle;

// ── Configuration ─────────────────────────────────────

/// Configuration for a roxWarp cluster peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    /// Unique node ID of the peer.
    pub node_id: String,
    /// Address in `host:port` format.
    pub address: String,
    /// Whether this peer is enabled for connection.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Cluster configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    /// This node's unique ID.
    pub node_id: String,
    /// roxWarp listener port (default 7443).
    #[serde(default = "default_cluster_port")]
    pub port: u16,
    /// Peer nodes to connect to.
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
    /// Heartbeat interval in seconds.
    #[serde(default = "default_heartbeat")]
    pub heartbeat_interval_secs: u64,
    /// Anti-entropy (full version vector exchange) interval in seconds.
    #[serde(default = "default_anti_entropy")]
    pub anti_entropy_interval_secs: u64,
    /// Channel feed interval in seconds (how often to scan engine channels).
    #[serde(default = "default_feed_interval")]
    pub feed_interval_secs: u64,
    /// Path to device certificate (PEM) for mTLS.
    #[serde(default)]
    pub cert_path: Option<String>,
    /// Path to device private key (PEM) for mTLS.
    #[serde(default)]
    pub key_path: Option<String>,
    /// Path to CA certificate (PEM) for mTLS peer verification.
    #[serde(default)]
    pub ca_path: Option<String>,
}

fn default_cluster_port() -> u16 {
    7443
}
fn default_heartbeat() -> u64 {
    5
}
fn default_anti_entropy() -> u64 {
    60
}
fn default_feed_interval() -> u64 {
    1
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            node_id: format!("sandstar-{}", hostname()),
            port: 7443,
            peers: Vec::new(),
            heartbeat_interval_secs: 5,
            anti_entropy_interval_secs: 60,
            feed_interval_secs: 1,
            cert_path: None,
            key_path: None,
            ca_path: None,
        }
    }
}

impl ClusterConfig {
    /// Load cluster configuration from a JSON file.
    pub fn load(path: &str) -> Result<Self, String> {
        let data = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
        serde_json::from_str(&data).map_err(|e| format!("parse {path}: {e}"))
    }
}

/// Return a short hostname for default node ID generation.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
        .chars()
        .take(32)
        .collect()
}

// ── Peer State ────────────────────────────────────────

/// Connection state of a peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PeerState {
    Offline,
    Connecting,
    Handshake,
    Syncing,
    Active,
}

impl std::fmt::Display for PeerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerState::Offline => write!(f, "offline"),
            PeerState::Connecting => write!(f, "connecting"),
            PeerState::Handshake => write!(f, "handshake"),
            PeerState::Syncing => write!(f, "syncing"),
            PeerState::Active => write!(f, "active"),
        }
    }
}

// ── Versioned Point ───────────────────────────────────

/// A point value with version tracking for delta sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionedPoint {
    /// Channel number (e.g. 1113).
    pub channel: u32,
    /// Current value in engineering units.
    pub value: f64,
    /// Unit string (e.g. "degF", "mA").
    pub unit: String,
    /// Status string (e.g. "ok", "fault", "down").
    pub status: String,
    /// Monotonically increasing version per node.
    pub version: u64,
    /// Unix milliseconds when the value changed.
    pub timestamp: i64,
}

// ── Delta Engine ──────────────────────────────────────

/// Tracks local point state and computes deltas for peer sync.
///
/// Thread-safe: all fields use interior mutability (atomics + RwLock).
pub struct DeltaEngine {
    /// This node's unique identifier.
    pub node_id: String,
    /// Current version counter (monotonically increasing).
    version: AtomicU64,
    /// Local point state: channel -> versioned point.
    points: RwLock<HashMap<u32, VersionedPoint>>,
    /// Peer version vectors: peer_id -> last acknowledged version.
    peer_versions: RwLock<HashMap<String, u64>>,
}

impl DeltaEngine {
    /// Create a new delta engine for the given node.
    pub fn new(node_id: String) -> Self {
        Self {
            node_id,
            version: AtomicU64::new(0),
            points: RwLock::new(HashMap::new()),
            peer_versions: RwLock::new(HashMap::new()),
        }
    }

    /// Current version number.
    pub fn current_version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// Number of tracked points.
    pub async fn point_count(&self) -> usize {
        self.points.read().await.len()
    }

    /// Record a local point change; returns the new version.
    pub async fn record_change(&self, channel: u32, value: f64, unit: &str, status: &str) -> u64 {
        let version = self.version.fetch_add(1, Ordering::SeqCst) + 1;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let point = VersionedPoint {
            channel,
            value,
            unit: unit.to_string(),
            status: status.to_string(),
            version,
            timestamp: now_ms,
        };
        self.points.write().await.insert(channel, point);
        version
    }

    /// Compute delta: points that changed since a given version.
    pub async fn delta_since(&self, since_version: u64) -> Vec<VersionedPoint> {
        self.points
            .read()
            .await
            .values()
            .filter(|p| p.version > since_version)
            .cloned()
            .collect()
    }

    /// Compute delta for a specific peer based on their last known version.
    pub async fn delta_for_peer(&self, peer_id: &str) -> (u64, Vec<VersionedPoint>) {
        let peer_ver = self
            .peer_versions
            .read()
            .await
            .get(peer_id)
            .copied()
            .unwrap_or(0);
        let current = self.version.load(Ordering::SeqCst);
        let deltas = self.delta_since(peer_ver).await;
        (current, deltas)
    }

    /// Update a peer's acknowledged version after successful sync.
    pub async fn ack_peer(&self, peer_id: &str, version: u64) {
        self.peer_versions
            .write()
            .await
            .insert(peer_id.to_string(), version);
    }

    /// Apply a remote delta from a peer (last-writer-wins by timestamp).
    pub async fn apply_remote_delta(&self, peer_id: &str, points: Vec<VersionedPoint>) {
        let mut local = self.points.write().await;
        let mut max_ver = 0u64;
        for point in &points {
            max_ver = max_ver.max(point.version);
            let should_update = local
                .get(&point.channel)
                .map(|existing| point.timestamp > existing.timestamp)
                .unwrap_or(true);
            if should_update {
                local.insert(point.channel, point.clone());
            }
        }
        drop(local);

        if max_ver > 0 {
            self.peer_versions
                .write()
                .await
                .insert(peer_id.to_string(), max_ver);
        }
    }

    /// Get full state (for initial sync with a new peer).
    pub async fn full_state(&self) -> (u64, Vec<VersionedPoint>) {
        let current = self.version.load(Ordering::SeqCst);
        let points: Vec<VersionedPoint> = self.points.read().await.values().cloned().collect();
        (current, points)
    }

    /// Get the version vector: node_id -> last known version.
    pub async fn get_version_vector(&self) -> HashMap<String, u64> {
        let mut vv = self.peer_versions.read().await.clone();
        vv.insert(self.node_id.clone(), self.version.load(Ordering::SeqCst));
        vv
    }
}

// ── Cluster Manager ───────────────────────────────────

/// Manages peer connections and feeds engine data into the delta engine.
pub struct ClusterManager {
    config: ClusterConfig,
    delta_engine: Arc<DeltaEngine>,
    engine_handle: EngineHandle,
    peer_states: Arc<RwLock<HashMap<String, PeerState>>>,
}

impl ClusterManager {
    /// Create a new cluster manager.
    pub fn new(
        config: ClusterConfig,
        delta_engine: Arc<DeltaEngine>,
        engine_handle: EngineHandle,
    ) -> Self {
        Self {
            config,
            delta_engine,
            engine_handle,
            peer_states: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Reference to the delta engine (shared with handlers).
    pub fn delta_engine(&self) -> &Arc<DeltaEngine> {
        &self.delta_engine
    }

    /// Reference to the peer states (shared with handlers and peer tasks).
    pub fn peer_states(&self) -> &Arc<RwLock<HashMap<String, PeerState>>> {
        &self.peer_states
    }

    /// Start the cluster manager:
    /// 1. Start the channel feed loop (reads engine channels into delta engine)
    /// 2. Build mTLS client config if configured
    /// 3. Spawn outbound connection tasks for each configured peer
    pub async fn run(&self) {
        info!(
            node_id = %self.config.node_id,
            port = self.config.port,
            peers = self.config.peers.len(),
            "roxWarp cluster manager starting"
        );

        // Initialize peer states
        {
            let mut states = self.peer_states.write().await;
            for peer in &self.config.peers {
                if peer.enabled {
                    states.insert(peer.node_id.clone(), PeerState::Offline);
                }
            }
        }

        // Build mTLS client config for outbound connections (if configured)
        let mtls_client: super::peer::MtlsClientConfig = if let (Some(cert), Some(key), Some(ca)) = (
            &self.config.cert_path,
            &self.config.key_path,
            &self.config.ca_path,
        ) {
            match super::mtls::build_mtls_client_config(cert, key, ca) {
                Ok(cfg) => {
                    info!("roxWarp: mTLS client config loaded for outbound connections");
                    Some(cfg)
                }
                Err(e) => {
                    warn!(error = %e, "roxWarp: failed to load mTLS client config, using plain WS");
                    None
                }
            }
        } else {
            None
        };

        // Spawn the channel feed loop
        let feed_engine = self.delta_engine.clone();
        let feed_handle = self.engine_handle.clone();
        let feed_interval = self.config.feed_interval_secs;
        tokio::spawn(async move {
            channel_feed_loop(feed_engine, feed_handle, feed_interval).await;
        });

        // Spawn outbound peer connections
        for peer_config in &self.config.peers {
            if !peer_config.enabled {
                continue;
            }
            let pc = peer_config.clone();
            let de = self.delta_engine.clone();
            let ps = self.peer_states.clone();
            let hb_secs = self.config.heartbeat_interval_secs;
            let ae_secs = self.config.anti_entropy_interval_secs;
            let tls = mtls_client.clone();

            tokio::spawn(async move {
                super::peer::connect_to_peer(&pc, de, ps, hb_secs, ae_secs, tls, false).await;
            });
        }

        info!("roxWarp cluster manager started");
    }

    /// Execute a distributed query across the local delta engine.
    ///
    /// Evaluates a Haystack filter against all local points and returns
    /// matching results. In a full cluster deployment, this would also
    /// fan out to connected peers — for now, it queries the local node.
    pub async fn distributed_query(
        &self,
        filter: &str,
        limit: Option<u32>,
    ) -> Vec<super::protocol::QueryPoint> {
        let (_, all_points) = self.delta_engine.full_state().await;
        let limit = limit.unwrap_or(u32::MAX) as usize;

        all_points
            .iter()
            .filter(|p| super::handler::evaluate_point_filter(filter, p))
            .take(limit)
            .map(|p| super::protocol::QueryPoint {
                channel: p.channel,
                value: p.value,
                unit: p.unit.clone(),
                status: p.status.clone(),
                node_id: self.config.node_id.clone(),
            })
            .collect()
    }

    /// Get cluster status for REST API / diagnostics.
    pub async fn status(&self) -> ClusterStatus {
        let states = self.peer_states.read().await;
        let peers: Vec<PeerStatus> = self
            .config
            .peers
            .iter()
            .map(|pc| {
                let state = states
                    .get(&pc.node_id)
                    .cloned()
                    .unwrap_or(PeerState::Offline);
                PeerStatus {
                    node_id: pc.node_id.clone(),
                    address: pc.address.clone(),
                    state: state.to_string(),
                    enabled: pc.enabled,
                }
            })
            .collect();

        ClusterStatus {
            node_id: self.config.node_id.clone(),
            version: self.delta_engine.current_version(),
            point_count: self.delta_engine.point_count().await,
            peers,
        }
    }
}

/// Feed engine channel values into the delta engine.
///
/// Periodically reads all channels from the engine handle, compares with
/// the delta engine's stored values, and records changes.
async fn channel_feed_loop(
    delta_engine: Arc<DeltaEngine>,
    engine_handle: EngineHandle,
    interval_secs: u64,
) {
    let mut timer = tokio::time::interval(Duration::from_secs(interval_secs));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Track previous values to only record actual changes
    let mut prev_values: HashMap<u32, (f64, String)> = HashMap::new();

    loop {
        timer.tick().await;

        let channels = match engine_handle.list_channels().await {
            Ok(ch) => ch,
            Err(e) => {
                warn!(error = %e, "roxWarp feed: failed to list channels");
                continue;
            }
        };

        for ch in &channels {
            if !ch.enabled {
                continue;
            }

            let changed = prev_values
                .get(&ch.id)
                .map(|(prev_val, prev_status)| {
                    (*prev_val - ch.cur).abs() > f64::EPSILON || *prev_status != ch.status
                })
                .unwrap_or(true);

            if changed {
                // Determine unit from channel type or default
                let unit = channel_unit(&ch.channel_type);
                delta_engine
                    .record_change(ch.id, ch.cur, &unit, &ch.status)
                    .await;
                prev_values.insert(ch.id, (ch.cur, ch.status.clone()));
                debug!(
                    channel = ch.id,
                    value = ch.cur,
                    status = %ch.status,
                    "roxWarp: recorded change"
                );
            }
        }
    }
}

/// Map channel type string to a reasonable unit string.
fn channel_unit(channel_type: &str) -> String {
    match channel_type {
        "temperature" | "temp" => "degF".to_string(),
        "humidity" => "%RH".to_string(),
        "pressure" => "inH2O".to_string(),
        "current" => "mA".to_string(),
        "voltage" => "V".to_string(),
        _ => String::new(),
    }
}

// ── Status Types ──────────────────────────────────────

/// Cluster status for REST API responses.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterStatus {
    pub node_id: String,
    pub version: u64,
    pub point_count: usize,
    pub peers: Vec<PeerStatus>,
}

/// Individual peer status.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerStatus {
    pub node_id: String,
    pub address: String,
    pub state: String,
    pub enabled: bool,
}

// ── Tests ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_config_defaults() {
        let config = ClusterConfig::default();
        assert!(config.node_id.starts_with("sandstar-"));
        assert_eq!(config.port, 7443);
        assert!(config.peers.is_empty());
        assert_eq!(config.heartbeat_interval_secs, 5);
        assert_eq!(config.anti_entropy_interval_secs, 60);
        assert_eq!(config.feed_interval_secs, 1);
        assert!(config.cert_path.is_none());
        assert!(config.key_path.is_none());
        assert!(config.ca_path.is_none());
    }

    #[test]
    fn cluster_config_deserialize() {
        let json = r#"{
            "node_id": "test-node-1",
            "peers": [
                { "node_id": "peer-1", "address": "192.168.1.10:8085" },
                { "node_id": "peer-2", "address": "192.168.1.11:8085", "enabled": false }
            ],
            "heartbeat_interval_secs": 10
        }"#;
        let config: ClusterConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.node_id, "test-node-1");
        assert_eq!(config.port, 7443); // default when not specified
        assert_eq!(config.peers.len(), 2);
        assert!(config.peers[0].enabled);
        assert!(!config.peers[1].enabled);
        assert_eq!(config.heartbeat_interval_secs, 10);
        assert_eq!(config.anti_entropy_interval_secs, 60); // default
    }

    #[test]
    fn cluster_config_deserialize_with_mtls() {
        let json = r#"{
            "node_id": "test-node-1",
            "port": 9443,
            "peers": [],
            "cert_path": "/etc/sandstar/device.pem",
            "key_path": "/etc/sandstar/device-key.pem",
            "ca_path": "/etc/sandstar/ca.pem"
        }"#;
        let config: ClusterConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.port, 9443);
        assert_eq!(
            config.cert_path.as_deref(),
            Some("/etc/sandstar/device.pem")
        );
        assert_eq!(
            config.key_path.as_deref(),
            Some("/etc/sandstar/device-key.pem")
        );
        assert_eq!(config.ca_path.as_deref(), Some("/etc/sandstar/ca.pem"));
    }

    #[test]
    fn peer_state_display() {
        assert_eq!(PeerState::Offline.to_string(), "offline");
        assert_eq!(PeerState::Connecting.to_string(), "connecting");
        assert_eq!(PeerState::Handshake.to_string(), "handshake");
        assert_eq!(PeerState::Syncing.to_string(), "syncing");
        assert_eq!(PeerState::Active.to_string(), "active");
    }

    #[test]
    fn peer_state_equality() {
        assert_eq!(PeerState::Active, PeerState::Active);
        assert_ne!(PeerState::Active, PeerState::Offline);
    }

    #[test]
    fn versioned_point_serialize() {
        let point = VersionedPoint {
            channel: 1113,
            value: 73.2,
            unit: "degF".to_string(),
            status: "ok".to_string(),
            version: 42,
            timestamp: 1706000000_000,
        };
        let json = serde_json::to_string(&point).unwrap();
        assert!(json.contains("1113"));
        assert!(json.contains("73.2"));
        assert!(json.contains("degF"));

        let decoded: VersionedPoint = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.channel, 1113);
        assert!((decoded.value - 73.2).abs() < f64::EPSILON);
    }

    #[test]
    fn channel_unit_mapping() {
        assert_eq!(channel_unit("temperature"), "degF");
        assert_eq!(channel_unit("current"), "mA");
        assert_eq!(channel_unit("voltage"), "V");
        assert_eq!(channel_unit("unknown"), "");
    }

    #[test]
    fn cluster_status_serialize() {
        let status = ClusterStatus {
            node_id: "test-node".to_string(),
            version: 100,
            point_count: 42,
            peers: vec![PeerStatus {
                node_id: "peer-1".to_string(),
                address: "192.168.1.10:8085".to_string(),
                state: "active".to_string(),
                enabled: true,
            }],
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"nodeId\":\"test-node\""));
        assert!(json.contains("\"pointCount\":42"));
        assert!(json.contains("\"peers\""));
    }

    #[tokio::test]
    async fn delta_engine_record_and_retrieve() {
        let engine = DeltaEngine::new("test-node".to_string());

        // Record a change
        let v1 = engine.record_change(1113, 72.5, "degF", "ok").await;
        assert_eq!(v1, 1);
        assert_eq!(engine.current_version(), 1);
        assert_eq!(engine.point_count().await, 1);

        // Record another change
        let v2 = engine.record_change(1206, 4.2, "mA", "ok").await;
        assert_eq!(v2, 2);
        assert_eq!(engine.point_count().await, 2);

        // Delta since version 0 returns all points
        let deltas = engine.delta_since(0).await;
        assert_eq!(deltas.len(), 2);

        // Delta since version 1 returns only the second point
        let deltas = engine.delta_since(1).await;
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].channel, 1206);
    }

    #[tokio::test]
    async fn delta_engine_peer_tracking() {
        let engine = DeltaEngine::new("node-a".to_string());

        engine.record_change(1113, 72.5, "degF", "ok").await;
        engine.record_change(1206, 4.2, "mA", "ok").await;
        engine.record_change(1113, 73.0, "degF", "ok").await;

        // Peer hasn't synced yet: should get all points
        let (version, deltas) = engine.delta_for_peer("peer-b").await;
        assert_eq!(version, 3);
        assert_eq!(deltas.len(), 2); // 2 unique channels

        // Ack peer at version 3
        engine.ack_peer("peer-b", 3).await;

        // No new changes: empty delta
        let (_, deltas) = engine.delta_for_peer("peer-b").await;
        assert!(deltas.is_empty());

        // New change
        engine.record_change(1113, 74.0, "degF", "ok").await;
        let (version, deltas) = engine.delta_for_peer("peer-b").await;
        assert_eq!(version, 4);
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].channel, 1113);
    }

    #[tokio::test]
    async fn delta_engine_apply_remote() {
        let engine = DeltaEngine::new("node-a".to_string());

        // Record a local point
        engine.record_change(1113, 72.5, "degF", "ok").await;

        // Apply a remote delta with a newer timestamp
        let remote_points = vec![VersionedPoint {
            channel: 1113,
            value: 75.0,
            unit: "degF".to_string(),
            status: "ok".to_string(),
            version: 10,
            timestamp: i64::MAX, // far future = always wins
        }];
        engine.apply_remote_delta("node-b", remote_points).await;

        // The remote value should win (last-writer-wins)
        let (_, all) = engine.full_state().await;
        let point = all.iter().find(|p| p.channel == 1113).unwrap();
        assert!((point.value - 75.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn delta_engine_apply_remote_older_loses() {
        let engine = DeltaEngine::new("node-a".to_string());

        // Record a local point (timestamp will be "now")
        engine.record_change(1113, 72.5, "degF", "ok").await;

        // Apply a remote delta with timestamp 0 (ancient)
        let remote_points = vec![VersionedPoint {
            channel: 1113,
            value: 50.0,
            unit: "degF".to_string(),
            status: "ok".to_string(),
            version: 1,
            timestamp: 0,
        }];
        engine.apply_remote_delta("node-b", remote_points).await;

        // Local value should remain (it's newer)
        let (_, all) = engine.full_state().await;
        let point = all.iter().find(|p| p.channel == 1113).unwrap();
        assert!((point.value - 72.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn delta_engine_full_state() {
        let engine = DeltaEngine::new("node-a".to_string());

        engine.record_change(1113, 72.5, "degF", "ok").await;
        engine.record_change(1206, 4.2, "mA", "ok").await;

        let (version, points) = engine.full_state().await;
        assert_eq!(version, 2);
        assert_eq!(points.len(), 2);
    }

    #[tokio::test]
    async fn delta_engine_version_vector() {
        let engine = DeltaEngine::new("node-a".to_string());

        engine.record_change(1113, 72.5, "degF", "ok").await;
        engine.ack_peer("node-b", 5).await;

        let vv = engine.get_version_vector().await;
        assert_eq!(*vv.get("node-a").unwrap(), 1);
        assert_eq!(*vv.get("node-b").unwrap(), 5);
    }
}
