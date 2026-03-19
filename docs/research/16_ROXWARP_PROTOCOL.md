# 16. roxWarp: Binary Trio Diff Gossip Protocol

## Overview

**roxWarp** is a gossip-based state synchronization protocol for Sandstar device clusters. It transmits **binary-encoded Trio diffs** over mTLS WebSocket connections, enabling efficient change-of-value (COV) replication across a mesh of embedded IoT devices.

**This document is a companion to [15_SOX_WEBSOCKET_MIGRATION.md](15_SOX_WEBSOCKET_MIGRATION.md) which defines the ROX protocol (Trio-over-WebSocket for client-device communication).**

### Protocol Naming

| Protocol | Full Name | Purpose | Encoding | Transport |
|----------|-----------|---------|----------|-----------|
| **SOX** | Sedona Object eXchange | Legacy device communication | Binary | UDP/DASP :1876 |
| **ROX** | Rust Object eXchange | Client ↔ Device communication | Trio (text) | WebSocket :7070 |
| **roxWarp** | ROX Binary Exchange And Mesh | Device ↔ Device cluster gossip | Binary Trio (MessagePack) | WSS+mTLS :7443 |

### Key Properties

- **Binary Trio encoding** — Haystack dicts serialized as MessagePack key-value maps (not JSON, not text Trio)
- **Delta encoding** — Only changed tags since the peer's last known version are transmitted
- **Scuttlebutt gossip** — Version vectors ensure convergence without a central coordinator
- **mTLS authentication** — RSA certificates for device-to-device trust (no SCRAM needed)
- **Text fallback** — Debug mode sends Trio text frames for human inspection

---

## Table of Contents

1. [Why Binary Trio?](#1-why-binary-trio)
2. [Binary Trio Encoding Specification](#2-binary-trio-encoding-specification)
3. [Delta Encoding & Version Vectors](#3-delta-encoding--version-vectors)
4. [roxWarp Gossip Protocol](#4-roxwarp-gossip-protocol)
5. [Wire Format Specification](#5-wire-format-specification)
6. [Rust Implementation](#6-rust-implementation)
7. [Fantom Pod: bassgRoxWarp](#7-fantom-pod-bassgroxwarp)
8. [Integration with SkySpark/Haxall](#8-integration-with-skysparkhaxall)
9. [Dependencies](#9-dependencies)
10. [Testing Strategy](#10-testing-strategy)

---

## 1. Why Binary Trio?

### 1.1 The Problem

Trio is a text format designed for human authoring. For high-frequency device-to-device state synchronization (heartbeats every 5s, COV events up to 100/sec), text encoding has drawbacks:

| Concern | Text Trio | Binary Trio (roxWarp) |
|---------|-----------|----------------------|
| COV event size | ~62 bytes | ~12-20 bytes |
| Parse overhead | String splitting + Zinc scalar parsing | MessagePack decode (zero-copy possible) |
| Encode overhead | String formatting + allocation | MessagePack encode (pre-allocated buffer) |
| Bandwidth (100 COV/sec) | ~6.2 KB/s | ~1.5 KB/s |
| Bandwidth (1000 COV/sec) | ~62 KB/s | ~15 KB/s |
| CPU cost | Higher (string ops) | Lower (binary ops) |

### 1.2 Design Principles

1. **Haystack-native** — Binary Trio preserves Haystack type fidelity (Marker, Ref, Number with units, etc.)
2. **Trio-compatible** — Any binary Trio message can be losslessly converted to/from text Trio
3. **MessagePack-based** — Uses an existing, well-specified binary format (RFC) as the container
4. **Diff-first** — Designed for delta encoding; messages carry only changed tags
5. **Self-describing** — Tag names are included in every message (no pre-shared schema required)
6. **Compact** — Type codes are single-byte; common patterns have optimized encodings

---

## 2. Binary Trio Encoding Specification

### 2.1 Overview

A binary Trio message is a **MessagePack map** where:
- **Keys** are tag names (MessagePack strings)
- **Values** are Haystack values encoded as MessagePack with type tagging

### 2.2 Haystack Value Type Codes

Each Haystack value is encoded as a MessagePack **fixext1** (1-byte type code + value) or **ext** (type code + variable-length value):

| Type Code | Haystack Type | MessagePack Encoding |
|-----------|---------------|---------------------|
| `0x00` | Null | MessagePack nil |
| `0x01` | Marker | fixext1: type=0x01, data=0x00 (1 byte marker) |
| `0x02` | Bool | MessagePack bool (true/false) |
| `0x03` | Number | ext8: type=0x03, data=[f64 big-endian] |
| `0x04` | Number+Unit | ext: type=0x04, data=[f64 big-endian + utf8 unit string] |
| `0x05` | Str | MessagePack str (native) |
| `0x06` | Ref | ext: type=0x06, data=[utf8 ref id] |
| `0x07` | Ref+Dis | ext: type=0x07, data=[u16 id_len + utf8 id + utf8 dis] |
| `0x08` | Date | ext8: type=0x08, data=[i16 year + u8 month + u8 day] (4 bytes) |
| `0x09` | Time | ext8: type=0x09, data=[u8 hour + u8 min + u8 sec + u16 ms] (5 bytes) |
| `0x0A` | DateTime | ext: type=0x0A, data=[i64 millis + utf8 tz] |
| `0x0B` | Uri | ext: type=0x0B, data=[utf8 uri string] |
| `0x0C` | Coord | ext8: type=0x0C, data=[f32 lat + f32 lng] (8 bytes) |
| `0x0D` | NA | fixext1: type=0x0D, data=0x00 |
| `0x0E` | Remove | fixext1: type=0x0E, data=0x00 |
| `0x0F` | Bin | ext: type=0x0F, data=[utf8 mime + 0x00 + raw bytes] |
| `0x10` | List | MessagePack array of typed values |
| `0x11` | Dict | MessagePack map (recursive) |
| `0x12` | Grid | ext: type=0x12, data=[msgpack-encoded grid] |

### 2.3 Optimized Encodings

For common patterns in IoT data, roxWarp uses compact representations:

**Small integers (0-127):** MessagePack positive fixint (1 byte total)
**Small strings (0-31 chars):** MessagePack fixstr (1 + N bytes)
**Markers:** Encoded as MessagePack `true` when context is unambiguous (e.g., tag presence in a diff map always means Marker)

### 2.4 Example: COV Event

**Text Trio (62 bytes):**
```
type:cov
compId:5
slotId:0
value:73.2degF
what:r
ts:1706000000
```

**Binary Trio (MessagePack, ~22 bytes):**
```
85                          # fixmap(5 entries)
  A4 74 79 70 65            # fixstr "type"
  A3 63 6F 76              # fixstr "cov"
  A6 63 6F 6D 70 49 64    # fixstr "compId"
  05                        # fixint 5
  A6 73 6C 6F 74 49 64    # fixstr "slotId"
  00                        # fixint 0
  A5 76 61 6C 75 65        # fixstr "value"
  C7 0C 04 40 52 4C CD ... # ext(type=0x04): 73.2 + "degF"
  A2 74 73                  # fixstr "ts"
  CE 65 B8 D8 00           # uint32 1706000000
```

**Size comparison:** 22 bytes vs 62 bytes text = **65% reduction**

### 2.5 String Table Optimization (Optional)

For long-running connections, roxWarp can negotiate a **string table** during handshake. Frequently used tag names are assigned 1-byte indices:

```
// String table (negotiated at connection setup)
0x00 = "type"
0x01 = "compId"
0x02 = "slotId"
0x03 = "value"
0x04 = "ts"
0x05 = "what"
0x06 = "nodeId"
0x07 = "stateVersion"
...
```

With string table, the same COV event shrinks to ~14 bytes:
```
85                    # fixmap(5)
  00 A3 63 6F 76     # idx:0="type" → "cov"
  01 05              # idx:1="compId" → 5
  02 00              # idx:2="slotId" → 0
  03 C7 0C 04 ...    # idx:3="value" → 73.2degF
  04 CE 65 B8 D8 00  # idx:4="ts" → 1706000000
```

---

## 3. Delta Encoding & Version Vectors

### 3.1 Concept

Each node maintains a **version vector** — a map of `node_id → sequence_number`. When syncing with a peer, a node only sends values that have changed since the peer's last known sequence number.

```
Node A version vector: { "A": 1542, "B": 1200, "C": 800 }
Node B version vector: { "A": 1500, "B": 1205, "C": 800 }

A sends to B: all of A's changes where A.version > 1500 (42 deltas)
B sends to A: all of B's changes where B.version > 1200 (5 deltas)
```

### 3.2 Versioned Point State

```rust
/// Each point value carries a version stamp
struct VersionedPoint {
    channel: u16,           // Channel number (e.g., 1113)
    value: HaystackValue,   // Current value with Haystack type
    status: PointStatus,    // ok, fault, disabled, etc.
    version: u64,           // Monotonically increasing per node
    timestamp: i64,         // Unix millis when value changed
}

/// Per-node state
struct NodeState {
    node_id: String,
    current_version: u64,    // Incremented on every change
    points: HashMap<u16, VersionedPoint>,
}
```

### 3.3 Delta Computation

```rust
/// Compute delta: points that changed since peer's last known version
fn compute_delta(
    local_state: &NodeState,
    peer_last_version: u64,
) -> Vec<VersionedPoint> {
    local_state.points.values()
        .filter(|p| p.version > peer_last_version)
        .cloned()
        .collect()
}
```

### 3.4 Delta Message (Binary Trio)

A delta message contains only changed points since the peer's last sync:

```
// Binary Trio delta message (MessagePack map)
{
    "type": "warp:delta",         // roxWarp delta sync
    "nodeId": "sandstar-a-001",
    "fromVersion": 1500,          // Peer's last known version
    "toVersion": 1542,            // Current version (42 changes)
    "points": [                   // Only changed points
        { "ch": 1113, "val": Number(73.2, "degF"), "st": "ok", "v": 1541, "ts": 1706000100 },
        { "ch": 1206, "val": Number(4.2, "mA"), "st": "ok", "v": 1542, "ts": 1706000200 }
    ]
}
```

### 3.5 Full State Request

When a node joins or reconnects, it requests a full state dump:

```
// Request full state
{ "type": "warp:fullReq", "nodeId": "sandstar-b-002" }

// Response: complete point table
{
    "type": "warp:full",
    "nodeId": "sandstar-a-001",
    "version": 1542,
    "points": [ ... all points ... ]
}
```

---

## 4. roxWarp Gossip Protocol

### 4.1 Protocol State Machine

```
                    ┌──────────────┐
                    │   OFFLINE    │
                    └──────┬───────┘
                           │ connect (mTLS handshake)
                    ┌──────▼───────┐
                    │  HANDSHAKE   │  Exchange node IDs + version vectors
                    └──────┬───────┘
                           │ handshake complete
                    ┌──────▼───────┐
                    │   SYNCING    │  Exchange full state or deltas
                    └──────┬───────┘
                           │ sync complete
                    ┌──────▼───────┐
              ┌─────│   ACTIVE     │─────┐
              │     └──────┬───────┘     │
              │            │             │
     heartbeat│     delta  │    query    │
     (5s)     │     push   │    req/res  │
              │            │             │
              └────────────┴─────────────┘
                           │
                           │ timeout / disconnect
                    ┌──────▼───────┐
                    │   OFFLINE    │  Reconnect with backoff
                    └──────────────┘
```

### 4.2 Message Types

| Message Type | Direction | Binary | Purpose |
|--------------|-----------|--------|---------|
| `warp:hello` | Bidirectional | Yes | Initial handshake with version vectors |
| `warp:welcome` | Response | Yes | Handshake accepted |
| `warp:fullReq` | Request | Yes | Request full state from peer |
| `warp:full` | Response | Yes | Full state dump |
| `warp:delta` | Push | Yes | Incremental state changes |
| `warp:heartbeat` | Bidirectional | Yes | Keep-alive with load metrics |
| `warp:query` | Request | Yes | Distributed Haystack filter query |
| `warp:queryResult` | Response | Yes | Query results from peer |
| `warp:join` | Broadcast | Yes | New node announcement |
| `warp:leave` | Broadcast | Yes | Graceful departure |
| `warp:ack` | Response | Yes | Acknowledgment with version |

### 4.3 Handshake Sequence

```
Node A                                  Node B
  │                                       │
  │── mTLS connect to :7443 ────────────→ │  (certificate verified)
  │                                       │
  │── warp:hello                          │
  │   nodeId: "sandstar-a-001"            │
  │   versions: {"A":1542, "B":1200}     │
  │   capabilities: ["delta","query"]    ──→ │
  │                                       │
  │                          warp:welcome │
  │                    nodeId: "sandstar-b-002"
  │                    versions: {"A":1500, "B":1205}
  │←─────────────── capabilities: ["delta","query"]
  │                                       │
  │── warp:delta (A's changes v1500→1542)──→ │
  │                                       │
  │←── warp:delta (B's changes v1200→1205)── │
  │                                       │
  │      ═══ ACTIVE (gossip loop) ═══     │
```

### 4.4 Anti-Entropy

roxWarp uses **push-based anti-entropy**: when a local value changes, the delta is immediately pushed to all connected peers. Additionally, a periodic full version vector exchange (every 60s) catches any missed updates.

```
// Periodic version vector exchange
{
    "type": "warp:versions",
    "nodeId": "sandstar-a-001",
    "versions": { "A": 1542, "B": 1205, "C": 800 }
}

// If peer detects it's behind, it requests a delta
{
    "type": "warp:deltaReq",
    "nodeId": "sandstar-b-002",
    "wantFrom": { "A": 1500 }  // "I'm at version 1500 for node A"
}
```

### 4.5 Conflict Resolution

For concurrent updates to the same point, roxWarp uses **last-writer-wins** with timestamp ordering:

```rust
fn merge_point(local: &VersionedPoint, remote: &VersionedPoint) -> &VersionedPoint {
    if remote.timestamp > local.timestamp {
        remote  // Remote is newer
    } else if remote.timestamp == local.timestamp {
        // Tie-break on node_id (lexicographic)
        if remote.node_id > local.node_id { remote } else { local }
    } else {
        local  // Local is newer
    }
}
```

---

## 5. Wire Format Specification

### 5.1 WebSocket Frame Types

| Frame Type | Usage |
|------------|-------|
| **Binary** | Normal operation: MessagePack-encoded binary Trio messages |
| **Text** | Debug mode: Trio text messages (human-readable) |
| **Ping/Pong** | WebSocket keep-alive (in addition to `warp:heartbeat`) |
| **Close** | Graceful shutdown with `warp:leave` before close frame |

### 5.2 Binary Frame Header

Each binary WebSocket frame contains a single MessagePack map. The `type` key identifies the message type. No additional framing is needed — WebSocket provides message boundaries.

### 5.3 Endpoint

```
wss://device:7443/roxwarp
```

- **TLS**: Required (mTLS with RSA certificates)
- **WebSocket subprotocol**: `roxwarp.v1` (via `Sec-WebSocket-Protocol` header)
- **Path**: `/roxwarp`

### 5.4 Debug Mode

Set via query parameter: `wss://device:7443/roxwarp?debug=trio`

In debug mode, all messages are sent as Trio text frames instead of binary. This enables debugging with standard WebSocket tools.

---

## 6. Rust Implementation

### 6.1 Binary Trio Encoder/Decoder

```rust
use rmp_serde::{Serializer, Deserializer};
use serde::{Serialize, Deserialize};
use libhaystack::val::{Value, Dict, Number, Ref, Marker};

/// Encode a Haystack Dict as binary Trio (MessagePack)
pub fn binary_trio_encode(dict: &Dict) -> Vec<u8> {
    let btrio = BinaryTrioDict::from_haystack(dict);
    rmp_serde::to_vec(&btrio).expect("MessagePack encode failed")
}

/// Decode binary Trio (MessagePack) back to Haystack Dict
pub fn binary_trio_decode(bytes: &[u8]) -> Result<Dict, BTrioError> {
    let btrio: BinaryTrioDict = rmp_serde::from_slice(bytes)?;
    Ok(btrio.to_haystack())
}

/// Intermediate representation for MessagePack serialization
#[derive(Serialize, Deserialize)]
struct BinaryTrioDict {
    #[serde(flatten)]
    tags: HashMap<String, BinaryTrioValue>,
}

/// Binary Trio value with Haystack type preservation
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum BinaryTrioValue {
    Null,
    Marker(bool),           // true = marker present
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    NumberUnit { val: f64, unit: String },
    Ref { id: String, dis: Option<String> },
    DateTime { ms: i64, tz: String },
    List(Vec<BinaryTrioValue>),
    Dict(HashMap<String, BinaryTrioValue>),
    // Typed wrapper for disambiguation
    Typed { t: u8, v: rmp_serde::Raw },
}

impl BinaryTrioDict {
    fn from_haystack(dict: &Dict) -> Self {
        let mut tags = HashMap::new();
        for (name, value) in dict.iter() {
            tags.insert(name.clone(), BinaryTrioValue::from_haystack(value));
        }
        BinaryTrioDict { tags }
    }

    fn to_haystack(&self) -> Dict {
        let mut dict = Dict::new();
        for (name, value) in &self.tags {
            dict.insert(name.clone(), value.to_haystack());
        }
        dict
    }
}
```

### 6.2 Delta Engine

```rust
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// roxWarp delta engine — tracks state and computes diffs
pub struct DeltaEngine {
    node_id: String,
    version: AtomicU64,
    /// Local point state: channel → versioned point
    points: DashMap<u16, VersionedPoint>,
    /// Peer version vectors: peer_id → last known version
    peer_versions: DashMap<String, u64>,
}

impl DeltaEngine {
    pub fn new(node_id: String) -> Self {
        Self {
            node_id,
            version: AtomicU64::new(0),
            points: DashMap::new(),
            peer_versions: DashMap::new(),
        }
    }

    /// Record a local point change, returns the new version
    pub fn record_change(&self, channel: u16, value: f64, unit: &str, status: &str) -> u64 {
        let version = self.version.fetch_add(1, Ordering::SeqCst) + 1;
        let point = VersionedPoint {
            channel,
            value,
            unit: unit.to_string(),
            status: status.to_string(),
            version,
            timestamp: chrono::Utc::now().timestamp_millis(),
        };
        self.points.insert(channel, point);
        version
    }

    /// Compute delta for a specific peer
    pub fn delta_for_peer(&self, peer_id: &str) -> (u64, Vec<VersionedPoint>) {
        let peer_ver = self.peer_versions.get(peer_id)
            .map(|v| *v)
            .unwrap_or(0);

        let current = self.version.load(Ordering::SeqCst);

        let deltas: Vec<VersionedPoint> = self.points.iter()
            .filter(|entry| entry.value().version > peer_ver)
            .map(|entry| entry.value().clone())
            .collect();

        (current, deltas)
    }

    /// Update peer's known version after successful sync
    pub fn ack_peer(&self, peer_id: &str, version: u64) {
        self.peer_versions.insert(peer_id.to_string(), version);
    }

    /// Apply remote delta from a peer
    pub fn apply_remote_delta(&self, peer_id: &str, points: Vec<VersionedPoint>) {
        for point in &points {
            // Last-writer-wins merge
            let should_update = self.points.get(&point.channel)
                .map(|existing| point.timestamp > existing.timestamp)
                .unwrap_or(true);

            if should_update {
                self.points.insert(point.channel, point.clone());
            }
        }

        // Update peer version
        if let Some(max_ver) = points.iter().map(|p| p.version).max() {
            self.peer_versions.insert(peer_id.to_string(), max_ver);
        }
    }

    /// Get full state for initial sync
    pub fn full_state(&self) -> (u64, Vec<VersionedPoint>) {
        let current = self.version.load(Ordering::SeqCst);
        let points: Vec<VersionedPoint> = self.points.iter()
            .map(|entry| entry.value().clone())
            .collect();
        (current, points)
    }
}
```

### 6.3 roxWarp Cluster Connection

```rust
use tokio_tungstenite::{connect_async_tls_with_config, Connector};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio::sync::mpsc;

/// roxWarp peer connection handler
pub struct RoxWarpPeer {
    peer_id: String,
    address: String,
    delta_engine: Arc<DeltaEngine>,
    tls_config: Arc<rustls::ClientConfig>,
    debug_mode: bool,
}

impl RoxWarpPeer {
    /// Connect to peer and start gossip loop
    pub async fn connect(&self) -> anyhow::Result<()> {
        let connector = Connector::Rustls(self.tls_config.clone());
        let url = if self.debug_mode {
            format!("wss://{}/roxwarp?debug=trio", self.address)
        } else {
            format!("wss://{}/roxwarp", self.address)
        };

        let (ws_stream, _) = connect_async_tls_with_config(
            &url, None, false, Some(connector),
        ).await?;

        let (mut ws_tx, mut ws_rx) = ws_stream.split();

        // Phase 1: Handshake
        let hello = WarpMessage::Hello {
            node_id: self.delta_engine.node_id.clone(),
            versions: self.delta_engine.get_version_vector(),
            capabilities: vec!["delta".into(), "query".into()],
        };
        ws_tx.send(self.encode_message(&hello)).await?;

        // Phase 2: Gossip loop
        let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(5));
        let mut anti_entropy_interval = tokio::time::interval(Duration::from_secs(60));

        loop {
            tokio::select! {
                msg = ws_rx.next() => {
                    match msg {
                        Some(Ok(WsMessage::Binary(data))) => {
                            self.handle_binary_message(&data, &mut ws_tx).await?;
                        }
                        Some(Ok(WsMessage::Text(text))) => {
                            self.handle_trio_message(&text, &mut ws_tx).await?;
                        }
                        Some(Ok(WsMessage::Close(_))) | None => break,
                        _ => {}
                    }
                }
                _ = heartbeat_interval.tick() => {
                    let hb = WarpMessage::Heartbeat {
                        node_id: self.delta_engine.node_id.clone(),
                        timestamp: chrono::Utc::now().timestamp_millis(),
                        load: self.collect_load_metrics(),
                    };
                    ws_tx.send(self.encode_message(&hb)).await?;
                }
                _ = anti_entropy_interval.tick() => {
                    let versions = WarpMessage::Versions {
                        node_id: self.delta_engine.node_id.clone(),
                        versions: self.delta_engine.get_version_vector(),
                    };
                    ws_tx.send(self.encode_message(&versions)).await?;
                }
            }
        }

        Ok(())
    }

    /// Encode message as binary (MessagePack) or text (Trio) based on mode
    fn encode_message(&self, msg: &WarpMessage) -> WsMessage {
        if self.debug_mode {
            WsMessage::Text(msg.to_trio())
        } else {
            WsMessage::Binary(msg.to_binary_trio())
        }
    }
}
```

### 6.4 roxWarp Message Types (Rust)

```rust
#[derive(Clone)]
enum WarpMessage {
    Hello {
        node_id: String,
        versions: HashMap<String, u64>,
        capabilities: Vec<String>,
    },
    Welcome {
        node_id: String,
        versions: HashMap<String, u64>,
        capabilities: Vec<String>,
    },
    Heartbeat {
        node_id: String,
        timestamp: i64,
        load: NodeLoad,
    },
    Delta {
        node_id: String,
        from_version: u64,
        to_version: u64,
        points: Vec<VersionedPoint>,
    },
    DeltaReq {
        node_id: String,
        want_from: HashMap<String, u64>,
    },
    Full {
        node_id: String,
        version: u64,
        points: Vec<VersionedPoint>,
    },
    FullReq {
        node_id: String,
    },
    Versions {
        node_id: String,
        versions: HashMap<String, u64>,
    },
    Query {
        request_id: String,
        filter: String,
    },
    QueryResult {
        request_id: String,
        node_id: String,
        points: Vec<VersionedPoint>,
    },
    Join {
        node_id: String,
        address: String,
        cert_fingerprint: String,
    },
    Leave {
        node_id: String,
        reason: String,
    },
    Ack {
        node_id: String,
        version: u64,
    },
}

impl WarpMessage {
    /// Serialize to MessagePack binary
    fn to_binary_trio(&self) -> Vec<u8> {
        let dict = self.to_dict();
        binary_trio_encode(&dict)
    }

    /// Serialize to Trio text (debug mode)
    fn to_trio(&self) -> String {
        let dict = self.to_dict();
        trio_encode(&dict)
    }

    /// Convert to Haystack Dict for encoding
    fn to_dict(&self) -> Dict {
        match self {
            WarpMessage::Heartbeat { node_id, timestamp, load } => {
                let mut d = Dict::new();
                d.insert("type".into(), Value::Str("warp:heartbeat".into()));
                d.insert("nodeId".into(), Value::Str(node_id.clone().into()));
                d.insert("ts".into(), Value::Number(Number::from(*timestamp as f64)));
                d.insert("cpuPercent".into(), Value::Number(Number::from(load.cpu_percent as f64)));
                d.insert("memUsedMb".into(), Value::Number(Number::from(load.mem_used_mb as f64)));
                d.insert("componentCount".into(), Value::Number(Number::from(load.component_count as f64)));
                d
            }
            WarpMessage::Delta { node_id, from_version, to_version, points } => {
                let mut d = Dict::new();
                d.insert("type".into(), Value::Str("warp:delta".into()));
                d.insert("nodeId".into(), Value::Str(node_id.clone().into()));
                d.insert("fromVersion".into(), Value::Number(Number::from(*from_version as f64)));
                d.insert("toVersion".into(), Value::Number(Number::from(*to_version as f64)));
                // Points encoded as list of dicts
                let pts: Vec<Value> = points.iter()
                    .map(|p| p.to_haystack_dict())
                    .collect();
                d.insert("points".into(), Value::List(pts.into()));
                d
            }
            // ... other variants
            _ => Dict::new(),
        }
    }
}
```

---

## 7. Fantom Pod: bassgRoxWarp

### 7.1 Overview

The `bassgRoxWarp` Fantom pod provides roxWarp protocol support for SkySpark and Haxall. It acts as a **roxWarp client**, connecting to Sandstar device clusters and consuming their state via the roxWarp gossip protocol.

**Key differences from bassgSoxWebSocket:**
- bassgSoxWebSocket: SkySpark → WebSocket/JSON → SOX/UDP → Device (bridge)
- bassgRoxWarp: SkySpark → WSS/binary Trio → Sandstar cluster (native)

### 7.2 Pod Structure

```
bassgRoxWarp/
├── build.fan                           # Build configuration
├── fan/
│   ├── RoxWarpExt.fan                  # Extension lifecycle (onStart/onStop)
│   ├── RoxWarpLib.fan                  # Library with @Axon functions
│   ├── RoxWarpService.fan              # Service lifecycle management
│   ├── RoxWarpConnection.fan           # WebSocket client to Sandstar cluster
│   ├── RoxWarpClusterManager.fan       # Multi-device cluster management
│   ├── BinaryTrioEncoder.fan           # Binary Trio ↔ MessagePack encoding
│   ├── BinaryTrioDecoder.fan           # Binary Trio ↔ Dict decoding
│   ├── DeltaEngine.fan                 # Version vector + delta computation
│   ├── GossipState.fan                 # Cluster state aggregation
│   ├── RoxWarpMessage.fan              # Base message class
│   ├── RoxWarpAxonFuncs.fan            # Axon function implementations
│   └── model/
│       ├── WarpHelloMsg.fan            # Handshake messages
│       ├── WarpDeltaMsg.fan            # Delta sync messages
│       ├── WarpHeartbeatMsg.fan        # Heartbeat messages
│       ├── WarpQueryMsg.fan            # Query messages
│       └── VersionedPoint.fan          # Point with version vector
├── test/
│   ├── BinaryTrioTest.fan             # Encoder/decoder tests
│   ├── DeltaEngineTest.fan            # Delta computation tests
│   └── RoxWarpConnectionTest.fan      # Connection lifecycle tests
└── doc/
    └── index.fandoc                    # Documentation
```

### 7.3 build.fan

```fantom
#! /usr/bin/env fan

using build

class Build : BuildPod
{
  override Void setup()
  {
    podName = "bassgRoxWarp"
    summary = "roxWarp binary Trio gossip protocol client for Sandstar clusters"
    version = Version("1.0.0")
    meta    = [
      "org.name":     "AnkaLabs",
      "license.name": "Proprietary",
      "vcs.uri":      "https://github.com/bassg/bassgRoxWarp"
    ]
    depends = [
      "sys        1.0",
      "concurrent 1.0",
      "inet       1.0",
      "web        1.0",
      "wisp       1.0",
      "util       1.0",
      "crypto     1.0",
      "haystack   3.0",
      "folio      3.0",
      "axon       3.0",
      "hx         3.0",
      "bassgCommon 1.0"
    ]
    srcDirs = [`fan/`, `fan/model/`, `test/`]
    resDirs = [`doc/`]
    docSrc  = true
    index   = [
      "skyarc.ext": "bassgRoxWarp::RoxWarpExt",
      "skyarc.lib": "bassgRoxWarp::RoxWarpLib"
    ]
  }
}
```

### 7.4 RoxWarpExt.fan — Extension Lifecycle

```fantom
//
// Copyright (c) 2026, AnkaLabs
// All Rights Reserved
//
// History:
//   05 Feb 26   Claude   Creation
//

using concurrent
using hx

**
** RoxWarpExt - SkySpark extension for roxWarp binary Trio gossip protocol.
**
** Connects to Sandstar device clusters via mTLS WebSocket,
** consuming state updates via binary Trio diff exchange.
**
@ExtMeta {
  name    = "bassgRoxWarp"
  icon    = "sync"
  depends = Str["bassgCommon"]
}
const class RoxWarpExt : HxExt
{
  private const Log log := Log.get("roxWarp")

  ** Shared service reference (thread-safe)
  private const AtomicRef serviceRef := AtomicRef(null)

  override Void onStart()
  {
    log.info("Starting roxWarp extension")

    // Create and start service
    service := RoxWarpService(rt)
    serviceRef.val = Unsafe(service)
    service.start
  }

  override Void onStop()
  {
    log.info("Stopping roxWarp extension")

    service := (serviceRef.val as Unsafe)?.val as RoxWarpService
    if (service != null)
    {
      service.stop
      serviceRef.val = null
    }
  }

  ** Get the running service (or null if not started)
  static RoxWarpService? service()
  {
    // Access via runtime extension lookup
    ext := HxContext.curHx.rt.libs.get("bassgRoxWarp", false) as RoxWarpExt
    if (ext == null) return null
    return (ext.serviceRef.val as Unsafe)?.val as RoxWarpService
  }
}
```

### 7.5 RoxWarpService.fan — Service Management

```fantom
using concurrent
using web
using wisp
using inet

**
** RoxWarpService - Manages roxWarp cluster connections and state
**
class RoxWarpService
{
  private const Log log := Log.get("roxWarp.service")

  ** SkySpark runtime
  private const HxRuntime rt

  ** Active cluster connections: nodeAddress -> RoxWarpConnection
  private const ConcurrentMap connections := ConcurrentMap()

  ** Aggregated cluster state from all connected nodes
  private const AtomicRef clusterStateRef := AtomicRef(null)

  ** Delta engine for version tracking
  private DeltaEngine? deltaEngine

  ** Heartbeat check actor
  private Actor? heartbeatActor

  ** Running flag
  private const AtomicBool running := AtomicBool(false)

  new make(HxRuntime rt)
  {
    this.rt = rt
  }

  Void start()
  {
    if (running.getAndSet(true)) return

    deltaEngine = DeltaEngine("skyspark-" + rt.name)
    clusterStateRef.val = Unsafe(GossipState())

    // Start heartbeat checker (every 10s)
    pool := ActorPool { name = "roxWarp-heartbeat"; maxThreads = 1 }
    heartbeatActor = Actor(pool) |msg|
    {
      checkHeartbeats
      return null
    }
    heartbeatActor.sendLater(10sec, "check")

    log.info("roxWarp service started")
  }

  Void stop()
  {
    if (!running.getAndSet(false)) return

    // Close all connections
    connections.each |conn, addr|
    {
      try { (conn as RoxWarpConnection).close }
      catch (Err e) { log.err("Error closing connection to $addr: $e.msg") }
    }
    connections.clear

    heartbeatActor = null
    log.info("roxWarp service stopped")
  }

  **
  ** Connect to a Sandstar device cluster node
  ** @param address hostname:port (e.g., "192.168.1.100:7443")
  ** @param caCertPath path to cluster CA certificate
  ** @param clientCertPath path to client certificate
  ** @param clientKeyPath path to client private key
  **
  RoxWarpConnection connect(Str address, Str caCertPath,
                             Str clientCertPath, Str clientKeyPath)
  {
    if (connections.containsKey(address))
      throw Err("Already connected to $address")

    conn := RoxWarpConnection(address, caCertPath,
                               clientCertPath, clientKeyPath,
                               deltaEngine)
    connections.set(address, conn)
    conn.start

    log.info("Connected to roxWarp peer: $address")
    return conn
  }

  **
  ** Disconnect from a cluster node
  **
  Void disconnect(Str address)
  {
    conn := connections.get(address) as RoxWarpConnection
    if (conn != null)
    {
      conn.close
      connections.remove(address)
      log.info("Disconnected from roxWarp peer: $address")
    }
  }

  **
  ** Get aggregated cluster state
  **
  GossipState clusterState()
  {
    return (clusterStateRef.val as Unsafe)?.val as GossipState ?: GossipState()
  }

  **
  ** Get all connected peers
  **
  Str[] connectedPeers()
  {
    result := Str[,]
    connections.each |v, k| { result.add(k) }
    return result
  }

  **
  ** Query across all connected cluster nodes
  ** @param filter Haystack filter expression
  ** @return aggregated results from all nodes
  **
  Dict[] clusterQuery(Str filter)
  {
    results := Dict[,]

    connections.each |conn, addr|
    {
      c := conn as RoxWarpConnection
      if (c != null && c.isActive)
      {
        try
        {
          nodeResults := c.query(filter)
          results.addAll(nodeResults)
        }
        catch (Err e)
        {
          log.err("Query failed for $addr: $e.msg")
        }
      }
    }

    return results
  }

  **
  ** Get statistics
  **
  Dict stats()
  {
    connCount := 0
    activeCount := 0
    totalPoints := 0

    connections.each |conn, addr|
    {
      connCount++
      c := conn as RoxWarpConnection
      if (c != null && c.isActive)
      {
        activeCount++
        totalPoints += c.pointCount
      }
    }

    return Etc.makeDict([
      "running":      running.val ? Marker.val : null,
      "connections":  Number(connCount),
      "active":       Number(activeCount),
      "totalPoints":  Number(totalPoints)
    ])
  }

  private Void checkHeartbeats()
  {
    if (!running.val) return

    now := Duration.nowTicks
    connections.each |conn, addr|
    {
      c := conn as RoxWarpConnection
      if (c != null)
      {
        elapsed := now - c.lastHeartbeat
        if (elapsed > 30sec.ticks)
        {
          log.warn("Peer $addr heartbeat timeout, reconnecting...")
          c.reconnect
        }
      }
    }

    // Reschedule
    heartbeatActor?.sendLater(10sec, "check")
  }
}
```

### 7.6 BinaryTrioEncoder.fan — Binary Trio Encoding

```fantom
using concurrent

**
** BinaryTrioEncoder - Encodes Haystack Dicts as MessagePack binary Trio
**
** MessagePack format:
**   Map { tagName(str) -> typedValue }
**
** Type codes:
**   0x01=Marker, 0x02=Bool, 0x03=Number, 0x04=Number+Unit,
**   0x05=Str, 0x06=Ref, 0x08=Date, 0x09=Time, 0x0A=DateTime
**
const class BinaryTrioEncoder
{
  private const Log log := Log.get("roxWarp.btrio")

  **
  ** Encode a Dict as binary Trio (MessagePack bytes)
  **
  Buf encode(Dict dict)
  {
    buf := Buf()

    // Write MessagePack map header
    size := 0
    dict.each |v, n| { size++ }
    writeMapHeader(buf, size)

    // Write each tag
    dict.each |val, name|
    {
      writeStr(buf, name)
      writeValue(buf, val)
    }

    return buf.flip
  }

  **
  ** Decode binary Trio (MessagePack bytes) to a Dict
  **
  Dict decode(Buf buf)
  {
    tags := Str:Obj?[:]
    size := readMapHeader(buf)

    size.times
    {
      name := readStr(buf)
      val := readValue(buf)
      tags[name] = val
    }

    return Etc.makeDict(tags)
  }

  ** Write MessagePack map header
  private Void writeMapHeader(Buf buf, Int size)
  {
    if (size <= 15)
      buf.write(0x80.or(size))  // fixmap
    else if (size <= 0xFFFF)
    {
      buf.write(0xDE)  // map16
      buf.writeI2(size)
    }
    else
    {
      buf.write(0xDF)  // map32
      buf.writeI4(size)
    }
  }

  ** Write MessagePack string
  private Void writeStr(Buf buf, Str s)
  {
    bytes := s.toBuf
    len := bytes.remaining
    if (len <= 31)
      buf.write(0xA0.or(len))  // fixstr
    else if (len <= 0xFF)
    {
      buf.write(0xD9)  // str8
      buf.write(len)
    }
    else
    {
      buf.write(0xDA)  // str16
      buf.writeI2(len)
    }
    buf.writeBuf(bytes)
  }

  ** Write a Haystack value as MessagePack
  private Void writeValue(Buf buf, Obj? val)
  {
    if (val == null)
    {
      buf.write(0xC0)  // nil
    }
    else if (val is Marker)
    {
      buf.write(0xC3)  // true (Marker = present)
    }
    else if (val is Bool)
    {
      buf.write((val as Bool).val ? 0xC3 : 0xC2)
    }
    else if (val is Number)
    {
      num := val as Number
      if (num.unit != null)
      {
        // Number with unit: ext type 0x04
        unitBytes := num.unit.toStr.toBuf
        buf.write(0xC7)  // ext8
        buf.write(8 + unitBytes.remaining)  // length
        buf.write(0x04)  // type code: Number+Unit
        buf.writeF8(num.toFloat)
        buf.writeBuf(unitBytes)
      }
      else
      {
        // Plain number
        f := num.toFloat
        if (f == f.toInt.toFloat && f.toInt >= 0 && f.toInt <= 127)
          buf.write(f.toInt)  // positive fixint
        else
        {
          buf.write(0xCB)  // float64
          buf.writeF8(f)
        }
      }
    }
    else if (val is Str)
    {
      writeStr(buf, val.toStr)
    }
    else if (val is Ref)
    {
      ref := val as Ref
      idBytes := ref.id.toBuf
      if (ref.dis != null)
      {
        disBytes := ref.dis.toBuf
        buf.write(0xC7)  // ext8
        buf.write(2 + idBytes.remaining + disBytes.remaining)
        buf.write(0x07)  // type: Ref+Dis
        buf.writeI2(idBytes.remaining)
        buf.writeBuf(idBytes)
        buf.writeBuf(disBytes)
      }
      else
      {
        buf.write(0xC7)  // ext8
        buf.write(idBytes.remaining)
        buf.write(0x06)  // type: Ref
        buf.writeBuf(idBytes)
      }
    }
    else if (val is DateTime)
    {
      dt := val as DateTime
      ms := dt.toJava  // millis since epoch
      tzBytes := dt.tz.name.toBuf
      buf.write(0xC7)  // ext8
      buf.write(8 + tzBytes.remaining)
      buf.write(0x0A)  // type: DateTime
      buf.writeI8(ms)
      buf.writeBuf(tzBytes)
    }
    else
    {
      // Fallback: encode as string
      writeStr(buf, val.toStr)
    }
  }

  ** Read MessagePack map header
  private Int readMapHeader(Buf buf)
  {
    b := buf.read
    if (b.and(0xF0) == 0x80)
      return b.and(0x0F)  // fixmap
    if (b == 0xDE)
      return buf.readU2  // map16
    if (b == 0xDF)
      return buf.readS4  // map32
    throw Err("Expected map header, got 0x${b.toHex}")
  }

  ** Read MessagePack string
  private Str readStr(Buf buf)
  {
    b := buf.read
    Int len
    if (b.and(0xE0) == 0xA0)
      len = b.and(0x1F)  // fixstr
    else if (b == 0xD9)
      len = buf.read  // str8
    else if (b == 0xDA)
      len = buf.readU2  // str16
    else
      throw Err("Expected str header, got 0x${b.toHex}")

    bytes := Buf(len)
    buf.readBuf(bytes, len)
    return bytes.flip.readAllStr
  }

  ** Read a MessagePack value and convert to Haystack
  private Obj? readValue(Buf buf)
  {
    b := buf.read

    // nil
    if (b == 0xC0) return null

    // bool
    if (b == 0xC2) return false
    if (b == 0xC3) return Marker.val  // true = Marker in roxWarp

    // positive fixint (0-127)
    if (b <= 0x7F) return Number(b)

    // float64
    if (b == 0xCB) return Number(buf.readF8)

    // fixstr
    if (b.and(0xE0) == 0xA0)
    {
      len := b.and(0x1F)
      bytes := Buf(len)
      buf.readBuf(bytes, len)
      return bytes.flip.readAllStr
    }

    // ext8 (typed value)
    if (b == 0xC7)
    {
      len := buf.read
      type := buf.read
      return readTypedValue(buf, type, len)
    }

    throw Err("Unsupported MessagePack type: 0x${b.toHex}")
  }

  ** Read a typed Haystack value from ext
  private Obj? readTypedValue(Buf buf, Int type, Int len)
  {
    switch (type)
    {
      case 0x01: return Marker.val  // Marker
      case 0x03:  // Number (no unit)
        return Number(buf.readF8)
      case 0x04:  // Number + Unit
        f := buf.readF8
        unitBytes := Buf(len - 8)
        buf.readBuf(unitBytes, len - 8)
        unit := Unit.fromStr(unitBytes.flip.readAllStr, false)
        return unit != null ? Number(f, unit) : Number(f)
      case 0x06:  // Ref
        idBytes := Buf(len)
        buf.readBuf(idBytes, len)
        return Ref(idBytes.flip.readAllStr)
      case 0x07:  // Ref + Dis
        idLen := buf.readU2
        idBytes := Buf(idLen)
        buf.readBuf(idBytes, idLen)
        disBytes := Buf(len - 2 - idLen)
        buf.readBuf(disBytes, len - 2 - idLen)
        return Ref(idBytes.flip.readAllStr, disBytes.flip.readAllStr)
      case 0x0A:  // DateTime
        ms := buf.readS8
        tzBytes := Buf(len - 8)
        buf.readBuf(tzBytes, len - 8)
        tz := TimeZone.fromStr(tzBytes.flip.readAllStr, false) ?: TimeZone.utc
        return DateTime.fromJava(ms, tz)
      default:
        // Skip unknown types
        buf.skip(len)
        return null
    }
  }
}
```

### 7.7 DeltaEngine.fan — Version Vector Engine

```fantom
using concurrent

**
** DeltaEngine - Tracks point state with version vectors for delta sync
**
class DeltaEngine
{
  private const Log log := Log.get("roxWarp.delta")

  ** Our node ID
  const Str nodeId

  ** Current version (monotonically increasing)
  private const AtomicInt version := AtomicInt(0)

  ** Local point state: channel(Int) -> VersionedPoint
  private const ConcurrentMap points := ConcurrentMap()

  ** Peer version vectors: peerId(Str) -> version(Int)
  private const ConcurrentMap peerVersions := ConcurrentMap()

  new make(Str nodeId)
  {
    this.nodeId = nodeId
  }

  **
  ** Record a local point change
  ** @return new version number
  **
  Int recordChange(Int channel, Float val, Str unit, Str status)
  {
    ver := version.incrementAndGet
    now := Duration.nowTicks / 1_000_000  // millis

    point := VersionedPoint
    {
      it.channel = channel
      it.val = val
      it.unit = unit
      it.status = status
      it.version = ver
      it.timestamp = now
      it.nodeId = this.nodeId
    }

    points.set(channel, point)
    return ver
  }

  **
  ** Compute delta for a peer (points changed since their last version)
  **
  VersionedPoint[] deltaForPeer(Str peerId)
  {
    peerVer := peerVersions.get(peerId) as Int ?: 0
    result := VersionedPoint[,]

    points.each |v, k|
    {
      point := v as VersionedPoint
      if (point != null && point.version > peerVer)
        result.add(point)
    }

    return result
  }

  **
  ** Acknowledge peer received up to version
  **
  Void ackPeer(Str peerId, Int ver)
  {
    peerVersions.set(peerId, ver)
  }

  **
  ** Apply remote delta from a peer (last-writer-wins merge)
  **
  Void applyRemoteDelta(Str peerId, VersionedPoint[] remotePoints)
  {
    maxVer := 0
    remotePoints.each |remote|
    {
      existing := points.get(remote.channel) as VersionedPoint
      shouldUpdate := existing == null || remote.timestamp > existing.timestamp

      if (shouldUpdate)
        points.set(remote.channel, remote)

      if (remote.version > maxVer)
        maxVer = remote.version
    }

    if (maxVer > 0)
      peerVersions.set(peerId, maxVer)
  }

  **
  ** Get current version
  **
  Int currentVersion() { version.val }

  **
  ** Get version vector (all known node versions)
  **
  Str:Int getVersionVector()
  {
    result := Str:Int[:]
    result[nodeId] = version.val
    peerVersions.each |v, k| { result[k] = v as Int ?: 0 }
    return result
  }

  **
  ** Get full state
  **
  VersionedPoint[] fullState()
  {
    result := VersionedPoint[,]
    points.each |v, k|
    {
      point := v as VersionedPoint
      if (point != null) result.add(point)
    }
    return result
  }

  **
  ** Get point count
  **
  Int pointCount() { points.size }
}
```

### 7.8 RoxWarpConnection.fan — WebSocket Client

```fantom
using concurrent
using web
using inet

**
** RoxWarpConnection - WebSocket client connection to a Sandstar roxWarp peer
**
class RoxWarpConnection
{
  private const Log log := Log.get("roxWarp.conn")

  ** Peer address (host:port)
  const Str address

  ** TLS certificate paths
  const Str caCertPath
  const Str clientCertPath
  const Str clientKeyPath

  ** Shared delta engine
  private DeltaEngine deltaEngine

  ** Binary Trio encoder/decoder
  private const BinaryTrioEncoder btrio := BinaryTrioEncoder()

  ** Connection state
  private const AtomicBool active := AtomicBool(false)

  ** Last heartbeat timestamp (nanos)
  private const AtomicInt lastHb := AtomicInt(0)

  ** Connection actor
  private Actor? connActor

  ** Peer node ID (set after handshake)
  private const AtomicRef peerNodeIdRef := AtomicRef(null)

  new make(Str address, Str caCertPath, Str clientCertPath,
           Str clientKeyPath, DeltaEngine deltaEngine)
  {
    this.address = address
    this.caCertPath = caCertPath
    this.clientCertPath = clientCertPath
    this.clientKeyPath = clientKeyPath
    this.deltaEngine = deltaEngine
  }

  ** Start the connection in a background actor
  Void start()
  {
    pool := ActorPool { name = "roxWarp-conn-$address"; maxThreads = 2 }
    connActor = Actor(pool) |msg| { return doConnect }
    connActor.send("connect")
  }

  ** Close the connection
  Void close()
  {
    active.val = false
    log.info("Closing roxWarp connection to $address")
  }

  ** Reconnect with backoff
  Void reconnect()
  {
    close
    // Exponential backoff with jitter
    delay := Duration((5_000 + Int.random(0..5_000)) * 1_000_000)
    connActor?.sendLater(delay, "connect")
  }

  ** Is the connection active?
  Bool isActive() { active.val }

  ** Last heartbeat timestamp
  Int lastHeartbeat() { lastHb.val }

  ** Point count from delta engine
  Int pointCount() { deltaEngine.pointCount }

  ** Query the peer with a Haystack filter
  Dict[] query(Str filter)
  {
    // TODO: Send warp:query, wait for warp:queryResult
    return Dict[,]
  }

  ** Peer node ID
  Str? peerNodeId() { peerNodeIdRef.val as Str }

  private Obj? doConnect()
  {
    try
    {
      // Create mTLS WebSocket connection
      uri := `wss://${address}/roxwarp`
      log.info("Connecting to roxWarp peer: $uri")

      // TODO: Implement mTLS WebSocket connection
      // For now, use standard WebSocket
      socket := WebSocket.openUri(uri)
      active.val = true
      lastHb.val = Duration.nowTicks

      // Send handshake
      hello := Etc.makeDict([
        "type": "warp:hello",
        "nodeId": deltaEngine.nodeId,
        // Version vector would go here
      ])
      socket.send(btrio.encode(hello))

      // Message loop
      while (active.val)
      {
        msg := socket.receive
        if (msg == null) break

        if (msg is Buf)
          handleBinaryMessage(msg as Buf)
        else if (msg is Str)
          handleTextMessage(msg as Str)
      }
    }
    catch (Err e)
    {
      log.err("roxWarp connection error ($address): $e.msg")
      if (active.val) reconnect
    }

    return null
  }

  private Void handleBinaryMessage(Buf data)
  {
    dict := btrio.decode(data)
    msgType := dict["type"] as Str

    switch (msgType)
    {
      case "warp:welcome":
        peerNodeIdRef.val = dict["nodeId"]
        log.info("roxWarp handshake complete with ${peerNodeId}")

      case "warp:heartbeat":
        lastHb.val = Duration.nowTicks
        log.debug("Heartbeat from ${dict["nodeId"]}")

      case "warp:delta":
        handleDelta(dict)

      case "warp:full":
        handleFullState(dict)

      default:
        log.debug("Unknown roxWarp message type: $msgType")
    }
  }

  private Void handleTextMessage(Str text)
  {
    // Trio text fallback for debugging
    log.debug("roxWarp text message: ${text.toStr.truncate(100)}")
  }

  private Void handleDelta(Dict dict)
  {
    peerId := dict["nodeId"] as Str ?: return
    // Parse points from dict and apply to delta engine
    log.debug("Applied delta from $peerId")
  }

  private Void handleFullState(Dict dict)
  {
    peerId := dict["nodeId"] as Str ?: return
    log.info("Received full state from $peerId")
  }
}
```

### 7.9 RoxWarpLib.fan — Axon Functions

```fantom
using axon
using haystack

**
** RoxWarpLib - Axon function library for roxWarp protocol
**
** Registered as skyarc.lib in build.fan index.
** All functions are accessible from SkySpark Axon expressions.
**
const class RoxWarpLib
{
  **
  ** Get roxWarp service status
  ** Usage: roxWarpStatus()
  **
  @Axon { admin = true }
  static Dict roxWarpStatus()
  {
    service := RoxWarpExt.service
    if (service == null)
      return Etc.makeDict(["running": false])
    return service.stats
  }

  **
  ** List connected cluster peers
  ** Usage: roxWarpPeers()
  **
  @Axon { admin = true }
  static Str[] roxWarpPeers()
  {
    service := RoxWarpExt.service
    if (service == null) return Str[,]
    return service.connectedPeers
  }

  **
  ** Connect to a roxWarp cluster node
  ** Usage: roxWarpConnect("192.168.1.100:7443", caCert, clientCert, clientKey)
  **
  @Axon { admin = true }
  static Dict roxWarpConnect(Str address, Str caCertPath,
                              Str clientCertPath, Str clientKeyPath)
  {
    service := RoxWarpExt.service
    if (service == null)
      throw Err("roxWarp service not running")
    service.connect(address, caCertPath, clientCertPath, clientKeyPath)
    return Etc.makeDict(["connected": Marker.val, "address": address])
  }

  **
  ** Disconnect from a roxWarp cluster node
  ** Usage: roxWarpDisconnect("192.168.1.100:7443")
  **
  @Axon { admin = true }
  static Dict roxWarpDisconnect(Str address)
  {
    service := RoxWarpExt.service
    if (service == null)
      throw Err("roxWarp service not running")
    service.disconnect(address)
    return Etc.makeDict(["disconnected": Marker.val, "address": address])
  }

  **
  ** Query across all connected cluster nodes
  ** Usage: roxWarpQuery("temp and sensor")
  **
  @Axon { admin = true }
  static Dict[] roxWarpQuery(Str filter)
  {
    service := RoxWarpExt.service
    if (service == null)
      throw Err("roxWarp service not running")
    return service.clusterQuery(filter)
  }

  **
  ** Get aggregated cluster state
  ** Usage: roxWarpClusterState()
  **
  @Axon { admin = true }
  static Dict roxWarpClusterState()
  {
    service := RoxWarpExt.service
    if (service == null)
      return Etc.makeDict(["error": "Service not running"])

    state := service.clusterState
    return Etc.makeDict([
      "nodeCount":  Number(state.nodeCount),
      "pointCount": Number(state.totalPointCount),
      "lastSync":   state.lastSyncTime
    ])
  }
}
```

### 7.10 VersionedPoint.fan — Data Model

```fantom
**
** VersionedPoint - A point value with version vector metadata
**
** Used for delta encoding in roxWarp gossip protocol.
** Immutable for thread-safe sharing via ConcurrentMap.
**
const class VersionedPoint
{
  ** Channel number (e.g., 1113)
  const Int channel := 0

  ** Current value
  const Float val := 0f

  ** Unit string (e.g., "degF", "mA")
  const Str unit := ""

  ** Point status: "ok", "fault", "disabled", "down", "alarm"
  const Str status := "ok"

  ** Version number (monotonically increasing per node)
  const Int version := 0

  ** Timestamp (Unix millis)
  const Int timestamp := 0

  ** Source node ID
  const Str nodeId := ""

  ** It-block constructor
  new make(|This| f) { f(this) }

  ** Convert to Dict for encoding
  Dict toDict()
  {
    tags := Str:Obj?[:]
    tags["ch"] = Number(channel)
    tags["val"] = unit.isEmpty ? Number(val) : Number(val, Unit.fromStr(unit, false))
    tags["st"] = status
    tags["v"] = Number(version)
    tags["ts"] = Number(timestamp)
    if (!nodeId.isEmpty)
      tags["nodeId"] = nodeId
    return Etc.makeDict(tags)
  }

  ** Create from Dict
  static VersionedPoint fromDict(Dict dict)
  {
    return VersionedPoint
    {
      it.channel = (dict["ch"] as Number)?.toInt ?: 0
      it.val = (dict["val"] as Number)?.toFloat ?: 0f
      it.unit = (dict["val"] as Number)?.unit?.toStr ?: ""
      it.status = dict["st"] as Str ?: "ok"
      it.version = (dict["v"] as Number)?.toInt ?: 0
      it.timestamp = (dict["ts"] as Number)?.toInt ?: 0
      it.nodeId = dict["nodeId"] as Str ?: ""
    }
  }
}
```

---

## 8. Integration with SkySpark/Haxall

### 8.1 Architecture

```
┌───────────────────────────────────────────────┐
│  SkySpark / Haxall                             │
│                                                │
│  ┌──────────────────┐  ┌───────────────────┐  │
│  │ bassgSoxWebSocket│  │ bassgRoxWarp      │  │
│  │ (legacy bridge)  │  │ (roxWarp client)  │  │
│  │ JSON ↔ SOX/UDP   │  │ Binary Trio ↔ WSS │  │
│  └────────┬─────────┘  └────────┬──────────┘  │
│           │                      │             │
│     ┌─────▼──────────────────────▼──────┐     │
│     │     Folio Database                │     │
│     │     (points, histories)           │     │
│     └───────────────────────────────────┘     │
└───────────────────────────────────────────────┘
         │                      │
    SOX/UDP :1876          roxWarp/WSS :7443
         │                      │
    ┌────▼─────┐           ┌────▼─────┐
    │ Sandstar │           │ Sandstar │
    │ (legacy) │           │ (Rust)   │
    └──────────┘           └──────────┘
```

### 8.2 Axon Usage Examples

```axon
// Check roxWarp status
roxWarpStatus()

// Connect to a Sandstar cluster
roxWarpConnect("192.168.1.100:7443",
  "/etc/certs/cluster-ca.pem",
  "/etc/certs/skyspark.pem",
  "/etc/certs/skyspark.key")

// Query temperature sensors across cluster
roxWarpQuery("temp and sensor")

// Get cluster state
roxWarpClusterState()

// List connected peers
roxWarpPeers()

// Disconnect
roxWarpDisconnect("192.168.1.100:7443")
```

### 8.3 Haxall Compatibility

The `bassgRoxWarp` pod uses `hx` library APIs (HxExt, HxRuntime) which are compatible with both SkySpark and standalone Haxall. The pod:

- Uses `@ExtMeta` annotation for SkySpark extension discovery
- Registers `@Axon` functions for both platforms
- Depends on `haystack`, `folio`, `axon`, `hx` (not SkySpark-specific APIs)
- Can run in both SkySpark (commercial) and Haxall (open-source) runtimes

---

## 9. Dependencies

### 9.1 Rust Crates (Sandstar device side)

| Crate | Version | Purpose |
|-------|---------|---------|
| `rmp-serde` | 1.3+ | MessagePack serialization for binary Trio |
| `rmp` | 0.8+ | Low-level MessagePack |
| `dashmap` | 6.0+ | Concurrent HashMap for point state |
| `tokio-tungstenite` | 0.24+ | WebSocket client/server |
| `rustls` | 0.23+ | mTLS |
| `tokio-rustls` | 0.26+ | Async TLS |
| `rcgen` | 0.13+ | Certificate generation |

### 9.2 Fantom Dependencies (SkySpark/Haxall side)

| Pod | Version | Purpose |
|-----|---------|---------|
| `sys` | 1.0+ | Core Fantom |
| `concurrent` | 1.0+ | AtomicRef, ConcurrentMap, Actor |
| `inet` | 1.0+ | Networking |
| `web` | 1.0+ | WebSocket client |
| `crypto` | 1.0+ | TLS/certificate handling |
| `haystack` | 3.0+ | Dict, Number, Ref, etc. |
| `folio` | 3.0+ | Database access |
| `axon` | 3.0+ | @Axon function registration |
| `hx` | 3.0+ | HxExt, HxRuntime |
| `bassgCommon` | 1.0+ | Shared utilities |

### 9.3 Binary Size Impact (Rust)

| Component | Size |
|-----------|------|
| rmp-serde | ~40KB |
| rmp | ~20KB |
| Binary Trio encoder (custom) | ~5KB |
| Delta engine (custom) | ~3KB |
| **Total roxWarp addition** | **~68KB** |

---

## 10. Testing Strategy

### 10.1 Rust Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binary_trio_roundtrip() {
        let mut dict = Dict::new();
        dict.insert("type".into(), Value::Str("cov".into()));
        dict.insert("compId".into(), Value::Number(Number::from(5)));
        dict.insert("value".into(), Value::Number(Number::make(73.2, "degF")));
        dict.insert("point".into(), Value::Marker(Marker));

        let encoded = binary_trio_encode(&dict);
        let decoded = binary_trio_decode(&encoded).unwrap();

        assert_eq!(decoded.get_str("type"), Some("cov"));
        assert_eq!(decoded.get_number("compId"), Some(5.0));
        assert!(decoded.has("point"));  // Marker preserved
    }

    #[test]
    fn test_delta_computation() {
        let engine = DeltaEngine::new("node-a".into());

        // Record 3 changes
        engine.record_change(1113, 72.5, "degF", "ok");
        engine.record_change(1206, 4.2, "mA", "ok");
        engine.record_change(1113, 73.0, "degF", "ok");  // update

        // Peer knows version 1
        engine.ack_peer("node-b", 1);

        // Delta should include versions 2 and 3
        let (ver, deltas) = engine.delta_for_peer("node-b");
        assert_eq!(ver, 3);
        assert_eq!(deltas.len(), 2);  // 1206 (v2) and 1113 (v3)
    }

    #[test]
    fn test_binary_trio_size() {
        let mut dict = Dict::new();
        dict.insert("type".into(), Value::Str("cov".into()));
        dict.insert("compId".into(), Value::Number(Number::from(5)));
        dict.insert("slotId".into(), Value::Number(Number::from(0)));
        dict.insert("value".into(), Value::Number(Number::make(73.2, "degF")));
        dict.insert("ts".into(), Value::Number(Number::from(1706000000.0)));

        let binary = binary_trio_encode(&dict);
        let trio_text = trio_encode(&dict);

        // Binary should be significantly smaller than text
        assert!(binary.len() < trio_text.len());
        println!("Binary: {} bytes, Text: {} bytes, Ratio: {:.0}%",
                 binary.len(), trio_text.len(),
                 (binary.len() as f64 / trio_text.len() as f64) * 100.0);
    }
}
```

### 10.2 Fantom Tests

```fantom
using haystack

class BinaryTrioTest : Test
{
  Void testEncodeDecodeRoundtrip()
  {
    encoder := BinaryTrioEncoder()

    dict := Etc.makeDict([
      "type": "warp:heartbeat",
      "nodeId": "sandstar-a-001",
      "ts": Number(1706000000),
      "cpuPercent": Number(23.5f),
      "point": Marker.val
    ])

    encoded := encoder.encode(dict)
    decoded := encoder.decode(encoded)

    verifyEq(decoded["type"], "warp:heartbeat")
    verifyEq(decoded["nodeId"], "sandstar-a-001")
    verify(decoded["point"] is Marker)
  }

  Void testDeltaEngine()
  {
    engine := DeltaEngine("test-node")

    engine.recordChange(1113, 72.5f, "degF", "ok")
    engine.recordChange(1206, 4.2f, "mA", "ok")

    // Peer at version 0 should get both
    delta := engine.deltaForPeer("peer-1")
    verifyEq(delta.size, 2)

    // Ack version 1, should only get version 2
    engine.ackPeer("peer-1", 1)
    delta = engine.deltaForPeer("peer-1")
    verifyEq(delta.size, 1)
    verifyEq(delta[0].channel, 1206)
  }

  Void testVersionedPointRoundtrip()
  {
    point := VersionedPoint
    {
      it.channel = 1113
      it.val = 72.5f
      it.unit = "degF"
      it.status = "ok"
      it.version = 42
      it.timestamp = 1706000000
      it.nodeId = "sandstar-a-001"
    }

    dict := point.toDict
    restored := VersionedPoint.fromDict(dict)

    verifyEq(restored.channel, 1113)
    verifyEq(restored.val, 72.5f)
    verifyEq(restored.unit, "degF")
    verifyEq(restored.version, 42)
  }
}
```

---

## Appendix A: Binary Trio vs Other Encodings

| Encoding | COV Size | Parse Speed | Haystack Types | Self-Describing |
|----------|----------|-------------|----------------|-----------------|
| **Binary Trio (roxWarp)** | ~20 bytes | Fast (MessagePack) | Full fidelity | Yes (tag names in map) |
| SOX binary | ~9 bytes | Fast | Sedona types only | No (requires schema) |
| Trio text (ROX) | ~62 bytes | Moderate | Full fidelity | Yes |
| JSON (Haystack 4) | ~180 bytes | Moderate | Full fidelity | Yes |
| JSON (Hayson) | ~110 bytes | Moderate | Full fidelity | Yes |
| CBOR | ~25 bytes | Fast | Requires mapping | Yes |
| Protobuf | ~15 bytes | Very fast | Requires .proto | No |

**Design choice rationale:** Binary Trio uses MessagePack for the container format but preserves Haystack type semantics via extension type codes. This means:
- Any Haystack value can be encoded without loss
- Standard MessagePack libraries can parse the structure (tags + raw values)
- Haystack-specific type codes provide exact type reconstruction
- No pre-shared schema needed (unlike Protobuf)
- Smaller than JSON while maintaining self-description (unlike SOX binary)

## Appendix B: Relationship to Prior Documents

| Document | Relationship |
|----------|-------------|
| [04 REST API / Axum](04_REST_API_AXUM_MIGRATION.md) | roxWarp cluster port (:7443) is separate from Haystack API (:8085) |
| [06 Sedona FFI Strategy](06_SEDONA_FFI_STRATEGY.md) | roxWarp state includes Sedona component values via FFI bridge |
| [09 Dependency Mapping](09_DEPENDENCY_MAPPING.md) | rmp-serde added to dependency graph |
| [11 Migration Roadmap](11_MIGRATION_ROADMAP.md) | roxWarp is Phase 2-3 of the ROX migration |
| [14 Scalability Limits](14_SEDONA_VM_SCALABILITY_LIMITS.md) | Delta encoding addresses COV bandwidth bottleneck |
| [15 ROX Protocol](15_SOX_WEBSOCKET_MIGRATION.md) | ROX (Trio-over-WS) is the companion client-facing protocol |
