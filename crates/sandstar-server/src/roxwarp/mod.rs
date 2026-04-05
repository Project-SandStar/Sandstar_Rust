//! roxWarp: Binary Trio diff gossip protocol for Sandstar device clusters.
//!
//! roxWarp enables efficient change-of-value (COV) replication across a mesh
//! of embedded IoT devices using a Scuttlebutt-style gossip protocol over
//! WebSocket connections.
//!
//! # Architecture
//!
//! - `binary_trio` — Binary Trio encoder/decoder (Haystack values as MessagePack)
//! - `delta` — Synchronous delta engine with version vectors and LWW merge
//! - `protocol` — Full roxWarp message types with JSON + MessagePack serialization
//! - `cluster` — Async cluster manager, channel feed loop, peer state tracking
//! - `handler` — Incoming WebSocket handler (server side)
//! - `peer` — Outbound WebSocket connections (client side)
//!
//! # Protocol
//!
//! Messages are JSON-encoded for Phase 1 with MessagePack (binary Trio) support
//! via `protocol::WarpMessage::to_msgpack()`. The gossip protocol uses version
//! vectors for delta sync:
//!
//! 1. **Handshake** (`warp:hello` / `warp:welcome`) — exchange node IDs + versions + capabilities
//! 2. **Delta sync** (`warp:delta`) — only changed points since peer's last version
//! 3. **Heartbeat** (`warp:heartbeat`) — keep-alive with load metrics
//! 4. **Anti-entropy** (`warp:versions` / `warp:deltaReq`) — periodic full version exchange
//! 5. **Membership** (`warp:join` / `warp:leave`) — cluster membership changes

pub mod binary_trio;
pub mod cluster;
pub mod delta;
pub mod handler;
pub mod mtls;
pub mod peer;
pub mod protocol;
pub mod string_table;

pub use binary_trio::{TrioDict, TrioValue};
pub use cluster::{ClusterConfig, ClusterManager, ClusterStatus, PeerConfig, PeerState, PeerStatus};
pub use handler::{roxwarp_upgrade, RoxWarpState};
pub use protocol::{LoadMetrics, QueryPoint, WarpMessage};
pub use string_table::StringTable;

// ── Error type ───────────────────────────────────────

/// Errors from roxWarp encoding, decoding, and protocol operations.
#[derive(Debug)]
pub enum RoxWarpError {
    /// MessagePack or JSON decode failure.
    Decode(String),
    /// MessagePack or JSON encode failure.
    Encode(String),
    /// WebSocket or network connection failure.
    Connection(String),
    /// Protocol violation (unexpected message, bad handshake, etc.).
    Protocol(String),
}

impl std::fmt::Display for RoxWarpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "roxWarp decode error: {e}"),
            Self::Encode(e) => write!(f, "roxWarp encode error: {e}"),
            Self::Connection(e) => write!(f, "roxWarp connection error: {e}"),
            Self::Protocol(e) => write!(f, "roxWarp protocol error: {e}"),
        }
    }
}

impl std::error::Error for RoxWarpError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display() {
        let e = RoxWarpError::Decode("bad bytes".into());
        assert_eq!(e.to_string(), "roxWarp decode error: bad bytes");

        let e = RoxWarpError::Encode("buffer full".into());
        assert_eq!(e.to_string(), "roxWarp encode error: buffer full");

        let e = RoxWarpError::Connection("refused".into());
        assert_eq!(e.to_string(), "roxWarp connection error: refused");

        let e = RoxWarpError::Protocol("no hello".into());
        assert_eq!(e.to_string(), "roxWarp protocol error: no hello");
    }
}
