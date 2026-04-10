//! Incoming WebSocket handler for roxWarp peer-to-peer connections.
//!
//! Handles the server side of the roxWarp gossip protocol when a remote
//! peer connects to `/roxwarp` on this node's HTTP server.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

use super::cluster::{ClusterConfig, DeltaEngine, VersionedPoint};

// ── Shared State ──────────────────────────────────────

/// Axum state for the roxWarp WebSocket endpoint.
#[derive(Clone)]
pub struct RoxWarpState {
    pub delta_engine: Arc<DeltaEngine>,
    pub config: ClusterConfig,
    /// Optional reference to the SOX component tree for name table sharing.
    pub sox_tree: Option<crate::sox::SharedComponentTree>,
}

// ── Wire Messages (JSON for Phase 1) ─────────────────

/// Messages exchanged over the roxWarp WebSocket connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum WarpMessage {
    /// Initial handshake from connecting peer.
    #[serde(rename = "warp:hello")]
    Hello {
        #[serde(rename = "nodeId")]
        node_id: String,
        versions: HashMap<String, u64>,
    },
    /// Handshake response from accepting peer.
    #[serde(rename = "warp:welcome")]
    Welcome {
        #[serde(rename = "nodeId")]
        node_id: String,
        versions: HashMap<String, u64>,
        /// Component name table entries from the SOX name intern table (optional).
        #[serde(rename = "nameTable", default, skip_serializing_if = "Vec::is_empty")]
        name_table: Vec<String>,
    },
    /// Keep-alive with optional load metrics.
    #[serde(rename = "warp:heartbeat")]
    Heartbeat {
        #[serde(rename = "nodeId")]
        node_id: String,
        #[serde(rename = "ts")]
        timestamp: i64,
    },
    /// Incremental state changes.
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
    /// Request deltas from a specific version.
    #[serde(rename = "warp:deltaReq")]
    DeltaReq {
        #[serde(rename = "nodeId")]
        node_id: String,
        #[serde(rename = "wantFrom")]
        want_from: HashMap<String, u64>,
    },
    /// Full state dump.
    #[serde(rename = "warp:full")]
    Full {
        #[serde(rename = "nodeId")]
        node_id: String,
        version: u64,
        points: Vec<VersionedPoint>,
    },
    /// Request full state.
    #[serde(rename = "warp:fullReq")]
    FullReq {
        #[serde(rename = "nodeId")]
        node_id: String,
    },
    /// Periodic version vector exchange (anti-entropy).
    #[serde(rename = "warp:versions")]
    Versions {
        #[serde(rename = "nodeId")]
        node_id: String,
        versions: HashMap<String, u64>,
    },
    /// Acknowledgment.
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
        results: Vec<super::protocol::QueryPoint>,
    },
}

// ── WebSocket Upgrade Handler ─────────────────────────

/// Axum handler that upgrades an HTTP request to a roxWarp WebSocket connection.
pub async fn roxwarp_upgrade(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<RoxWarpState>,
) -> Response {
    let debug = params.get("debug").map(|v| v == "trio").unwrap_or(false);
    ws.on_upgrade(move |socket| {
        handle_roxwarp_connection(
            socket,
            state.delta_engine,
            state.config,
            state.sox_tree,
            debug,
        )
    })
}

// ── Connection Handler ────────────────────────────────

/// Handle an incoming roxWarp WebSocket connection from a peer.
///
/// Protocol flow:
/// 1. Receive `warp:hello` from peer
/// 2. Send `warp:welcome` with our version vector
/// 3. Exchange deltas based on version vector comparison
/// 4. Enter active gossip loop (heartbeat + delta push + anti-entropy)
async fn handle_roxwarp_connection(
    ws: WebSocket,
    delta_engine: Arc<DeltaEngine>,
    config: ClusterConfig,
    sox_tree: Option<crate::sox::SharedComponentTree>,
    debug_mode: bool,
) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    info!("roxWarp: incoming connection");

    // Phase 1: Wait for hello (binary or text)
    let peer_node_id = match wait_for_hello(&mut ws_rx).await {
        Some((node_id, _peer_versions)) => {
            info!(peer = %node_id, "roxWarp: received hello");
            node_id
        }
        None => {
            warn!("roxWarp: no hello received, closing");
            return;
        }
    };

    // Phase 2: Send welcome with our version vector + name table
    let our_versions = delta_engine.get_version_vector().await;
    let name_table_entries: Vec<String> = sox_tree
        .as_ref()
        .map(|tree| {
            let t = tree.read().unwrap();
            t.name_table
                .all_names()
                .into_iter()
                .map(|(_, n)| n)
                .collect()
        })
        .unwrap_or_default();
    let welcome = WarpMessage::Welcome {
        node_id: config.node_id.clone(),
        versions: our_versions,
        name_table: name_table_entries,
    };
    if send_message(&mut ws_tx, &welcome, debug_mode)
        .await
        .is_err()
    {
        return;
    }

    // Phase 3: Send initial delta to peer (full state if peer is new)
    let (current_version, deltas) = delta_engine.delta_for_peer(&peer_node_id).await;
    if !deltas.is_empty() {
        let delta_msg = WarpMessage::Delta {
            node_id: config.node_id.clone(),
            from_version: 0,
            to_version: current_version,
            points: deltas,
        };
        if send_message(&mut ws_tx, &delta_msg, debug_mode)
            .await
            .is_err()
        {
            return;
        }
    }

    // Phase 4: Active gossip loop
    let mut heartbeat_timer =
        tokio::time::interval(Duration::from_secs(config.heartbeat_interval_secs));
    heartbeat_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut anti_entropy_timer =
        tokio::time::interval(Duration::from_secs(config.anti_entropy_interval_secs));
    anti_entropy_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut last_sent_version = current_version;
    let mut last_activity = Instant::now();
    let timeout = Duration::from_secs(120);

    loop {
        tokio::select! {
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        last_activity = Instant::now();
                        if let Err(e) = handle_incoming_binary(
                            &data,
                            &delta_engine,
                            &config,
                            &peer_node_id,
                            &mut ws_tx,
                            debug_mode,
                        ).await {
                            warn!(peer = %peer_node_id, error = %e, "roxWarp: binary message error");
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        last_activity = Instant::now();
                        if let Err(e) = handle_incoming_text(
                            &text,
                            &delta_engine,
                            &config,
                            &peer_node_id,
                            &mut ws_tx,
                            debug_mode,
                        ).await {
                            warn!(peer = %peer_node_id, error = %e, "roxWarp: text message error");
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        last_activity = Instant::now();
                        if ws_tx.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        info!(peer = %peer_node_id, "roxWarp: peer disconnected");
                        break;
                    }
                    _ => {
                        last_activity = Instant::now();
                    }
                }
            }

            _ = heartbeat_timer.tick() => {
                // Check timeout
                if last_activity.elapsed() > timeout {
                    info!(peer = %peer_node_id, "roxWarp: peer timed out");
                    break;
                }

                // Send heartbeat
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let hb = WarpMessage::Heartbeat {
                    node_id: config.node_id.clone(),
                    timestamp: now_ms,
                };
                if send_message(&mut ws_tx, &hb, debug_mode).await.is_err() {
                    break;
                }

                // Push any new deltas since last send
                let current = delta_engine.current_version();
                if current > last_sent_version {
                    let deltas = delta_engine.delta_since(last_sent_version).await;
                    if !deltas.is_empty() {
                        let delta_msg = WarpMessage::Delta {
                            node_id: config.node_id.clone(),
                            from_version: last_sent_version,
                            to_version: current,
                            points: deltas,
                        };
                        if send_message(&mut ws_tx, &delta_msg, debug_mode).await.is_err() {
                            break;
                        }
                        last_sent_version = current;
                    }
                }
            }

            _ = anti_entropy_timer.tick() => {
                // Send our full version vector for anti-entropy
                let versions = delta_engine.get_version_vector().await;
                let msg = WarpMessage::Versions {
                    node_id: config.node_id.clone(),
                    versions,
                };
                if send_message(&mut ws_tx, &msg, debug_mode).await.is_err() {
                    break;
                }
            }
        }
    }

    info!(peer = %peer_node_id, "roxWarp: connection closed");
}

/// Wait for a `warp:hello` message from the connecting peer.
///
/// Accepts both binary (MessagePack) and text (JSON) frames.
async fn wait_for_hello(
    ws_rx: &mut futures_util::stream::SplitStream<WebSocket>,
) -> Option<(String, HashMap<String, u64>)> {
    // Give the peer 10 seconds to send hello
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(10), ws_rx.next()).await {
            Ok(Some(Ok(Message::Binary(data)))) => {
                if let Ok(WarpMessage::Hello {
                    node_id, versions, ..
                }) = rmp_serde::from_slice::<WarpMessage>(&data)
                {
                    return Some((node_id, versions));
                }
            }
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(WarpMessage::Hello {
                    node_id, versions, ..
                }) = serde_json::from_str::<WarpMessage>(&text)
                {
                    return Some((node_id, versions));
                }
            }
            _ => return None,
        }
    }
    None
}

/// Handle an incoming binary (MessagePack) message from a connected peer.
async fn handle_incoming_binary(
    data: &[u8],
    delta_engine: &Arc<DeltaEngine>,
    config: &ClusterConfig,
    peer_node_id: &str,
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    debug_mode: bool,
) -> Result<(), String> {
    let msg: WarpMessage =
        rmp_serde::from_slice(data).map_err(|e| format!("invalid msgpack message: {e}"))?;
    process_warp_message(msg, delta_engine, config, peer_node_id, ws_tx, debug_mode).await
}

/// Handle an incoming text (JSON) message from a connected peer.
async fn handle_incoming_text(
    text: &str,
    delta_engine: &Arc<DeltaEngine>,
    config: &ClusterConfig,
    peer_node_id: &str,
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    debug_mode: bool,
) -> Result<(), String> {
    let msg: WarpMessage =
        serde_json::from_str(text).map_err(|e| format!("invalid json message: {e}"))?;
    process_warp_message(msg, delta_engine, config, peer_node_id, ws_tx, debug_mode).await
}

/// Process a decoded WarpMessage from either binary or text frame.
async fn process_warp_message(
    msg: WarpMessage,
    delta_engine: &Arc<DeltaEngine>,
    config: &ClusterConfig,
    peer_node_id: &str,
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    debug_mode: bool,
) -> Result<(), String> {
    match msg {
        WarpMessage::Delta {
            node_id,
            to_version,
            points,
            ..
        } => {
            debug!(
                peer = %node_id,
                version = to_version,
                count = points.len(),
                "roxWarp: received delta"
            );
            delta_engine.apply_remote_delta(&node_id, points).await;

            // Ack the delta
            let ack = WarpMessage::Ack {
                node_id: config.node_id.clone(),
                version: to_version,
            };
            send_message(ws_tx, &ack, debug_mode)
                .await
                .map_err(|_| "send failed".to_string())?;
        }

        WarpMessage::Heartbeat { node_id, .. } => {
            debug!(peer = %node_id, "roxWarp: received heartbeat");
        }

        WarpMessage::Versions {
            node_id, versions, ..
        } => {
            debug!(peer = %node_id, "roxWarp: received version vector");
            // Check if peer is behind on our data
            let our_version = delta_engine.current_version();
            let peer_has = versions.get(&config.node_id).copied().unwrap_or(0);
            if peer_has < our_version {
                // Send deltas for what the peer is missing
                let deltas = delta_engine.delta_since(peer_has).await;
                if !deltas.is_empty() {
                    let delta_msg = WarpMessage::Delta {
                        node_id: config.node_id.clone(),
                        from_version: peer_has,
                        to_version: our_version,
                        points: deltas,
                    };
                    send_message(ws_tx, &delta_msg, debug_mode)
                        .await
                        .map_err(|_| "send failed".to_string())?;
                }
            }
        }

        WarpMessage::DeltaReq { node_id, want_from } => {
            debug!(peer = %node_id, "roxWarp: received delta request");
            let since = want_from.get(&config.node_id).copied().unwrap_or(0);
            let deltas = delta_engine.delta_since(since).await;
            let current = delta_engine.current_version();
            let delta_msg = WarpMessage::Delta {
                node_id: config.node_id.clone(),
                from_version: since,
                to_version: current,
                points: deltas,
            };
            send_message(ws_tx, &delta_msg, debug_mode)
                .await
                .map_err(|_| "send failed".to_string())?;
        }

        WarpMessage::FullReq { node_id } => {
            debug!(peer = %node_id, "roxWarp: received full state request");
            let (version, points) = delta_engine.full_state().await;
            let full_msg = WarpMessage::Full {
                node_id: config.node_id.clone(),
                version,
                points,
            };
            send_message(ws_tx, &full_msg, debug_mode)
                .await
                .map_err(|_| "send failed".to_string())?;
        }

        WarpMessage::Ack { node_id, version } => {
            debug!(peer = %node_id, version = version, "roxWarp: received ack");
            delta_engine.ack_peer(&node_id, version).await;
        }

        WarpMessage::Query {
            node_id,
            query_id,
            filter,
            limit,
        } => {
            debug!(
                peer = %node_id,
                query_id = %query_id,
                filter = %filter,
                "roxWarp: received query"
            );
            let results =
                evaluate_query_against_delta_engine(delta_engine, &config.node_id, &filter, limit)
                    .await;
            let result_msg = WarpMessage::QueryResult {
                node_id: config.node_id.clone(),
                query_id,
                results,
            };
            send_message(ws_tx, &result_msg, debug_mode)
                .await
                .map_err(|_| "send query result failed".to_string())?;
        }

        WarpMessage::QueryResult {
            node_id, query_id, ..
        } => {
            debug!(
                peer = %node_id,
                query_id = %query_id,
                "roxWarp: received query result (handled by caller)"
            );
            // Query results are typically collected by the distributed_query
            // coordinator — here we just log them.
        }

        _ => {
            debug!(peer = %peer_node_id, "roxWarp: ignoring unexpected message type");
        }
    }

    Ok(())
}

// ── Distributed Query Evaluation ─────────────────────

/// Evaluate a filter query against the DeltaEngine's local points.
///
/// Supports basic filter operations:
/// - `"point"` — matches all points
/// - `"channel==N"` — exact channel match
/// - `"channel > N"`, `"channel < N"` — channel range
/// - Substring match on unit or status
/// - `"X and Y"` — conjunction of two terms
/// - `"X or Y"` — disjunction of two terms
async fn evaluate_query_against_delta_engine(
    delta_engine: &Arc<DeltaEngine>,
    local_node_id: &str,
    filter: &str,
    limit: Option<u32>,
) -> Vec<super::protocol::QueryPoint> {
    let (_, all_points) = delta_engine.full_state().await;
    let limit = limit.unwrap_or(u32::MAX) as usize;

    all_points
        .iter()
        .filter(|p| evaluate_point_filter(filter, p))
        .take(limit)
        .map(|p| super::protocol::QueryPoint {
            channel: p.channel,
            value: p.value,
            unit: p.unit.clone(),
            status: p.status.clone(),
            node_id: local_node_id.to_string(),
        })
        .collect()
}

/// Evaluate a filter expression against a single VersionedPoint.
///
/// This is a simplified filter evaluator for distributed queries.
/// It handles the most common patterns without requiring the full
/// Haystack filter parser (which operates on `ChannelInfo`).
pub(crate) fn evaluate_point_filter(filter: &str, point: &VersionedPoint) -> bool {
    let f = filter.trim();

    // Handle "and" conjunction
    if let Some(pos) = find_keyword(f, " and ") {
        let left = &f[..pos];
        let right = &f[pos + 5..];
        return evaluate_point_filter(left, point) && evaluate_point_filter(right, point);
    }

    // Handle "or" disjunction
    if let Some(pos) = find_keyword(f, " or ") {
        let left = &f[..pos];
        let right = &f[pos + 4..];
        return evaluate_point_filter(left, point) || evaluate_point_filter(right, point);
    }

    let f_lower = f.to_lowercase();

    // "point" matches everything
    if f_lower == "point" {
        return true;
    }

    // Comparison operators: channel==N, channel>N, channel<N, channel>=N, channel<=N
    if f.contains("==") {
        let parts: Vec<&str> = f.splitn(2, "==").collect();
        let tag = parts[0].trim().to_lowercase();
        let val = parts[1].trim();
        return match tag.as_str() {
            "channel" | "id" => val
                .parse::<u32>()
                .map(|n| point.channel == n)
                .unwrap_or(false),
            "unit" => point.unit.eq_ignore_ascii_case(val.trim_matches('"')),
            "status" => point.status.eq_ignore_ascii_case(val.trim_matches('"')),
            _ => false,
        };
    }

    if f.contains(">=") {
        let parts: Vec<&str> = f.splitn(2, ">=").collect();
        let tag = parts[0].trim().to_lowercase();
        let val = parts[1].trim();
        if tag == "channel" || tag == "id" {
            return val
                .parse::<u32>()
                .map(|n| point.channel >= n)
                .unwrap_or(false);
        }
        if tag == "value" || tag == "cur" {
            return val
                .parse::<f64>()
                .map(|n| point.value >= n)
                .unwrap_or(false);
        }
    }

    if f.contains("<=") {
        let parts: Vec<&str> = f.splitn(2, "<=").collect();
        let tag = parts[0].trim().to_lowercase();
        let val = parts[1].trim();
        if tag == "channel" || tag == "id" {
            return val
                .parse::<u32>()
                .map(|n| point.channel <= n)
                .unwrap_or(false);
        }
        if tag == "value" || tag == "cur" {
            return val
                .parse::<f64>()
                .map(|n| point.value <= n)
                .unwrap_or(false);
        }
    }

    // Single > or < (check after >= and <=)
    if f.contains('>') && !f.contains(">=") {
        let parts: Vec<&str> = f.splitn(2, '>').collect();
        let tag = parts[0].trim().to_lowercase();
        let val = parts[1].trim();
        if tag == "channel" || tag == "id" {
            return val
                .parse::<u32>()
                .map(|n| point.channel > n)
                .unwrap_or(false);
        }
        if tag == "value" || tag == "cur" {
            return val.parse::<f64>().map(|n| point.value > n).unwrap_or(false);
        }
    }

    if f.contains('<') && !f.contains("<=") {
        let parts: Vec<&str> = f.splitn(2, '<').collect();
        let tag = parts[0].trim().to_lowercase();
        let val = parts[1].trim();
        if tag == "channel" || tag == "id" {
            return val
                .parse::<u32>()
                .map(|n| point.channel < n)
                .unwrap_or(false);
        }
        if tag == "value" || tag == "cur" {
            return val.parse::<f64>().map(|n| point.value < n).unwrap_or(false);
        }
    }

    // Substring match on unit or status (fallback)
    point.unit.to_lowercase().contains(&f_lower) || point.status.to_lowercase().contains(&f_lower)
}

/// Find the position of a keyword in a filter string, avoiding
/// matches inside quoted strings.
fn find_keyword(s: &str, keyword: &str) -> Option<usize> {
    let lower = s.to_lowercase();
    let kw_lower = keyword.to_lowercase();
    // Simple: find the keyword outside of quotes
    let mut in_quotes = false;
    let chars: Vec<char> = lower.chars().collect();
    let kw_chars: Vec<char> = kw_lower.chars().collect();
    for i in 0..chars.len() {
        if chars[i] == '"' {
            in_quotes = !in_quotes;
            continue;
        }
        if !in_quotes
            && i + kw_chars.len() <= chars.len()
            && chars[i..i + kw_chars.len()] == kw_chars[..]
        {
            return Some(i);
        }
    }
    None
}

/// Send a WarpMessage over WebSocket.
///
/// Uses binary (MessagePack) frames by default for efficiency.
/// In debug mode (`?debug=trio`), uses text (JSON) frames for human inspection.
async fn send_message(
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    msg: &WarpMessage,
    debug_mode: bool,
) -> Result<(), ()> {
    if debug_mode {
        match serde_json::to_string(msg) {
            Ok(json) => ws_tx.send(Message::Text(json.into())).await.map_err(|_| ()),
            Err(_) => Err(()),
        }
    } else {
        match rmp_serde::to_vec(msg) {
            Ok(bytes) => ws_tx
                .send(Message::Binary(bytes.into()))
                .await
                .map_err(|_| ()),
            Err(_) => Err(()),
        }
    }
}

// ── Tests ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warp_hello_serialize() {
        let msg = WarpMessage::Hello {
            node_id: "test-node".to_string(),
            versions: HashMap::from([("test-node".to_string(), 42)]),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"warp:hello\""));
        assert!(json.contains("\"nodeId\":\"test-node\""));
        assert!(json.contains("\"versions\""));
    }

    #[test]
    fn warp_welcome_serialize() {
        let msg = WarpMessage::Welcome {
            node_id: "responder".to_string(),
            versions: HashMap::new(),
            name_table: vec![],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"warp:welcome\""));
    }

    #[test]
    fn warp_heartbeat_serialize() {
        let msg = WarpMessage::Heartbeat {
            node_id: "node-a".to_string(),
            timestamp: 1706000000_000,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"warp:heartbeat\""));
        assert!(json.contains("\"ts\":1706000000000"));
    }

    #[test]
    fn warp_delta_serialize() {
        let msg = WarpMessage::Delta {
            node_id: "node-a".to_string(),
            from_version: 10,
            to_version: 15,
            points: vec![VersionedPoint {
                channel: 1113,
                value: 73.2,
                unit: "degF".to_string(),
                status: "ok".to_string(),
                version: 15,
                timestamp: 1706000000_000,
            }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"warp:delta\""));
        assert!(json.contains("\"fromVersion\":10"));
        assert!(json.contains("\"toVersion\":15"));
        assert!(json.contains("\"points\""));
    }

    #[test]
    fn warp_message_roundtrip() {
        let original = WarpMessage::Delta {
            node_id: "node-a".to_string(),
            from_version: 5,
            to_version: 10,
            points: vec![
                VersionedPoint {
                    channel: 1113,
                    value: 72.5,
                    unit: "degF".to_string(),
                    status: "ok".to_string(),
                    version: 8,
                    timestamp: 1706000100_000,
                },
                VersionedPoint {
                    channel: 1206,
                    value: 4.2,
                    unit: "mA".to_string(),
                    status: "ok".to_string(),
                    version: 10,
                    timestamp: 1706000200_000,
                },
            ],
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: WarpMessage = serde_json::from_str(&json).unwrap();

        if let WarpMessage::Delta {
            node_id,
            from_version,
            to_version,
            points,
        } = decoded
        {
            assert_eq!(node_id, "node-a");
            assert_eq!(from_version, 5);
            assert_eq!(to_version, 10);
            assert_eq!(points.len(), 2);
            assert_eq!(points[0].channel, 1113);
            assert_eq!(points[1].channel, 1206);
        } else {
            panic!("expected Delta");
        }
    }

    #[test]
    fn warp_versions_roundtrip() {
        let msg = WarpMessage::Versions {
            node_id: "node-a".to_string(),
            versions: HashMap::from([("node-a".to_string(), 1542), ("node-b".to_string(), 1200)]),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: WarpMessage = serde_json::from_str(&json).unwrap();

        if let WarpMessage::Versions { node_id, versions } = decoded {
            assert_eq!(node_id, "node-a");
            assert_eq!(*versions.get("node-a").unwrap(), 1542);
            assert_eq!(*versions.get("node-b").unwrap(), 1200);
        } else {
            panic!("expected Versions");
        }
    }

    #[test]
    fn warp_delta_req_roundtrip() {
        let msg = WarpMessage::DeltaReq {
            node_id: "node-b".to_string(),
            want_from: HashMap::from([("node-a".to_string(), 1500)]),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: WarpMessage = serde_json::from_str(&json).unwrap();

        if let WarpMessage::DeltaReq { node_id, want_from } = decoded {
            assert_eq!(node_id, "node-b");
            assert_eq!(*want_from.get("node-a").unwrap(), 1500);
        } else {
            panic!("expected DeltaReq");
        }
    }

    #[test]
    fn warp_ack_serialize() {
        let msg = WarpMessage::Ack {
            node_id: "node-b".to_string(),
            version: 42,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"warp:ack\""));
        assert!(json.contains("\"version\":42"));
    }

    #[test]
    fn warp_full_req_roundtrip() {
        let msg = WarpMessage::FullReq {
            node_id: "node-b".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: WarpMessage = serde_json::from_str(&json).unwrap();
        if let WarpMessage::FullReq { node_id } = decoded {
            assert_eq!(node_id, "node-b");
        } else {
            panic!("expected FullReq");
        }
    }

    #[test]
    fn warp_full_roundtrip() {
        let msg = WarpMessage::Full {
            node_id: "node-a".to_string(),
            version: 100,
            points: vec![VersionedPoint {
                channel: 1113,
                value: 72.5,
                unit: "degF".to_string(),
                status: "ok".to_string(),
                version: 100,
                timestamp: 1706000000_000,
            }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: WarpMessage = serde_json::from_str(&json).unwrap();
        if let WarpMessage::Full {
            node_id,
            version,
            points,
        } = decoded
        {
            assert_eq!(node_id, "node-a");
            assert_eq!(version, 100);
            assert_eq!(points.len(), 1);
        } else {
            panic!("expected Full");
        }
    }

    // ── Filter evaluation tests ──────────────────────────

    fn test_point(channel: u32, value: f64, unit: &str, status: &str) -> VersionedPoint {
        VersionedPoint {
            channel,
            value,
            unit: unit.to_string(),
            status: status.to_string(),
            version: 1,
            timestamp: 1706000000_000,
        }
    }

    #[test]
    fn filter_point_matches_all() {
        let p = test_point(1113, 72.5, "degF", "ok");
        assert!(evaluate_point_filter("point", &p));
    }

    #[test]
    fn filter_channel_eq() {
        let p = test_point(1113, 72.5, "degF", "ok");
        assert!(evaluate_point_filter("channel==1113", &p));
        assert!(!evaluate_point_filter("channel==612", &p));
    }

    #[test]
    fn filter_channel_gt() {
        let p = test_point(1113, 72.5, "degF", "ok");
        assert!(evaluate_point_filter("channel > 1000", &p));
        assert!(!evaluate_point_filter("channel > 2000", &p));
    }

    #[test]
    fn filter_channel_lt() {
        let p = test_point(1113, 72.5, "degF", "ok");
        assert!(evaluate_point_filter("channel < 2000", &p));
        assert!(!evaluate_point_filter("channel < 1000", &p));
    }

    #[test]
    fn filter_channel_ge_le() {
        let p = test_point(1113, 72.5, "degF", "ok");
        assert!(evaluate_point_filter("channel >= 1113", &p));
        assert!(evaluate_point_filter("channel <= 1113", &p));
        assert!(!evaluate_point_filter("channel >= 1114", &p));
        assert!(!evaluate_point_filter("channel <= 1112", &p));
    }

    #[test]
    fn filter_unit_substring() {
        let p = test_point(1113, 72.5, "degF", "ok");
        assert!(evaluate_point_filter("degf", &p));
        assert!(!evaluate_point_filter("mA", &p));
    }

    #[test]
    fn filter_status_substring() {
        let p = test_point(1113, 72.5, "degF", "fault");
        assert!(evaluate_point_filter("fault", &p));
        assert!(!evaluate_point_filter("ok", &p));
    }

    #[test]
    fn filter_unit_eq() {
        let p = test_point(1113, 72.5, "degF", "ok");
        assert!(evaluate_point_filter("unit==\"degF\"", &p));
        assert!(!evaluate_point_filter("unit==\"mA\"", &p));
    }

    #[test]
    fn filter_status_eq() {
        let p = test_point(1113, 72.5, "degF", "ok");
        assert!(evaluate_point_filter("status==\"ok\"", &p));
        assert!(!evaluate_point_filter("status==\"fault\"", &p));
    }

    #[test]
    fn filter_value_gt() {
        let p = test_point(1113, 72.5, "degF", "ok");
        assert!(evaluate_point_filter("value > 70", &p));
        assert!(!evaluate_point_filter("value > 80", &p));
    }

    #[test]
    fn filter_and() {
        let p = test_point(1113, 72.5, "degF", "ok");
        assert!(evaluate_point_filter("point and channel==1113", &p));
        assert!(!evaluate_point_filter("point and channel==612", &p));
    }

    #[test]
    fn filter_or() {
        let p = test_point(1113, 72.5, "degF", "ok");
        assert!(evaluate_point_filter("channel==1113 or channel==612", &p));
        assert!(evaluate_point_filter("channel==612 or channel==1113", &p));
        assert!(!evaluate_point_filter("channel==612 or channel==999", &p));
    }

    #[test]
    fn filter_channel_range_and() {
        let p = test_point(1113, 72.5, "degF", "ok");
        assert!(evaluate_point_filter(
            "channel > 1000 and channel < 2000",
            &p
        ));
        assert!(!evaluate_point_filter(
            "channel > 1000 and channel < 1100",
            &p
        ));
    }

    // ── Query message serialization ──────────────────────

    #[test]
    fn warp_query_serialize() {
        let msg = WarpMessage::Query {
            node_id: "node-a".to_string(),
            query_id: "q-001".to_string(),
            filter: "point and temp".to_string(),
            limit: Some(100),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"warp:query\""));
        assert!(json.contains("\"queryId\":\"q-001\""));
        assert!(json.contains("\"filter\":\"point and temp\""));
        assert!(json.contains("\"limit\":100"));
    }

    #[test]
    fn warp_query_result_serialize() {
        let msg = WarpMessage::QueryResult {
            node_id: "node-b".to_string(),
            query_id: "q-001".to_string(),
            results: vec![super::super::protocol::QueryPoint {
                channel: 1113,
                value: 73.2,
                unit: "degF".into(),
                status: "ok".into(),
                node_id: "node-b".into(),
            }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"warp:queryResult\""));
        assert!(json.contains("\"results\""));

        let decoded: WarpMessage = serde_json::from_str(&json).unwrap();
        if let WarpMessage::QueryResult {
            node_id,
            query_id,
            results,
        } = decoded
        {
            assert_eq!(node_id, "node-b");
            assert_eq!(query_id, "q-001");
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].channel, 1113);
        } else {
            panic!("expected QueryResult");
        }
    }
}
