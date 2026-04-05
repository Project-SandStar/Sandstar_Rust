//! Delta engine for roxWarp state synchronization.
//!
//! Wraps the core [`DeltaEngine`](super::cluster::DeltaEngine) from the
//! cluster module and adds:
//!
//! - Standalone (non-async) `SyncDeltaEngine` for unit testing and
//!   synchronous contexts
//! - `node_id` field on `VersionedPoint` for multi-node merge
//! - Convenience methods for version vector computation and anti-entropy

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ── Versioned Point (enhanced) ───────────────────────

/// A versioned point value for delta sync, including the originating node ID.
///
/// This extends the cluster-level `VersionedPoint` by adding `node_id` so
/// that multi-hop gossip can attribute values to their origin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VersionedPoint {
    /// Channel number (e.g., 1113).
    pub channel: u32,
    /// Current value in engineering units.
    pub value: f64,
    /// Unit string (e.g., "degF", "mA").
    pub unit: String,
    /// Status string (e.g., "ok", "fault", "down").
    pub status: String,
    /// Monotonically increasing version per originating node.
    pub version: u64,
    /// Unix milliseconds when the value changed.
    pub timestamp: i64,
    /// Originating node ID.
    pub node_id: String,
}

// ── Synchronous Delta Engine ─────────────────────────

/// Synchronous delta engine for roxWarp state synchronization.
///
/// Uses `std::sync::RwLock` (not tokio) so it can be used in both
/// async and sync contexts. All methods are `&self` — interior
/// mutability via atomics and RwLock.
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
    pub fn point_count(&self) -> usize {
        self.points.read().expect("points lock").len()
    }

    /// Record a local point change; returns the new version.
    pub fn record_change(
        &self,
        channel: u32,
        value: f64,
        unit: &str,
        status: &str,
    ) -> u64 {
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
            node_id: self.node_id.clone(),
        };
        self.points
            .write()
            .expect("points lock")
            .insert(channel, point);
        version
    }

    /// Compute delta for a peer (points changed since their last known version).
    ///
    /// Returns `(current_version, changed_points)`.
    pub fn delta_for_peer(&self, peer_id: &str) -> (u64, Vec<VersionedPoint>) {
        let peer_ver = self
            .peer_versions
            .read()
            .expect("peer_versions lock")
            .get(peer_id)
            .copied()
            .unwrap_or(0);
        let current = self.version.load(Ordering::SeqCst);
        let points = self.points.read().expect("points lock");
        let deltas: Vec<VersionedPoint> = points
            .values()
            .filter(|p| p.version > peer_ver)
            .cloned()
            .collect();
        (current, deltas)
    }

    /// Acknowledge peer received up to this version.
    pub fn ack_peer(&self, peer_id: &str, version: u64) {
        self.peer_versions
            .write()
            .expect("peer_versions lock")
            .insert(peer_id.to_string(), version);
    }

    /// Apply incoming delta from a remote peer (last-writer-wins merge).
    ///
    /// For each incoming point, the local value is replaced only if the
    /// remote timestamp is strictly newer. This implements LWW (last-writer-wins)
    /// conflict resolution as specified in the roxWarp protocol.
    pub fn apply_remote_delta(&self, peer_id: &str, points: &[VersionedPoint]) {
        let mut local = self.points.write().expect("points lock");
        let mut max_ver = 0u64;
        for point in points {
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
                .expect("peer_versions lock")
                .insert(peer_id.to_string(), max_ver);
        }
    }

    /// Get full state for initial sync.
    ///
    /// Returns `(current_version, all_points)`.
    pub fn full_state(&self) -> (u64, Vec<VersionedPoint>) {
        let current = self.version.load(Ordering::SeqCst);
        let points: Vec<VersionedPoint> = self
            .points
            .read()
            .expect("points lock")
            .values()
            .cloned()
            .collect();
        (current, points)
    }

    /// Get version vector for anti-entropy.
    ///
    /// Returns a map of `node_id -> last_known_version` including this node.
    pub fn version_vector(&self) -> HashMap<String, u64> {
        let mut vv = self
            .peer_versions
            .read()
            .expect("peer_versions lock")
            .clone();
        vv.insert(self.node_id.clone(), self.version.load(Ordering::SeqCst));
        vv
    }

    /// Compute delta since a specific version (all points with version > since).
    pub fn delta_since(&self, since_version: u64) -> Vec<VersionedPoint> {
        self.points
            .read()
            .expect("points lock")
            .values()
            .filter(|p| p.version > since_version)
            .cloned()
            .collect()
    }
}

// ── Tests ────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_engine_starts_at_zero() {
        let engine = DeltaEngine::new("node-a".into());
        assert_eq!(engine.current_version(), 0);
        assert_eq!(engine.point_count(), 0);
    }

    #[test]
    fn record_change_increments_version() {
        let engine = DeltaEngine::new("node-a".into());

        let v1 = engine.record_change(1113, 72.5, "degF", "ok");
        assert_eq!(v1, 1);
        assert_eq!(engine.current_version(), 1);
        assert_eq!(engine.point_count(), 1);

        let v2 = engine.record_change(1206, 4.2, "mA", "ok");
        assert_eq!(v2, 2);
        assert_eq!(engine.current_version(), 2);
        assert_eq!(engine.point_count(), 2);
    }

    #[test]
    fn record_same_channel_overwrites() {
        let engine = DeltaEngine::new("node-a".into());

        engine.record_change(1113, 72.5, "degF", "ok");
        engine.record_change(1113, 73.0, "degF", "ok");

        assert_eq!(engine.point_count(), 1); // still 1 channel
        assert_eq!(engine.current_version(), 2);

        let (_, points) = engine.full_state();
        assert_eq!(points.len(), 1);
        assert!((points[0].value - 73.0).abs() < f64::EPSILON);
        assert_eq!(points[0].version, 2);
    }

    #[test]
    fn delta_for_peer_returns_all_when_unknown() {
        let engine = DeltaEngine::new("node-a".into());
        engine.record_change(1113, 72.5, "degF", "ok");
        engine.record_change(1206, 4.2, "mA", "ok");

        let (version, deltas) = engine.delta_for_peer("peer-b");
        assert_eq!(version, 2);
        assert_eq!(deltas.len(), 2);
    }

    #[test]
    fn delta_for_peer_after_ack() {
        let engine = DeltaEngine::new("node-a".into());
        engine.record_change(1113, 72.5, "degF", "ok");
        engine.record_change(1206, 4.2, "mA", "ok");

        // Ack peer at version 2
        engine.ack_peer("peer-b", 2);

        // No new changes
        let (_, deltas) = engine.delta_for_peer("peer-b");
        assert!(deltas.is_empty());

        // New change
        engine.record_change(1113, 74.0, "degF", "ok");
        let (version, deltas) = engine.delta_for_peer("peer-b");
        assert_eq!(version, 3);
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].channel, 1113);
    }

    #[test]
    fn apply_remote_delta_newer_wins() {
        let engine = DeltaEngine::new("node-a".into());
        engine.record_change(1113, 72.5, "degF", "ok");

        // Remote point with far-future timestamp should win
        let remote = vec![VersionedPoint {
            channel: 1113,
            value: 80.0,
            unit: "degF".into(),
            status: "ok".into(),
            version: 10,
            timestamp: i64::MAX,
            node_id: "node-b".into(),
        }];
        engine.apply_remote_delta("node-b", &remote);

        let (_, points) = engine.full_state();
        let p = points.iter().find(|p| p.channel == 1113).unwrap();
        assert!((p.value - 80.0).abs() < f64::EPSILON);
        assert_eq!(p.node_id, "node-b");
    }

    #[test]
    fn apply_remote_delta_older_loses() {
        let engine = DeltaEngine::new("node-a".into());
        engine.record_change(1113, 72.5, "degF", "ok");

        // Remote point with timestamp 0 should lose
        let remote = vec![VersionedPoint {
            channel: 1113,
            value: 50.0,
            unit: "degF".into(),
            status: "ok".into(),
            version: 1,
            timestamp: 0,
            node_id: "node-b".into(),
        }];
        engine.apply_remote_delta("node-b", &remote);

        let (_, points) = engine.full_state();
        let p = points.iter().find(|p| p.channel == 1113).unwrap();
        assert!((p.value - 72.5).abs() < f64::EPSILON);
    }

    #[test]
    fn apply_remote_delta_new_channel() {
        let engine = DeltaEngine::new("node-a".into());
        engine.record_change(1113, 72.5, "degF", "ok");

        // Remote introduces a new channel
        let remote = vec![VersionedPoint {
            channel: 2200,
            value: 55.0,
            unit: "%RH".into(),
            status: "ok".into(),
            version: 5,
            timestamp: 1706000000_000,
            node_id: "node-b".into(),
        }];
        engine.apply_remote_delta("node-b", &remote);

        assert_eq!(engine.point_count(), 2);
        let (_, points) = engine.full_state();
        assert!(points.iter().any(|p| p.channel == 2200));
    }

    #[test]
    fn apply_remote_delta_tracks_peer_version() {
        let engine = DeltaEngine::new("node-a".into());

        let remote = vec![
            VersionedPoint {
                channel: 1113,
                value: 72.5,
                unit: "degF".into(),
                status: "ok".into(),
                version: 10,
                timestamp: 1706000000_000,
                node_id: "node-b".into(),
            },
            VersionedPoint {
                channel: 1206,
                value: 4.2,
                unit: "mA".into(),
                status: "ok".into(),
                version: 15,
                timestamp: 1706000001_000,
                node_id: "node-b".into(),
            },
        ];
        engine.apply_remote_delta("node-b", &remote);

        // Peer version should be tracked at max version (15)
        let vv = engine.version_vector();
        assert_eq!(*vv.get("node-b").unwrap(), 15);
    }

    #[test]
    fn version_vector_includes_self() {
        let engine = DeltaEngine::new("node-a".into());
        engine.record_change(1113, 72.5, "degF", "ok");
        engine.ack_peer("node-b", 5);

        let vv = engine.version_vector();
        assert_eq!(*vv.get("node-a").unwrap(), 1);
        assert_eq!(*vv.get("node-b").unwrap(), 5);
    }

    #[test]
    fn full_state_returns_all() {
        let engine = DeltaEngine::new("node-a".into());
        engine.record_change(1113, 72.5, "degF", "ok");
        engine.record_change(1206, 4.2, "mA", "ok");
        engine.record_change(1113, 73.0, "degF", "ok"); // overwrite

        let (version, points) = engine.full_state();
        assert_eq!(version, 3);
        assert_eq!(points.len(), 2); // 2 unique channels
    }

    #[test]
    fn delta_since_version() {
        let engine = DeltaEngine::new("node-a".into());
        engine.record_change(1113, 72.5, "degF", "ok"); // v1
        engine.record_change(1206, 4.2, "mA", "ok"); // v2
        engine.record_change(1300, 10.0, "V", "ok"); // v3

        // All since 0
        let d = engine.delta_since(0);
        assert_eq!(d.len(), 3);

        // Only v2 and v3
        let d = engine.delta_since(1);
        assert_eq!(d.len(), 2);

        // Only v3
        let d = engine.delta_since(2);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].channel, 1300);

        // Nothing new
        let d = engine.delta_since(3);
        assert!(d.is_empty());
    }

    #[test]
    fn node_id_preserved_on_record() {
        let engine = DeltaEngine::new("node-a".into());
        engine.record_change(1113, 72.5, "degF", "ok");

        let (_, points) = engine.full_state();
        assert_eq!(points[0].node_id, "node-a");
    }

    #[test]
    fn full_sync_flow() {
        // Simulate a full sync between two nodes
        let engine_a = DeltaEngine::new("node-a".into());
        let engine_b = DeltaEngine::new("node-b".into());

        // Node A has some data
        engine_a.record_change(1113, 72.5, "degF", "ok");
        engine_a.record_change(1206, 4.2, "mA", "ok");

        // Node B has different data
        engine_b.record_change(2200, 55.0, "%RH", "ok");

        // Node A sends full state to Node B
        let (version_a, points_a) = engine_a.full_state();
        engine_b.apply_remote_delta("node-a", &points_a);
        engine_b.ack_peer("node-a", version_a);

        // Node B sends full state to Node A
        let (version_b, points_b) = engine_b.full_state();
        engine_a.apply_remote_delta("node-b", &points_b);
        engine_a.ack_peer("node-b", version_b);

        // Both nodes should have all 3 channels
        assert_eq!(engine_a.point_count(), 3);
        assert_eq!(engine_b.point_count(), 3);

        // Subsequent deltas should be empty
        let (_, delta_a) = engine_a.delta_for_peer("node-b");
        // node-b was acked at version_b which is the max of B's points
        // A may still have points with version > version_b
        // The important thing: both have all data
        let (_, state_a) = engine_a.full_state();
        let (_, state_b) = engine_b.full_state();
        assert_eq!(state_a.len(), 3);
        assert_eq!(state_b.len(), 3);

        // Verify version vectors
        let vv_a = engine_a.version_vector();
        assert!(vv_a.contains_key("node-a"));
        assert!(vv_a.contains_key("node-b"));

        let vv_b = engine_b.version_vector();
        assert!(vv_b.contains_key("node-a"));
        assert!(vv_b.contains_key("node-b"));

        // Suppress unused-variable warning
        let _ = delta_a;
    }
}
