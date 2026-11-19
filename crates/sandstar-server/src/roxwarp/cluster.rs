//! Cluster manager — manages peer connections and delta distribution.
//!
//! The `ClusterManager` is the central coordinator for roxWarp clustering:
//! - Maintains the `DeltaEngine` that tracks local point state
//! - Feeds engine channel values into the delta engine
//! - Spawns outbound connection tasks for configured peers
//! - Provides cluster status for diagnostics

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::rest::EngineHandle;
use sandstar_engine::EngineStatus;

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
    /// Phase 9.0b: enable mDNS peer discovery. When `true`, this node
    /// advertises itself and browses for peers on the local subnet,
    /// augmenting the static `peers` list. When `false` (default),
    /// only the static list is used — no multicast traffic.
    #[serde(default)]
    pub enable_mdns: bool,
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
            enable_mdns: false,
        }
    }
}

/// Largest accepted timer interval (one day). Larger values make
/// `tokio::time::interval` overflow `Instant` and panic (R-39).
pub const MAX_INTERVAL_SECS: u64 = 86_400;

impl ClusterConfig {
    /// Load cluster configuration from a JSON file and validate it.
    pub fn load(path: &str) -> Result<Self, String> {
        let data = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
        let config: Self = serde_json::from_str(&data).map_err(|e| format!("parse {path}: {e}"))?;
        config
            .validate()
            .map_err(|e| format!("invalid {path}: {e}"))?;
        Ok(config)
    }

    /// Check the timer intervals before any task uses them.
    ///
    /// `tokio::time::interval` panics on a zero period and overflows on a
    /// huge one; with `panic = "abort"` either would kill the process (R-39).
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("heartbeat_interval_secs", self.heartbeat_interval_secs),
            (
                "anti_entropy_interval_secs",
                self.anti_entropy_interval_secs,
            ),
            ("feed_interval_secs", self.feed_interval_secs),
        ] {
            if value == 0 || value > MAX_INTERVAL_SECS {
                return Err(format!(
                    "{name} must be between 1 and {MAX_INTERVAL_SECS}, got {value}"
                ));
            }
        }
        Ok(())
    }
}

/// Return a short hostname for default node ID generation.
///
/// Fallback order: $HOSTNAME → $COMPUTERNAME → /etc/hostname → "unknown".
/// systemd strips $HOSTNAME from service child processes, so /etc/hostname
/// is the reliable fallback on Linux.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
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
    /// Unix ms of the last warn about non-finite peer values (rate limit).
    peer_non_finite_warn_ms: AtomicU64,
}

/// Minimum time between two warns about non-finite peer values.
const NON_FINITE_WARN_INTERVAL_MS: u64 = 60_000;

/// Current Unix time in milliseconds (0 if the clock is before the epoch).
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl DeltaEngine {
    /// Create a new delta engine for the given node.
    pub fn new(node_id: String) -> Self {
        Self {
            node_id,
            version: AtomicU64::new(0),
            points: RwLock::new(HashMap::new()),
            peer_versions: RwLock::new(HashMap::new()),
            peer_non_finite_warn_ms: AtomicU64::new(0),
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
    ///
    /// Returns `None` and records nothing when `value` is NaN or infinite, so the
    /// replicated state only ever holds finite values (R-45). The feed loop maps
    /// such readings to the last finite value with a fault status beforehand.
    pub async fn record_change(
        &self,
        channel: u32,
        value: f64,
        unit: &str,
        status: &str,
    ) -> Option<u64> {
        if !value.is_finite() {
            return None;
        }
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
        Some(version)
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
    ///
    /// Peer input is untrusted: a NaN or infinite value is never stored (R-45).
    /// If the channel is already known, the newer point keeps the known finite
    /// value and carries a fault status instead; otherwise it is dropped. The
    /// peer version still advances, so the peer does not resend the point forever.
    pub async fn apply_remote_delta(&self, peer_id: &str, points: Vec<VersionedPoint>) {
        let mut local = self.points.write().await;
        let mut max_ver = 0u64;
        let mut kept_as_fault = 0usize;
        let mut dropped = 0usize;
        for point in &points {
            max_ver = max_ver.max(point.version);
            let existing = local.get(&point.channel);
            let should_update = existing
                .map(|existing| point.timestamp > existing.timestamp)
                .unwrap_or(true);
            if !should_update {
                continue;
            }
            let incoming = if point.value.is_finite() {
                point.clone()
            } else if let Some(existing) = existing {
                kept_as_fault += 1;
                VersionedPoint {
                    value: existing.value,
                    unit: existing.unit.clone(),
                    status: fault_status(&point.status),
                    ..point.clone()
                }
            } else {
                dropped += 1;
                continue;
            };
            local.insert(point.channel, incoming);
        }
        drop(local);

        if kept_as_fault + dropped > 0 {
            let now_ms = unix_millis();
            let last = self.peer_non_finite_warn_ms.load(Ordering::Relaxed);
            // At most one warn per interval across all peers; the rest go to debug.
            if now_ms.saturating_sub(last) >= NON_FINITE_WARN_INTERVAL_MS
                && self
                    .peer_non_finite_warn_ms
                    .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                warn!(
                    peer = %peer_id,
                    kept_as_fault,
                    dropped,
                    total = points.len(),
                    "roxWarp: peer sent non-finite values; kept last value as fault or dropped \
                     (further reports within 60 s at debug level)"
                );
            } else {
                debug!(
                    peer = %peer_id,
                    kept_as_fault,
                    dropped,
                    total = points.len(),
                    "roxWarp: peer sent non-finite values"
                );
            }
        }

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

        // Spawn outbound peer connections for the static peer list.
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

        // Phase 9.0b: spawn mDNS responder + browser when enabled.
        // Discovered peers are merged into peer_states alongside the
        // static list, and each new one gets its own connect_to_peer
        // task. Leaked handle is OK — dropping MdnsHandle stops the
        // daemon, and we want it alive for the cluster's lifetime.
        if self.config.enable_mdns {
            let de = self.delta_engine.clone();
            let ps = self.peer_states.clone();
            let hb_secs = self.config.heartbeat_interval_secs;
            let ae_secs = self.config.anti_entropy_interval_secs;
            let tls = mtls_client.clone();
            let my_id = self.config.node_id.clone();
            let port = self.config.port;

            let on_discover = move |peer: super::mdns::DiscoveredPeer| {
                let pc = super::cluster::PeerConfig {
                    node_id: peer.node_id.clone(),
                    address: peer.address,
                    enabled: true,
                };
                let de = de.clone();
                let ps = ps.clone();
                let tls = tls.clone();
                // Dedup: if this peer is already in peer_states, skip.
                let ps_check = ps.clone();
                let pc_node_id = pc.node_id.clone();
                tokio::spawn(async move {
                    {
                        let guard = ps_check.read().await;
                        if guard.contains_key(&pc_node_id) {
                            return; // already known from static list or prior discovery
                        }
                    }
                    super::peer::connect_to_peer(&pc, de, ps, hb_secs, ae_secs, tls, false).await;
                });
            };

            match super::mdns::start_mdns(&my_id, port, on_discover) {
                Ok(handle) => {
                    info!(node_id = %my_id, port, "roxWarp: mDNS discovery enabled");
                    // Keep the handle alive for the cluster's lifetime by
                    // leaking it into a static-lifetime Box. The daemon
                    // gracefully stops when the process ends.
                    std::mem::forget(handle);
                }
                Err(e) => {
                    warn!(error = %e, "roxWarp: failed to start mDNS; continuing with static peers only");
                }
            }
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
/// Periodically reads all channels from the engine handle, compares each with
/// what this loop last recorded for it, and records changes. NaN/inf readings
/// are mapped by [`feed_decision`] (R-45).
async fn channel_feed_loop(
    delta_engine: Arc<DeltaEngine>,
    engine_handle: EngineHandle,
    interval_secs: u64,
) {
    let mut timer = tokio::time::interval(Duration::from_secs(interval_secs));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // What was last recorded per channel, to only record actual changes.
    let mut prev_values: HashMap<u32, FedValue> = HashMap::new();
    // Channels whose current reading is NaN/inf, to log only on transitions.
    let mut non_finite: HashSet<u32> = HashSet::new();

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

            let prev = prev_values.get(&ch.id);
            if !ch.cur.is_finite() {
                if non_finite.insert(ch.id) {
                    if prev.is_some() {
                        warn!(
                            channel = ch.id,
                            value = %ch.cur,
                            status = %ch.status,
                            "roxWarp feed: non-finite channel value; publishing last value as fault"
                        );
                    } else {
                        warn!(
                            channel = ch.id,
                            value = %ch.cur,
                            status = %ch.status,
                            "roxWarp feed: non-finite channel value; not published (no prior value)"
                        );
                    }
                }
            } else if non_finite.remove(&ch.id) {
                info!(
                    channel = ch.id,
                    value = ch.cur,
                    "roxWarp feed: channel value finite again"
                );
            }

            let FeedAction::Record(next) = feed_decision(prev, ch.cur, &ch.status) else {
                continue;
            };
            let unit = channel_unit(&ch.channel_type);
            match delta_engine
                .record_change(ch.id, next.value, &unit, &next.status)
                .await
            {
                Some(_) => {
                    debug!(
                        channel = ch.id,
                        value = next.value,
                        status = %next.status,
                        "roxWarp: recorded change"
                    );
                    prev_values.insert(ch.id, next);
                }
                // feed_decision only yields finite values; reaching this is a bug.
                None => warn!(
                    channel = ch.id,
                    value = next.value,
                    "roxWarp feed: delta engine rejected a value; not published"
                ),
            }
        }
    }
}

/// The value and status last recorded for a channel by the feed loop.
#[derive(Debug, Clone, PartialEq)]
struct FedValue {
    /// Always finite.
    value: f64,
    status: String,
}

/// What the feed loop does with one channel reading.
#[derive(Debug, PartialEq)]
enum FeedAction {
    /// Nothing new to publish, or a non-finite reading with no prior value.
    Skip,
    /// Record this value and status.
    Record(FedValue),
}

/// Decide what to publish for a channel reading (R-45).
///
/// A finite reading is published when value or status changed. A NaN/inf
/// reading is never published as a value: a channel that was published before
/// keeps its last finite value with a fault status; one that never was is skipped.
fn feed_decision(prev: Option<&FedValue>, cur: f64, status: &str) -> FeedAction {
    let next = if cur.is_finite() {
        FedValue {
            value: cur,
            status: status.to_string(),
        }
    } else {
        match prev {
            Some(p) => FedValue {
                value: p.value,
                status: fault_status(status),
            },
            None => return FeedAction::Skip,
        }
    };
    let changed = prev
        .map(|p| (p.value - next.value).abs() > f64::EPSILON || p.status != next.status)
        .unwrap_or(true);
    if changed {
        FeedAction::Record(next)
    } else {
        FeedAction::Skip
    }
}

/// Status published for a point whose value is not finite.
///
/// A known degraded engine status (Unknown, Stale, Disabled, Fault, Down) is
/// kept in its canonical spelling; anything else, including "Ok", "ok ",
/// an empty or an unknown peer-supplied string, becomes `Fault`, so a stale
/// value is never published as healthy.
fn fault_status(status: &str) -> String {
    const KEPT: [EngineStatus; 5] = [
        EngineStatus::Unknown,
        EngineStatus::Stale,
        EngineStatus::Disabled,
        EngineStatus::Fault,
        EngineStatus::Down,
    ];
    KEPT.iter()
        .find(|s| s.as_str().eq_ignore_ascii_case(status))
        .unwrap_or(&EngineStatus::Fault)
        .as_str()
        .to_string()
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

    fn load_json(json: &str) -> Result<ClusterConfig, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cluster.json");
        std::fs::write(&path, json).unwrap();
        ClusterConfig::load(path.to_str().unwrap())
    }

    #[test]
    fn default_config_is_valid() {
        assert!(ClusterConfig::default().validate().is_ok());
    }

    #[test]
    fn load_rejects_zero_intervals() {
        for field in [
            "heartbeat_interval_secs",
            "anti_entropy_interval_secs",
            "feed_interval_secs",
        ] {
            let err = load_json(&format!(r#"{{"node_id":"n1","{field}":0}}"#)).unwrap_err();
            assert!(err.contains(field), "{err}");
        }
    }

    #[test]
    fn load_rejects_huge_interval() {
        let err = load_json(&format!(
            r#"{{"node_id":"n1","feed_interval_secs":{}}}"#,
            u64::MAX
        ))
        .unwrap_err();
        assert!(err.contains("feed_interval_secs"), "{err}");
        assert!(load_json(r#"{"node_id":"n1","feed_interval_secs":86401}"#).is_err());
    }

    #[test]
    fn load_accepts_bounds() {
        let c = load_json(
            r#"{"node_id":"n1","heartbeat_interval_secs":1,"anti_entropy_interval_secs":86400}"#,
        )
        .unwrap();
        assert_eq!(c.heartbeat_interval_secs, 1);
        assert_eq!(c.anti_entropy_interval_secs, 86_400);
    }

    #[test]
    fn load_rejects_empty_and_malformed_json() {
        assert!(load_json("").is_err());
        assert!(load_json("{").is_err());
        assert!(load_json(r#"{"node_id":"çğü-node","feed_interval_secs":"x"}"#).is_err());
    }

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
        assert_eq!(v1, Some(1));
        assert_eq!(engine.current_version(), 1);
        assert_eq!(engine.point_count().await, 1);

        // Record another change
        let v2 = engine.record_change(1206, 4.2, "mA", "ok").await;
        assert_eq!(v2, Some(2));
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

    // ── R-45: non-finite values ─────────────────────────

    fn fed(value: f64, status: &str) -> FedValue {
        FedValue {
            value,
            status: status.to_string(),
        }
    }

    fn remote(channel: u32, value: f64, status: &str, version: u64) -> VersionedPoint {
        VersionedPoint {
            channel,
            value,
            unit: "degF".to_string(),
            status: status.to_string(),
            version,
            timestamp: i64::MAX,
        }
    }

    #[test]
    fn feed_first_finite_reading_is_recorded() {
        assert_eq!(
            feed_decision(None, 72.5, "Ok"),
            FeedAction::Record(fed(72.5, "Ok"))
        );
    }

    #[test]
    fn feed_unchanged_reading_is_skipped() {
        let prev = fed(72.5, "Ok");
        assert_eq!(feed_decision(Some(&prev), 72.5, "Ok"), FeedAction::Skip);
    }

    #[test]
    fn feed_first_non_finite_reading_is_skipped() {
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(feed_decision(None, v, "Ok"), FeedAction::Skip, "value {v}");
        }
    }

    #[test]
    fn feed_finite_to_non_finite_publishes_last_value_as_fault() {
        let prev = fed(72.5, "Ok");
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                feed_decision(Some(&prev), v, "Ok"),
                FeedAction::Record(fed(72.5, "Fault")),
                "value {v}"
            );
        }
    }

    #[test]
    fn feed_non_finite_keeps_non_ok_status() {
        let prev = fed(72.5, "Ok");
        assert_eq!(
            feed_decision(Some(&prev), f64::NAN, "Down"),
            FeedAction::Record(fed(72.5, "Down"))
        );
    }

    #[test]
    fn feed_repeated_non_finite_is_skipped() {
        let prev = fed(72.5, "Fault");
        assert_eq!(feed_decision(Some(&prev), f64::NAN, "Ok"), FeedAction::Skip);
    }

    #[test]
    fn feed_recovery_after_non_finite_is_recorded() {
        // Before R-45 the NaN comparison was always false, so recovery to the
        // same status was never published.
        let prev = fed(72.5, "Fault");
        assert_eq!(
            feed_decision(Some(&prev), 73.0, "Ok"),
            FeedAction::Record(fed(73.0, "Ok"))
        );
        // Recovery to the same value is still a status change.
        assert_eq!(
            feed_decision(Some(&prev), 72.5, "Ok"),
            FeedAction::Record(fed(72.5, "Ok"))
        );
    }

    #[test]
    fn fault_status_keeps_only_known_degraded_statuses() {
        // Healthy, malformed or unknown statuses never survive a non-finite value.
        for s in [
            "Ok", "ok", "Ok ", "", "Healthy", "Ok
",
        ] {
            assert_eq!(fault_status(s), "Fault", "status {s:?}");
        }
        // Known degraded statuses are kept, in canonical spelling.
        assert_eq!(fault_status("Stale"), "Stale");
        assert_eq!(fault_status("down"), "Down");
        assert_eq!(fault_status("DISABLED"), "Disabled");
        assert_eq!(fault_status("Unknown"), "Unknown");
        assert_eq!(fault_status("Fault"), "Fault");
    }

    #[tokio::test]
    async fn record_change_rejects_non_finite() {
        let engine = DeltaEngine::new("node-a".to_string());
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(engine.record_change(1113, v, "degF", "Ok").await, None);
        }
        assert_eq!(engine.current_version(), 0);
        assert_eq!(engine.point_count().await, 0);
    }

    #[tokio::test]
    async fn apply_remote_non_finite_unknown_channel_is_dropped() {
        let engine = DeltaEngine::new("node-a".to_string());
        engine
            .apply_remote_delta(
                "node-b",
                vec![
                    remote(1113, f64::NAN, "ok", 3),
                    remote(1206, 4.2, "ok", 4),
                    remote(1300, f64::INFINITY, "ok", 5),
                ],
            )
            .await;
        let (_, all) = engine.full_state().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].channel, 1206);
        // Peer version advances past the dropped points.
        let vv = engine.get_version_vector().await;
        assert_eq!(vv.get("node-b").copied(), Some(5));
    }

    #[tokio::test]
    async fn apply_remote_non_finite_known_channel_keeps_value_as_fault() {
        let engine = DeltaEngine::new("node-a".to_string());
        engine.record_change(1113, 72.5, "degC", "Ok").await;
        engine
            .apply_remote_delta(
                "node-b",
                vec![remote(1113, f64::NEG_INFINITY, "Healthy", 9)],
            )
            .await;
        let (_, all) = engine.full_state().await;
        let p = all.iter().find(|p| p.channel == 1113).unwrap();
        assert!((p.value - 72.5).abs() < f64::EPSILON);
        assert_eq!(p.status, "Fault");
        // The stored unit belongs to the kept value, not to the peer point.
        assert_eq!(p.unit, "degC");
        assert_eq!(p.version, 9);
    }

    #[tokio::test]
    async fn apply_remote_older_non_finite_point_is_ignored() {
        let engine = DeltaEngine::new("node-a".to_string());
        engine.record_change(1113, 72.5, "degF", "Ok").await;
        let old = VersionedPoint {
            timestamp: 0,
            ..remote(1113, f64::NAN, "ok", 4)
        };
        engine.apply_remote_delta("node-b", vec![old]).await;
        let (_, all) = engine.full_state().await;
        let p = all.iter().find(|p| p.channel == 1113).unwrap();
        assert!((p.value - 72.5).abs() < f64::EPSILON);
        assert_eq!(p.status, "Ok");
        assert_eq!(
            engine.get_version_vector().await.get("node-b").copied(),
            Some(4)
        );
    }

    #[tokio::test]
    async fn delta_with_non_finite_inputs_stays_decodable() {
        use crate::roxwarp::handler::WarpMessage;

        let engine = DeltaEngine::new("node-a".to_string());
        engine.record_change(1113, 72.5, "degF", "Ok").await;
        engine.record_change(1206, f64::NAN, "mA", "Ok").await;
        engine
            .apply_remote_delta(
                "node-b",
                vec![
                    remote(1113, f64::NAN, "ok", 2),
                    remote(1300, f64::NAN, "ok", 3),
                ],
            )
            .await;
        let (version, points) = engine.full_state().await;
        assert!(points.iter().all(|p| p.value.is_finite()));

        let msg = WarpMessage::Delta {
            node_id: "node-a".to_string(),
            from_version: 0,
            to_version: version,
            points,
        };
        // JSON writes NaN as null, which a peer cannot decode into f64.
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("null"), "{json}");
        let decoded: WarpMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, WarpMessage::Delta { ref points, .. } if points.len() == 1));

        let bytes = rmp_serde::to_vec(&msg).unwrap();
        let decoded: WarpMessage = rmp_serde::from_slice(&bytes).unwrap();
        assert!(matches!(decoded, WarpMessage::Delta { ref points, .. } if points.len() == 1));
    }
}
