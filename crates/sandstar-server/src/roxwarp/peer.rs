//! Outbound peer connection — connects to a remote roxWarp peer and runs
//! the gossip loop with reconnection and exponential backoff.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::RwLock;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
use tracing::{debug, info, warn};

use super::cluster::{DeltaEngine, PeerConfig, PeerState};
use super::handler::WarpMessage;

/// Optional mTLS client configuration for outbound connections.
pub type MtlsClientConfig = Option<Arc<rustls::ClientConfig>>;

/// Maximum backoff delay between reconnection attempts (60 seconds).
const MAX_BACKOFF_SECS: u64 = 60;

/// Connect to a remote roxWarp peer with automatic reconnection.
///
/// This function runs forever, attempting to maintain a connection to the
/// configured peer. On disconnect it waits with exponential backoff before
/// retrying.
///
/// When `tls_config` is `Some`, outbound connections use WSS with mTLS.
/// When `debug_mode` is true, messages use JSON text frames instead of binary.
pub async fn connect_to_peer(
    peer: &PeerConfig,
    delta_engine: Arc<DeltaEngine>,
    peer_states: Arc<RwLock<HashMap<String, PeerState>>>,
    heartbeat_secs: u64,
    anti_entropy_secs: u64,
    tls_config: MtlsClientConfig,
    debug_mode: bool,
) {
    let mut backoff_secs = 1u64;

    loop {
        // Update state to Connecting
        set_peer_state(&peer_states, &peer.node_id, PeerState::Connecting).await;

        info!(
            peer = %peer.node_id,
            address = %peer.address,
            "roxWarp: connecting to peer"
        );

        match try_connect(
            peer,
            &delta_engine,
            &peer_states,
            heartbeat_secs,
            anti_entropy_secs,
            &tls_config,
            debug_mode,
        )
        .await
        {
            Ok(()) => {
                info!(peer = %peer.node_id, "roxWarp: peer session ended normally");
                backoff_secs = 1; // Reset backoff on clean disconnect
            }
            Err(e) => {
                warn!(
                    peer = %peer.node_id,
                    error = %e,
                    backoff_secs = backoff_secs,
                    "roxWarp: peer connection failed"
                );
            }
        }

        // Update state to Offline
        set_peer_state(&peer_states, &peer.node_id, PeerState::Offline).await;

        // Exponential backoff
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }
}

/// Attempt a single connection to a peer and run the gossip loop.
async fn try_connect(
    peer: &PeerConfig,
    delta_engine: &Arc<DeltaEngine>,
    peer_states: &Arc<RwLock<HashMap<String, PeerState>>>,
    heartbeat_secs: u64,
    anti_entropy_secs: u64,
    tls_config: &MtlsClientConfig,
    debug_mode: bool,
) -> Result<(), String> {
    // Build WebSocket URL: WSS when mTLS is configured, WS otherwise
    let (url, ws_stream) = if let Some(tls_cfg) = tls_config {
        let url = format!("wss://{}/roxwarp", peer.address);
        let connector = tokio_tungstenite::Connector::Rustls(tls_cfg.clone());
        let (stream, _) =
            tokio_tungstenite::connect_async_tls_with_config(&url, None, false, Some(connector))
                .await
                .map_err(|e| format!("WSS connect failed: {e}"))?;
        (url, stream)
    } else {
        let url = format!("ws://{}/roxwarp", peer.address);
        let (stream, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| format!("WebSocket connect failed: {e}"))?;
        (url, stream)
    };
    let _ = url; // suppress unused warning

    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // Phase 1: Send hello
    set_peer_state(peer_states, &peer.node_id, PeerState::Handshake).await;

    let our_versions = delta_engine.get_version_vector().await;
    let hello = WarpMessage::Hello {
        node_id: delta_engine.node_id.clone(),
        versions: our_versions,
    };
    send_tungstenite(&mut ws_tx, &hello, debug_mode).await?;

    // Phase 2: Wait for welcome
    let peer_versions = match wait_for_welcome(&mut ws_rx).await {
        Some((node_id, versions)) => {
            info!(peer = %node_id, "roxWarp: received welcome");
            versions
        }
        None => {
            return Err("no welcome received".to_string());
        }
    };

    // Phase 3: Send initial delta based on peer's version vector
    set_peer_state(peer_states, &peer.node_id, PeerState::Syncing).await;

    let peer_has_our_version = peer_versions
        .get(&delta_engine.node_id)
        .copied()
        .unwrap_or(0);
    let deltas = delta_engine.delta_since(peer_has_our_version).await;
    let current_version = delta_engine.current_version();

    if !deltas.is_empty() {
        let delta_msg = WarpMessage::Delta {
            node_id: delta_engine.node_id.clone(),
            from_version: peer_has_our_version,
            to_version: current_version,
            points: deltas,
        };
        send_tungstenite(&mut ws_tx, &delta_msg, debug_mode).await?;
    }

    // Phase 4: Active gossip loop
    set_peer_state(peer_states, &peer.node_id, PeerState::Active).await;

    let mut heartbeat_timer =
        tokio::time::interval(Duration::from_secs(heartbeat_secs));
    heartbeat_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut anti_entropy_timer =
        tokio::time::interval(Duration::from_secs(anti_entropy_secs));
    anti_entropy_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut last_sent_version = current_version;
    let mut last_activity = Instant::now();
    let timeout = Duration::from_secs(120);

    loop {
        tokio::select! {
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(TungsteniteMessage::Binary(data))) => {
                        last_activity = Instant::now();
                        handle_peer_binary(
                            &data,
                            delta_engine,
                            &peer.node_id,
                            &mut ws_tx,
                            debug_mode,
                        ).await?;
                    }
                    Some(Ok(TungsteniteMessage::Text(text))) => {
                        last_activity = Instant::now();
                        handle_peer_text(
                            &text,
                            delta_engine,
                            &peer.node_id,
                            &mut ws_tx,
                            debug_mode,
                        ).await?;
                    }
                    Some(Ok(TungsteniteMessage::Ping(data))) => {
                        last_activity = Instant::now();
                        ws_tx
                            .send(TungsteniteMessage::Pong(data))
                            .await
                            .map_err(|e| format!("pong failed: {e}"))?;
                    }
                    Some(Ok(TungsteniteMessage::Close(_))) | None => {
                        info!(peer = %peer.node_id, "roxWarp: peer closed connection");
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        return Err(format!("WebSocket error: {e}"));
                    }
                    _ => {
                        last_activity = Instant::now();
                    }
                }
            }

            _ = heartbeat_timer.tick() => {
                // Check timeout
                if last_activity.elapsed() > timeout {
                    return Err("peer timed out".to_string());
                }

                // Send heartbeat
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let hb = WarpMessage::Heartbeat {
                    node_id: delta_engine.node_id.clone(),
                    timestamp: now_ms,
                };
                send_tungstenite(&mut ws_tx, &hb, debug_mode).await?;

                // Push any new deltas
                let current = delta_engine.current_version();
                if current > last_sent_version {
                    let deltas = delta_engine.delta_since(last_sent_version).await;
                    if !deltas.is_empty() {
                        let delta_msg = WarpMessage::Delta {
                            node_id: delta_engine.node_id.clone(),
                            from_version: last_sent_version,
                            to_version: current,
                            points: deltas,
                        };
                        send_tungstenite(&mut ws_tx, &delta_msg, debug_mode).await?;
                        last_sent_version = current;
                    }
                }
            }

            _ = anti_entropy_timer.tick() => {
                let versions = delta_engine.get_version_vector().await;
                let msg = WarpMessage::Versions {
                    node_id: delta_engine.node_id.clone(),
                    versions,
                };
                send_tungstenite(&mut ws_tx, &msg, debug_mode).await?;
            }
        }
    }
}

/// Wait for a `warp:welcome` response from the remote peer.
///
/// Accepts both binary (MessagePack) and text (JSON) frames.
async fn wait_for_welcome<S>(
    ws_rx: &mut futures_util::stream::SplitStream<S>,
) -> Option<(String, HashMap<String, u64>)>
where
    S: futures_util::Stream<Item = Result<TungsteniteMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    match tokio::time::timeout(Duration::from_secs(10), ws_rx.next()).await {
        Ok(Some(Ok(TungsteniteMessage::Binary(data)))) => {
            if let Ok(WarpMessage::Welcome {
                node_id, versions, ..
            }) = rmp_serde::from_slice(&data)
            {
                return Some((node_id, versions));
            }
            None
        }
        Ok(Some(Ok(TungsteniteMessage::Text(text)))) => {
            if let Ok(WarpMessage::Welcome {
                node_id, versions, ..
            }) = serde_json::from_str(&text)
            {
                return Some((node_id, versions));
            }
            None
        }
        _ => None,
    }
}

/// Handle an incoming binary (MessagePack) message from the connected peer.
async fn handle_peer_binary<S>(
    data: &[u8],
    delta_engine: &Arc<DeltaEngine>,
    peer_node_id: &str,
    ws_tx: &mut futures_util::stream::SplitSink<S, TungsteniteMessage>,
    debug_mode: bool,
) -> Result<(), String>
where
    S: futures_util::Sink<TungsteniteMessage> + Unpin,
{
    let msg: WarpMessage = rmp_serde::from_slice(data)
        .map_err(|e| format!("invalid msgpack message: {e}"))?;
    process_peer_message(msg, delta_engine, peer_node_id, ws_tx, debug_mode).await
}

/// Handle an incoming text (JSON) message from the connected peer.
async fn handle_peer_text<S>(
    text: &str,
    delta_engine: &Arc<DeltaEngine>,
    peer_node_id: &str,
    ws_tx: &mut futures_util::stream::SplitSink<S, TungsteniteMessage>,
    debug_mode: bool,
) -> Result<(), String>
where
    S: futures_util::Sink<TungsteniteMessage> + Unpin,
{
    let msg: WarpMessage = serde_json::from_str(text)
        .map_err(|e| format!("invalid json message: {e}"))?;
    process_peer_message(msg, delta_engine, peer_node_id, ws_tx, debug_mode).await
}

/// Process a decoded WarpMessage from either binary or text frame.
async fn process_peer_message<S>(
    msg: WarpMessage,
    delta_engine: &Arc<DeltaEngine>,
    peer_node_id: &str,
    ws_tx: &mut futures_util::stream::SplitSink<S, TungsteniteMessage>,
    debug_mode: bool,
) -> Result<(), String>
where
    S: futures_util::Sink<TungsteniteMessage> + Unpin,
{
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
                "roxWarp peer: received delta"
            );
            delta_engine.apply_remote_delta(&node_id, points).await;

            // Ack
            let ack = WarpMessage::Ack {
                node_id: delta_engine.node_id.clone(),
                version: to_version,
            };
            send_tungstenite(ws_tx, &ack, debug_mode).await?;
        }

        WarpMessage::Heartbeat { node_id, .. } => {
            debug!(peer = %node_id, "roxWarp peer: received heartbeat");
        }

        WarpMessage::Versions {
            node_id, versions, ..
        } => {
            debug!(peer = %node_id, "roxWarp peer: received version vector");
            // Check if peer is behind on our data
            let our_version = delta_engine.current_version();
            let peer_has = versions
                .get(&delta_engine.node_id)
                .copied()
                .unwrap_or(0);
            if peer_has < our_version {
                let deltas = delta_engine.delta_since(peer_has).await;
                if !deltas.is_empty() {
                    let delta_msg = WarpMessage::Delta {
                        node_id: delta_engine.node_id.clone(),
                        from_version: peer_has,
                        to_version: our_version,
                        points: deltas,
                    };
                    send_tungstenite(ws_tx, &delta_msg, debug_mode).await?;
                }
            }
        }

        WarpMessage::Ack { node_id, version } => {
            debug!(peer = %node_id, version = version, "roxWarp peer: received ack");
            delta_engine.ack_peer(&node_id, version).await;
        }

        WarpMessage::Full {
            node_id,
            version: _,
            points,
        } => {
            debug!(
                peer = %node_id,
                count = points.len(),
                "roxWarp peer: received full state"
            );
            delta_engine.apply_remote_delta(&node_id, points).await;
        }

        _ => {
            debug!(peer = %peer_node_id, "roxWarp peer: ignoring unexpected message");
        }
    }

    Ok(())
}

/// Send a WarpMessage over a tokio-tungstenite WebSocket.
///
/// Uses binary (MessagePack) frames by default for efficiency.
/// In debug mode, uses text (JSON) frames for human inspection.
async fn send_tungstenite<S>(
    ws_tx: &mut futures_util::stream::SplitSink<S, TungsteniteMessage>,
    msg: &WarpMessage,
    debug_mode: bool,
) -> Result<(), String>
where
    S: futures_util::Sink<TungsteniteMessage> + Unpin,
{
    if debug_mode {
        let json = serde_json::to_string(msg).map_err(|e| format!("serialize json: {e}"))?;
        ws_tx
            .send(TungsteniteMessage::Text(json.into()))
            .await
            .map_err(|_| "send failed".to_string())
    } else {
        let bytes = rmp_serde::to_vec(msg).map_err(|e| format!("serialize msgpack: {e}"))?;
        ws_tx
            .send(TungsteniteMessage::Binary(bytes.into()))
            .await
            .map_err(|_| "send failed".to_string())
    }
}

/// Update peer state in the shared state map.
async fn set_peer_state(
    peer_states: &Arc<RwLock<HashMap<String, PeerState>>>,
    peer_id: &str,
    state: PeerState,
) {
    peer_states
        .write()
        .await
        .insert(peer_id.to_string(), state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_backoff_is_reasonable() {
        assert_eq!(MAX_BACKOFF_SECS, 60);
    }

    #[test]
    fn backoff_growth() {
        let mut backoff = 1u64;
        let mut steps = vec![backoff];
        for _ in 0..10 {
            backoff = (backoff * 2).min(MAX_BACKOFF_SECS);
            steps.push(backoff);
        }
        // Should grow: 1, 2, 4, 8, 16, 32, 60, 60, 60, 60, 60
        assert_eq!(steps[0], 1);
        assert_eq!(steps[1], 2);
        assert_eq!(steps[2], 4);
        assert_eq!(steps[5], 32);
        assert_eq!(steps[6], 60);
        assert_eq!(steps[10], 60);
    }

    #[tokio::test]
    async fn set_peer_state_works() {
        let states: Arc<RwLock<HashMap<String, PeerState>>> =
            Arc::new(RwLock::new(HashMap::new()));

        set_peer_state(&states, "peer-1", PeerState::Connecting).await;
        assert_eq!(
            *states.read().await.get("peer-1").unwrap(),
            PeerState::Connecting
        );

        set_peer_state(&states, "peer-1", PeerState::Active).await;
        assert_eq!(
            *states.read().await.get("peer-1").unwrap(),
            PeerState::Active
        );
    }
}
