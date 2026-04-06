//! roxWarp protocol message types and serialization.
//!
//! Defines the full set of roxWarp protocol messages including the enhanced
//! variants with capabilities, load metrics, and cluster membership (Join/Leave).
//!
//! These types are used for both JSON (Phase 1) and MessagePack (Phase 2)
//! serialization via serde.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::delta::VersionedPoint;

// ── Load Metrics ─────────────────────────────────────

/// Node health/load metrics included in heartbeat messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LoadMetrics {
    /// CPU usage percentage (0.0 - 100.0).
    pub cpu_percent: f32,
    /// Memory usage percentage (0.0 - 100.0).
    pub memory_percent: f32,
    /// Number of active channels on this node.
    pub channel_count: u32,
    /// Seconds since the node started.
    pub uptime_secs: u64,
}

impl Default for LoadMetrics {
    fn default() -> Self {
        Self {
            cpu_percent: 0.0,
            memory_percent: 0.0,
            channel_count: 0,
            uptime_secs: 0,
        }
    }
}

// ── Protocol Messages ────────────────────────────────

/// roxWarp protocol messages.
///
/// All messages are tagged with `"type"` for JSON serialization and include
/// the originating `nodeId`. The protocol supports:
///
/// - **Handshake**: Hello / Welcome (with version vectors and capabilities)
/// - **State sync**: Delta / Full / FullReq / DeltaReq
/// - **Keep-alive**: Heartbeat (with load metrics)
/// - **Anti-entropy**: Versions (periodic version vector exchange)
/// - **Cluster membership**: Join / Leave
/// - **Acknowledgment**: Ack
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum WarpMessage {
    /// Initial handshake from connecting peer.
    #[serde(rename = "warp:hello")]
    Hello {
        #[serde(rename = "nodeId")]
        node_id: String,
        versions: HashMap<String, u64>,
        #[serde(default)]
        capabilities: Vec<String>,
        /// String table entries for tag name compression (optional).
        #[serde(rename = "stringTable", default, skip_serializing_if = "Vec::is_empty")]
        string_table: Vec<String>,
        /// Component name table entries from the SOX name intern table (optional).
        #[serde(rename = "nameTable", default, skip_serializing_if = "Vec::is_empty")]
        name_table: Vec<String>,
    },

    /// Handshake response from accepting peer.
    #[serde(rename = "warp:welcome")]
    Welcome {
        #[serde(rename = "nodeId")]
        node_id: String,
        versions: HashMap<String, u64>,
        #[serde(default)]
        capabilities: Vec<String>,
        /// String table entries for tag name compression (optional).
        #[serde(rename = "stringTable", default, skip_serializing_if = "Vec::is_empty")]
        string_table: Vec<String>,
        /// Component name table entries from the SOX name intern table (optional).
        #[serde(rename = "nameTable", default, skip_serializing_if = "Vec::is_empty")]
        name_table: Vec<String>,
    },

    /// Incremental state delta.
    #[serde(rename = "warp:delta")]
    Delta {
        #[serde(rename = "nodeId")]
        node_id: String,
        #[serde(rename = "fromVersion")]
        from_version: u64,
        #[serde(rename = "toVersion")]
        to_version: u64,
        points: Vec<VersionedPoint>,
    },

    /// Request full state from a peer.
    #[serde(rename = "warp:fullReq")]
    FullReq {
        #[serde(rename = "nodeId")]
        node_id: String,
    },

    /// Full state dump.
    #[serde(rename = "warp:full")]
    Full {
        #[serde(rename = "nodeId")]
        node_id: String,
        version: u64,
        points: Vec<VersionedPoint>,
    },

    /// Keep-alive heartbeat with load metrics.
    #[serde(rename = "warp:heartbeat")]
    Heartbeat {
        #[serde(rename = "nodeId")]
        node_id: String,
        timestamp: i64,
        load: LoadMetrics,
    },

    /// Periodic version vector exchange (anti-entropy).
    #[serde(rename = "warp:versions")]
    Versions {
        #[serde(rename = "nodeId")]
        node_id: String,
        versions: HashMap<String, u64>,
    },

    /// Request deltas from specific versions.
    #[serde(rename = "warp:deltaReq")]
    DeltaReq {
        #[serde(rename = "nodeId")]
        node_id: String,
        #[serde(rename = "wantFrom")]
        want_from: HashMap<String, u64>,
    },

    /// Node joining the cluster.
    #[serde(rename = "warp:join")]
    Join {
        #[serde(rename = "nodeId")]
        node_id: String,
        address: String,
    },

    /// Node leaving the cluster.
    #[serde(rename = "warp:leave")]
    Leave {
        #[serde(rename = "nodeId")]
        node_id: String,
    },

    /// Acknowledgment of a received delta.
    #[serde(rename = "warp:ack")]
    Ack {
        #[serde(rename = "nodeId")]
        node_id: String,
        version: u64,
    },

    /// Distributed Haystack filter query.
    #[serde(rename = "warp:query")]
    Query {
        #[serde(rename = "nodeId")]
        node_id: String,
        #[serde(rename = "queryId")]
        query_id: String,
        filter: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },

    /// Response to a distributed query.
    #[serde(rename = "warp:queryResult")]
    QueryResult {
        #[serde(rename = "nodeId")]
        node_id: String,
        #[serde(rename = "queryId")]
        query_id: String,
        results: Vec<QueryPoint>,
    },
}

/// A point returned by a distributed query.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QueryPoint {
    /// Channel number.
    pub channel: u32,
    /// Current value in engineering units.
    pub value: f64,
    /// Unit string.
    pub unit: String,
    /// Status string.
    pub status: String,
    /// Originating node ID.
    #[serde(rename = "nodeId")]
    pub node_id: String,
}

// ── Serialization helpers ────────────────────────────

impl WarpMessage {
    /// Serialize to JSON string.
    pub fn to_json(&self) -> Result<String, super::RoxWarpError> {
        serde_json::to_string(self).map_err(|e| super::RoxWarpError::Encode(e.to_string()))
    }

    /// Deserialize from JSON string.
    pub fn from_json(s: &str) -> Result<Self, super::RoxWarpError> {
        serde_json::from_str(s).map_err(|e| super::RoxWarpError::Decode(e.to_string()))
    }

    /// Serialize to MessagePack bytes (binary Trio transport).
    pub fn to_msgpack(&self) -> Result<Vec<u8>, super::RoxWarpError> {
        rmp_serde::to_vec(self).map_err(|e| super::RoxWarpError::Encode(e.to_string()))
    }

    /// Deserialize from MessagePack bytes.
    pub fn from_msgpack(bytes: &[u8]) -> Result<Self, super::RoxWarpError> {
        rmp_serde::from_slice(bytes).map_err(|e| super::RoxWarpError::Decode(e.to_string()))
    }

    /// Returns the node_id from any message variant.
    pub fn node_id(&self) -> &str {
        match self {
            Self::Hello { node_id, .. }
            | Self::Welcome { node_id, .. }
            | Self::Delta { node_id, .. }
            | Self::FullReq { node_id, .. }
            | Self::Full { node_id, .. }
            | Self::Heartbeat { node_id, .. }
            | Self::Versions { node_id, .. }
            | Self::DeltaReq { node_id, .. }
            | Self::Join { node_id, .. }
            | Self::Leave { node_id, .. }
            | Self::Ack { node_id, .. }
            | Self::Query { node_id, .. }
            | Self::QueryResult { node_id, .. } => node_id,
        }
    }

    /// Returns the message type tag string.
    pub fn type_tag(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "warp:hello",
            Self::Welcome { .. } => "warp:welcome",
            Self::Delta { .. } => "warp:delta",
            Self::FullReq { .. } => "warp:fullReq",
            Self::Full { .. } => "warp:full",
            Self::Heartbeat { .. } => "warp:heartbeat",
            Self::Versions { .. } => "warp:versions",
            Self::DeltaReq { .. } => "warp:deltaReq",
            Self::Join { .. } => "warp:join",
            Self::Leave { .. } => "warp:leave",
            Self::Ack { .. } => "warp:ack",
            Self::Query { .. } => "warp:query",
            Self::QueryResult { .. } => "warp:queryResult",
        }
    }
}

// ── Standard capabilities ────────────────────────────

/// Well-known capability strings for the Hello/Welcome handshake.
pub mod capabilities {
    /// Supports binary Trio (MessagePack) encoding.
    pub const BINARY_TRIO: &str = "binaryTrio";
    /// Supports delta sync.
    pub const DELTA_SYNC: &str = "deltaSync";
    /// Supports full state sync.
    pub const FULL_SYNC: &str = "fullSync";
    /// Supports anti-entropy version vector exchange.
    pub const ANTI_ENTROPY: &str = "antiEntropy";
    /// Supports load-based routing.
    pub const LOAD_ROUTING: &str = "loadRouting";
    /// Supports distributed Haystack filter queries.
    pub const DISTRIBUTED_QUERY: &str = "distributedQuery";
    /// Supports string table compression for tag names.
    pub const STRING_TABLE: &str = "stringTable";

    /// Returns the default set of capabilities for a Sandstar node.
    pub fn defaults() -> Vec<String> {
        vec![
            BINARY_TRIO.to_string(),
            DELTA_SYNC.to_string(),
            FULL_SYNC.to_string(),
            ANTI_ENTROPY.to_string(),
            DISTRIBUTED_QUERY.to_string(),
            STRING_TABLE.to_string(),
        ]
    }
}

// ── Tests ────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::string_table::StringTable;

    fn sample_point() -> VersionedPoint {
        VersionedPoint {
            channel: 1113,
            value: 73.2,
            unit: "degF".into(),
            status: "ok".into(),
            version: 42,
            timestamp: 1706000000_000,
            node_id: "node-a".into(),
        }
    }

    // -- JSON roundtrips --

    #[test]
    fn hello_json_roundtrip() {
        let msg = WarpMessage::Hello {
            node_id: "node-a".into(),
            versions: HashMap::from([("node-a".into(), 100)]),
            capabilities: capabilities::defaults(),
            string_table: vec![],
            name_table: vec![],
        };
        let json = msg.to_json().unwrap();
        assert!(json.contains("\"type\":\"warp:hello\""));
        assert!(json.contains("\"nodeId\":\"node-a\""));
        assert!(json.contains("binaryTrio"));

        let decoded = WarpMessage::from_json(&json).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn welcome_json_roundtrip() {
        let msg = WarpMessage::Welcome {
            node_id: "node-b".into(),
            versions: HashMap::new(),
            capabilities: vec!["deltaSync".into()],
            string_table: vec![],
            name_table: vec![],
        };
        let json = msg.to_json().unwrap();
        let decoded = WarpMessage::from_json(&json).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn delta_json_roundtrip() {
        let msg = WarpMessage::Delta {
            node_id: "node-a".into(),
            from_version: 10,
            to_version: 15,
            points: vec![sample_point()],
        };
        let json = msg.to_json().unwrap();
        assert!(json.contains("\"fromVersion\":10"));
        assert!(json.contains("\"toVersion\":15"));
        let decoded = WarpMessage::from_json(&json).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn full_req_json_roundtrip() {
        let msg = WarpMessage::FullReq {
            node_id: "node-b".into(),
        };
        let json = msg.to_json().unwrap();
        let decoded = WarpMessage::from_json(&json).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn full_json_roundtrip() {
        let msg = WarpMessage::Full {
            node_id: "node-a".into(),
            version: 100,
            points: vec![sample_point()],
        };
        let json = msg.to_json().unwrap();
        let decoded = WarpMessage::from_json(&json).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn heartbeat_json_roundtrip() {
        let msg = WarpMessage::Heartbeat {
            node_id: "node-a".into(),
            timestamp: 1706000000_000,
            load: LoadMetrics {
                cpu_percent: 15.2,
                memory_percent: 42.0,
                channel_count: 140,
                uptime_secs: 86400,
            },
        };
        let json = msg.to_json().unwrap();
        assert!(json.contains("\"cpuPercent\""));
        assert!(json.contains("\"channelCount\":140"));
        let decoded = WarpMessage::from_json(&json).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn versions_json_roundtrip() {
        let msg = WarpMessage::Versions {
            node_id: "node-a".into(),
            versions: HashMap::from([
                ("node-a".into(), 1542),
                ("node-b".into(), 1200),
            ]),
        };
        let json = msg.to_json().unwrap();
        let decoded = WarpMessage::from_json(&json).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn delta_req_json_roundtrip() {
        let msg = WarpMessage::DeltaReq {
            node_id: "node-b".into(),
            want_from: HashMap::from([("node-a".into(), 1500)]),
        };
        let json = msg.to_json().unwrap();
        let decoded = WarpMessage::from_json(&json).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn join_json_roundtrip() {
        let msg = WarpMessage::Join {
            node_id: "node-c".into(),
            address: "192.168.1.20:7443".into(),
        };
        let json = msg.to_json().unwrap();
        assert!(json.contains("\"type\":\"warp:join\""));
        let decoded = WarpMessage::from_json(&json).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn leave_json_roundtrip() {
        let msg = WarpMessage::Leave {
            node_id: "node-c".into(),
        };
        let json = msg.to_json().unwrap();
        assert!(json.contains("\"type\":\"warp:leave\""));
        let decoded = WarpMessage::from_json(&json).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn ack_json_roundtrip() {
        let msg = WarpMessage::Ack {
            node_id: "node-b".into(),
            version: 42,
        };
        let json = msg.to_json().unwrap();
        let decoded = WarpMessage::from_json(&json).unwrap();
        assert_eq!(decoded, msg);
    }

    // -- MessagePack roundtrips --

    #[test]
    fn hello_msgpack_roundtrip() {
        let msg = WarpMessage::Hello {
            node_id: "node-a".into(),
            versions: HashMap::from([("node-a".into(), 100)]),
            capabilities: capabilities::defaults(),
            string_table: vec![],
            name_table: vec![],
        };
        let bytes = msg.to_msgpack().unwrap();
        let decoded = WarpMessage::from_msgpack(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn delta_msgpack_roundtrip() {
        let msg = WarpMessage::Delta {
            node_id: "node-a".into(),
            from_version: 10,
            to_version: 15,
            points: vec![sample_point()],
        };
        let bytes = msg.to_msgpack().unwrap();
        let decoded = WarpMessage::from_msgpack(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn heartbeat_msgpack_roundtrip() {
        let msg = WarpMessage::Heartbeat {
            node_id: "node-a".into(),
            timestamp: 1706000000_000,
            load: LoadMetrics::default(),
        };
        let bytes = msg.to_msgpack().unwrap();
        let decoded = WarpMessage::from_msgpack(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn msgpack_is_compact() {
        let msg = WarpMessage::Delta {
            node_id: "node-a".into(),
            from_version: 10,
            to_version: 15,
            points: vec![sample_point()],
        };
        let json_bytes = msg.to_json().unwrap().len();
        let msgpack_bytes = msg.to_msgpack().unwrap().len();
        assert!(
            msgpack_bytes < json_bytes,
            "MessagePack ({msgpack_bytes}) should be smaller than JSON ({json_bytes})"
        );
    }

    // -- Helper methods --

    #[test]
    fn node_id_accessor() {
        let msg = WarpMessage::Heartbeat {
            node_id: "node-x".into(),
            timestamp: 0,
            load: LoadMetrics::default(),
        };
        assert_eq!(msg.node_id(), "node-x");
    }

    #[test]
    fn type_tag_accessor() {
        let msg = WarpMessage::Hello {
            node_id: "n".into(),
            versions: HashMap::new(),
            capabilities: vec![],
            string_table: vec![],
            name_table: vec![],
        };
        assert_eq!(msg.type_tag(), "warp:hello");

        let msg = WarpMessage::Join {
            node_id: "n".into(),
            address: "addr".into(),
        };
        assert_eq!(msg.type_tag(), "warp:join");

        let msg = WarpMessage::Leave {
            node_id: "n".into(),
        };
        assert_eq!(msg.type_tag(), "warp:leave");
    }

    #[test]
    fn capabilities_defaults() {
        let caps = capabilities::defaults();
        assert!(caps.contains(&"binaryTrio".to_string()));
        assert!(caps.contains(&"deltaSync".to_string()));
        assert!(caps.contains(&"fullSync".to_string()));
        assert!(caps.contains(&"antiEntropy".to_string()));
        assert!(caps.contains(&"distributedQuery".to_string()));
        assert!(caps.contains(&"stringTable".to_string()));
        assert!(!caps.contains(&"loadRouting".to_string()));
    }

    #[test]
    fn load_metrics_default() {
        let m = LoadMetrics::default();
        assert!((m.cpu_percent - 0.0).abs() < f32::EPSILON);
        assert!((m.memory_percent - 0.0).abs() < f32::EPSILON);
        assert_eq!(m.channel_count, 0);
        assert_eq!(m.uptime_secs, 0);
    }

    // -- Query / QueryResult --

    #[test]
    fn query_json_roundtrip() {
        let msg = WarpMessage::Query {
            node_id: "node-a".into(),
            query_id: "q-001".into(),
            filter: "point and temp".into(),
            limit: Some(100),
        };
        let json = msg.to_json().unwrap();
        assert!(json.contains("\"type\":\"warp:query\""));
        assert!(json.contains("\"queryId\":\"q-001\""));
        assert!(json.contains("\"filter\":\"point and temp\""));
        let decoded = WarpMessage::from_json(&json).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn query_no_limit_json_roundtrip() {
        let msg = WarpMessage::Query {
            node_id: "node-a".into(),
            query_id: "q-002".into(),
            filter: "channel==1113".into(),
            limit: None,
        };
        let json = msg.to_json().unwrap();
        assert!(!json.contains("\"limit\""));
        let decoded = WarpMessage::from_json(&json).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn query_result_json_roundtrip() {
        let msg = WarpMessage::QueryResult {
            node_id: "node-b".into(),
            query_id: "q-001".into(),
            results: vec![
                QueryPoint {
                    channel: 1113,
                    value: 73.2,
                    unit: "degF".into(),
                    status: "ok".into(),
                    node_id: "node-b".into(),
                },
            ],
        };
        let json = msg.to_json().unwrap();
        assert!(json.contains("\"type\":\"warp:queryResult\""));
        assert!(json.contains("\"results\""));
        let decoded = WarpMessage::from_json(&json).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn query_msgpack_roundtrip() {
        let msg = WarpMessage::Query {
            node_id: "node-a".into(),
            query_id: "q-001".into(),
            filter: "point".into(),
            limit: Some(50),
        };
        let bytes = msg.to_msgpack().unwrap();
        let decoded = WarpMessage::from_msgpack(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn query_result_msgpack_roundtrip() {
        let msg = WarpMessage::QueryResult {
            node_id: "node-a".into(),
            query_id: "q-001".into(),
            results: vec![
                QueryPoint {
                    channel: 2200,
                    value: 55.0,
                    unit: "%RH".into(),
                    status: "ok".into(),
                    node_id: "node-a".into(),
                },
            ],
        };
        let bytes = msg.to_msgpack().unwrap();
        let decoded = WarpMessage::from_msgpack(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn query_type_tag() {
        let msg = WarpMessage::Query {
            node_id: "n".into(),
            query_id: "q".into(),
            filter: "point".into(),
            limit: None,
        };
        assert_eq!(msg.type_tag(), "warp:query");
        assert_eq!(msg.node_id(), "n");

        let msg = WarpMessage::QueryResult {
            node_id: "n".into(),
            query_id: "q".into(),
            results: vec![],
        };
        assert_eq!(msg.type_tag(), "warp:queryResult");
    }

    // -- String table in handshake --

    #[test]
    fn hello_with_string_table_roundtrip() {
        let table = StringTable::new();
        let msg = WarpMessage::Hello {
            node_id: "node-a".into(),
            versions: HashMap::from([("node-a".into(), 100)]),
            capabilities: capabilities::defaults(),
            string_table: table.to_entries(),
            name_table: vec![],
        };
        let json = msg.to_json().unwrap();
        assert!(json.contains("\"stringTable\""));
        let decoded = WarpMessage::from_json(&json).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn hello_empty_string_table_omitted() {
        let msg = WarpMessage::Hello {
            node_id: "node-a".into(),
            versions: HashMap::new(),
            capabilities: vec![],
            string_table: vec![],
            name_table: vec![],
        };
        let json = msg.to_json().unwrap();
        // Empty string table should be omitted from JSON
        assert!(!json.contains("stringTable"));
    }

    // -- All variants msgpack roundtrip --

    #[test]
    fn welcome_msgpack_roundtrip() {
        let msg = WarpMessage::Welcome {
            node_id: "node-b".into(),
            versions: HashMap::from([("node-b".into(), 200)]),
            capabilities: vec!["deltaSync".into()],
            string_table: vec![],
            name_table: vec![],
        };
        let bytes = msg.to_msgpack().unwrap();
        let decoded = WarpMessage::from_msgpack(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn full_msgpack_roundtrip() {
        let msg = WarpMessage::Full {
            node_id: "node-a".into(),
            version: 100,
            points: vec![sample_point()],
        };
        let bytes = msg.to_msgpack().unwrap();
        let decoded = WarpMessage::from_msgpack(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn full_req_msgpack_roundtrip() {
        let msg = WarpMessage::FullReq {
            node_id: "node-b".into(),
        };
        let bytes = msg.to_msgpack().unwrap();
        let decoded = WarpMessage::from_msgpack(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn versions_msgpack_roundtrip() {
        let msg = WarpMessage::Versions {
            node_id: "node-a".into(),
            versions: HashMap::from([
                ("node-a".into(), 1542),
                ("node-b".into(), 1200),
            ]),
        };
        let bytes = msg.to_msgpack().unwrap();
        let decoded = WarpMessage::from_msgpack(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn delta_req_msgpack_roundtrip() {
        let msg = WarpMessage::DeltaReq {
            node_id: "node-b".into(),
            want_from: HashMap::from([("node-a".into(), 1500)]),
        };
        let bytes = msg.to_msgpack().unwrap();
        let decoded = WarpMessage::from_msgpack(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn join_msgpack_roundtrip() {
        let msg = WarpMessage::Join {
            node_id: "node-c".into(),
            address: "192.168.1.20:7443".into(),
        };
        let bytes = msg.to_msgpack().unwrap();
        let decoded = WarpMessage::from_msgpack(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn leave_msgpack_roundtrip() {
        let msg = WarpMessage::Leave {
            node_id: "node-c".into(),
        };
        let bytes = msg.to_msgpack().unwrap();
        let decoded = WarpMessage::from_msgpack(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn ack_msgpack_roundtrip() {
        let msg = WarpMessage::Ack {
            node_id: "node-b".into(),
            version: 42,
        };
        let bytes = msg.to_msgpack().unwrap();
        let decoded = WarpMessage::from_msgpack(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn query_point_serialize() {
        let qp = QueryPoint {
            channel: 1113,
            value: 73.2,
            unit: "degF".into(),
            status: "ok".into(),
            node_id: "node-a".into(),
        };
        let json = serde_json::to_string(&qp).unwrap();
        assert!(json.contains("\"channel\":1113"));
        assert!(json.contains("\"nodeId\":\"node-a\""));
        let decoded: QueryPoint = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, qp);
    }
}
