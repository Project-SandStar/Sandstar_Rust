//! Haystack-over-WebSocket: real-time channel value push.
//!
//! Clients connect to `GET /api/ws`, optionally authenticate, subscribe
//! to channels, and receive server-pushed value updates.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

use super::EngineHandle;
use crate::auth::AuthState;
use crate::drivers::actor::DriverHandle;
use crate::drivers::CovEvent;

// ── Constants ──────────────────────────────────────

const MAX_WS_CONNECTIONS: i64 = 32;
const MIN_POLL_INTERVAL_MS: u64 = 200;
const MAX_POLL_INTERVAL_MS: u64 = 60_000;
const DEFAULT_POLL_INTERVAL_MS: u64 = 1000;
const CLIENT_TIMEOUT_SECS: u64 = 120;

// ── State ──────────────────────────────────────────

#[derive(Clone)]
pub struct WsState {
    pub engine: EngineHandle,
    pub auth_token: Option<String>,
    /// SCRAM auth state (None = legacy-only mode for backward compat).
    pub auth_state: Option<AuthState>,
    /// Optional driver actor handle. When present, each WS session
    /// subscribes to the driver `CovEvent` broadcast and expedites watch
    /// polls on change-of-value for sub-second push latency. `None`
    /// preserves legacy poll-only behavior (used by some older tests).
    pub driver_handle: Option<DriverHandle>,
}

#[derive(Deserialize)]
pub struct WsParams {
    token: Option<String>,
}

// ── Client → Server messages ───────────────────────

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum ClientMsg {
    Auth {
        token: String,
        #[serde(default)]
        id: Option<String>,
    },
    /// SCRAM step 1: client sends username to begin handshake.
    Hello {
        username: String,
        #[serde(default)]
        id: Option<String>,
    },
    /// SCRAM step 3: client sends proof to complete handshake.
    Authenticate {
        #[serde(rename = "handshakeToken")]
        handshake_token: String,
        proof: String,
        #[serde(default)]
        id: Option<String>,
    },
    Subscribe {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, rename = "watchId")]
        watch_id: Option<String>,
        #[serde(default)]
        dis: Option<String>,
        ids: Vec<u32>,
        #[serde(default, rename = "pollInterval")]
        poll_interval: Option<u64>,
    },
    Unsubscribe {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "watchId")]
        watch_id: String,
        #[serde(default)]
        ids: Vec<u32>,
        #[serde(default)]
        close: bool,
    },
    Refresh {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "watchId")]
        watch_id: String,
    },
    Ping {
        #[serde(default)]
        id: Option<String>,
    },
}

// ── Server → Client messages ───────────────────────

#[derive(Serialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum ServerMsg<'a> {
    AuthOk {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Session token returned after successful SCRAM auth.
        #[serde(skip_serializing_if = "Option::is_none", rename = "authToken")]
        auth_token: Option<String>,
    },
    /// SCRAM step 2: server sends challenge back to client.
    Challenge {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(rename = "handshakeToken")]
        handshake_token: &'a str,
        hash: &'a str,
        salt: &'a str,
        iterations: u32,
    },
    Subscribed {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(rename = "watchId")]
        watch_id: &'a str,
        lease: u32,
        #[serde(rename = "pollInterval")]
        poll_interval: u64,
        rows: &'a [super::ChannelValue],
    },
    Unsubscribed {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(rename = "watchId")]
        watch_id: &'a str,
        ok: bool,
    },
    Update {
        #[serde(rename = "watchId")]
        watch_id: &'a str,
        ts: &'a str,
        rows: &'a [super::ChannelValue],
    },
    Snapshot {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(rename = "watchId")]
        watch_id: &'a str,
        ts: &'a str,
        rows: &'a [super::ChannelValue],
    },
    Pong {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        code: &'a str,
        message: &'a str,
    },
}

// ── Per-watch metadata ─────────────────────────────

struct WatchMeta {
    poll_interval: Duration,
    last_poll: Instant,
    /// Set of point/channel ids the client subscribed to on this watch.
    /// Used by the CovEvent bridge (Phase 12.0D.WS) to decide whether a
    /// given driver-side value change should expedite the next poll for
    /// this watch.
    subscribed_ids: HashSet<u32>,
    /// Set to `true` when a relevant `CovEvent` arrives between pushes.
    /// The next push-timer tick will poll the engine even if the interval
    /// hasn't elapsed, then reset the flag. Provides sub-second push
    /// latency bounded by the 200 ms push-timer resolution, while the
    /// engine's channel write (level 16) has already settled.
    cov_pending: bool,
}

// ── Upgrade handler ────────────────────────────────

pub async fn ws_upgrade(
    ws: WebSocketUpgrade,
    Query(params): Query<WsParams>,
    State(state): State<WsState>,
) -> Response {
    // Check connection limit
    let current = crate::metrics::metrics()
        .ws_active
        .load(std::sync::atomic::Ordering::Relaxed);
    if current >= MAX_WS_CONNECTIONS {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many WebSocket connections",
        )
            .into_response();
    }

    // Check query-param auth
    let pre_authed = match (&state.auth_token, &params.token) {
        (None, _) => true,
        (Some(required), Some(provided)) => provided == required,
        _ => false,
    };

    ws.on_upgrade(move |socket| {
        ws_connection_task(
            socket,
            state.engine,
            state.auth_token,
            state.auth_state,
            state.driver_handle,
            pre_authed,
        )
    })
}

// ── Connection task ────────────────────────────────

async fn ws_connection_task(
    ws: WebSocket,
    engine: EngineHandle,
    auth_token: Option<String>,
    auth_state: Option<AuthState>,
    driver_handle: Option<DriverHandle>,
    pre_authed: bool,
) {
    let metrics = crate::metrics::metrics();
    metrics
        .ws_active
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    metrics
        .ws_total
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    info!("WebSocket connected (pre_authed={})", pre_authed);

    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut authenticated = pre_authed;
    let mut watches: HashMap<String, WatchMeta> = HashMap::new();
    let mut last_client_msg = Instant::now();

    let mut push_timer = tokio::time::interval(Duration::from_millis(MIN_POLL_INTERVAL_MS));
    push_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Phase 12.0D.WS: subscribe to the driver actor's CovEvent broadcast
    // if we have a handle. On event, mark matching watches as `cov_pending`
    // so the next push-timer tick polls immediately regardless of interval.
    // Rate limiting is therefore the push timer itself (200 ms).
    let mut cov_rx: Option<broadcast::Receiver<CovEvent>> =
        driver_handle.as_ref().map(|h| h.subscribe_cov());

    loop {
        tokio::select! {
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        last_client_msg = Instant::now();
                        metrics.ws_messages_in.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // parse and handle
                        let reply = handle_client_msg(
                            &text, &engine, &auth_token, &auth_state,
                            &mut authenticated, &mut watches,
                        ).await;
                        if let Some(reply_json) = reply {
                            metrics.ws_messages_out.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if ws_tx.send(Message::Text(reply_json.into())).await.is_err() {
                                break; // client gone
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        last_client_msg = Instant::now();
                        if ws_tx.send(Message::Pong(data)).await.is_err() { break; }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => { last_client_msg = Instant::now(); }
                }
            }
            // Phase 12.0D.WS: listen for CovEvents (no-op if no driver
            // handle — inner future stays pending forever and never fires).
            cov = async {
                match cov_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match cov {
                    Ok(evt) => {
                        apply_cov_event(&mut watches, evt.point_id);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Subscriber fell behind; nothing to forward for
                        // the dropped events. Clients will still receive
                        // the latest state on the next poll tick.
                        debug!(
                            dropped = n,
                            "WS CovEvent subscriber lagged; next poll will resync"
                        );
                        mark_all_cov_pending(&mut watches);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Driver actor shut down. Drop the subscription;
                        // fall back to poll-only until reconnect.
                        cov_rx = None;
                    }
                }
            }
            _ = push_timer.tick() => {
                // Check client timeout
                if last_client_msg.elapsed() > Duration::from_secs(CLIENT_TIMEOUT_SECS) {
                    debug!("WebSocket client timeout after {}s inactivity", CLIENT_TIMEOUT_SECS);
                    break;
                }

                // Poll each watch whose interval has elapsed, OR any watch
                // with a pending CovEvent (Phase 12.0D.WS).
                if !authenticated { continue; }
                let now = Instant::now();
                let ts = timestamp_now();
                let mut disconnected = false;
                for (watch_id, meta) in watches.iter_mut() {
                    let interval_elapsed =
                        now.duration_since(meta.last_poll) >= meta.poll_interval;
                    if !interval_elapsed && !meta.cov_pending {
                        continue;
                    }
                    meta.cov_pending = false;
                    meta.last_poll = now;
                    match engine.watch_poll(watch_id.clone(), false).await {
                        Ok(resp) if !resp.rows.is_empty() => {
                            let msg = ServerMsg::Update {
                                watch_id: &resp.watch_id,
                                ts: &ts,
                                rows: &resp.rows,
                            };
                            if let Ok(json) = serde_json::to_string(&msg) {
                                metrics.ws_messages_out.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if ws_tx.send(Message::Text(json.into())).await.is_err() {
                                    disconnected = true;
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            warn!(watch_id = %watch_id, error = %e, "watch poll failed in WS push");
                        }
                        _ => {} // no changes
                    }
                }
                if disconnected { break; }
            }
        }
    }

    // Cleanup: close all watches
    for watch_id in watches.keys() {
        let _ = engine.watch_unsub(watch_id.clone(), true, vec![]).await;
    }

    metrics
        .ws_active
        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    info!(
        "WebSocket disconnected (watches cleaned: {})",
        watches.len()
    );
}

// ── Client message handler ─────────────────────────

async fn handle_client_msg(
    text: &str,
    engine: &EngineHandle,
    auth_token: &Option<String>,
    auth_state: &Option<AuthState>,
    authenticated: &mut bool,
    watches: &mut HashMap<String, WatchMeta>,
) -> Option<String> {
    let msg: ClientMsg = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(_) => {
            return serde_json::to_string(&ServerMsg::Error {
                id: None,
                code: "INVALID_MESSAGE",
                message: "invalid JSON or unknown op",
            })
            .ok();
        }
    };

    match msg {
        ClientMsg::Auth { token, id } => {
            // Legacy bearer token auth
            match auth_token {
                None => {
                    // No auth configured — also check auth_state
                    let required = auth_state
                        .as_ref()
                        .is_some_and(|a| a.store.is_auth_required());
                    if !required {
                        *authenticated = true;
                        serde_json::to_string(&ServerMsg::AuthOk {
                            id,
                            auth_token: None,
                        })
                        .ok()
                    } else if let Some(ref state) = auth_state {
                        // Check as session token
                        if state.check_token(&token) {
                            *authenticated = true;
                            serde_json::to_string(&ServerMsg::AuthOk {
                                id,
                                auth_token: None,
                            })
                            .ok()
                        } else {
                            serde_json::to_string(&ServerMsg::Error {
                                id,
                                code: "AUTH_REQUIRED",
                                message: "invalid token",
                            })
                            .ok()
                        }
                    } else {
                        serde_json::to_string(&ServerMsg::Error {
                            id,
                            code: "AUTH_REQUIRED",
                            message: "invalid token",
                        })
                        .ok()
                    }
                }
                Some(required) if token == *required => {
                    *authenticated = true;
                    serde_json::to_string(&ServerMsg::AuthOk {
                        id,
                        auth_token: None,
                    })
                    .ok()
                }
                _ => {
                    // Check session token via auth_state
                    if let Some(ref state) = auth_state {
                        if state.check_token(&token) {
                            *authenticated = true;
                            return serde_json::to_string(&ServerMsg::AuthOk {
                                id,
                                auth_token: None,
                            })
                            .ok();
                        }
                    }
                    serde_json::to_string(&ServerMsg::Error {
                        id,
                        code: "AUTH_REQUIRED",
                        message: "invalid token",
                    })
                    .ok()
                }
            }
        }
        ClientMsg::Hello { username, id } => {
            // SCRAM step 1: client sends username
            let Some(ref state) = auth_state else {
                return serde_json::to_string(&ServerMsg::Error {
                    id,
                    code: "AUTH_REQUIRED",
                    message: "SCRAM auth not configured",
                })
                .ok();
            };

            // Build a client-first-message from the username
            let nonce = crate::auth::generate_nonce();
            let client_first = format!("n,,n={},r={}", username, nonce);

            match state.begin_scram(&client_first) {
                Ok((hs_token, server_first)) => {
                    // Parse salt + iterations from server-first
                    let mut salt_b64 = String::new();
                    let mut iterations = 0u32;
                    for part in server_first.split(',') {
                        if let Some(s) = part.strip_prefix("s=") {
                            salt_b64 = s.to_string();
                        } else if let Some(i) = part.strip_prefix("i=") {
                            iterations = i.parse().unwrap_or(0);
                        }
                    }
                    serde_json::to_string(&ServerMsg::Challenge {
                        id,
                        handshake_token: &hs_token,
                        hash: "SHA-256",
                        salt: &salt_b64,
                        iterations,
                    })
                    .ok()
                }
                Err(e) => serde_json::to_string(&ServerMsg::Error {
                    id,
                    code: "AUTH_REQUIRED",
                    message: &e,
                })
                .ok(),
            }
        }
        ClientMsg::Authenticate {
            handshake_token,
            proof,
            id,
        } => {
            // SCRAM step 3: client sends proof
            let Some(ref state) = auth_state else {
                return serde_json::to_string(&ServerMsg::Error {
                    id,
                    code: "AUTH_REQUIRED",
                    message: "SCRAM auth not configured",
                })
                .ok();
            };

            // The proof field is base64(client-final-message)
            let client_final =
                match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &proof) {
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Ok(s) => s,
                        Err(_) => {
                            return serde_json::to_string(&ServerMsg::Error {
                                id,
                                code: "AUTH_REQUIRED",
                                message: "invalid UTF-8 in proof",
                            })
                            .ok();
                        }
                    },
                    Err(_) => {
                        return serde_json::to_string(&ServerMsg::Error {
                            id,
                            code: "AUTH_REQUIRED",
                            message: "invalid base64 in proof",
                        })
                        .ok();
                    }
                };

            match state.complete_scram(&handshake_token, &client_final) {
                Ok((session_token, _server_sig)) => {
                    *authenticated = true;
                    serde_json::to_string(&ServerMsg::AuthOk {
                        id,
                        auth_token: Some(session_token),
                    })
                    .ok()
                }
                Err(e) => serde_json::to_string(&ServerMsg::Error {
                    id,
                    code: "AUTH_REQUIRED",
                    message: &e,
                })
                .ok(),
            }
        }
        ClientMsg::Ping { id } => serde_json::to_string(&ServerMsg::Pong { id }).ok(),
        _ if !*authenticated => serde_json::to_string(&ServerMsg::Error {
            id: None,
            code: "AUTH_REQUIRED",
            message: "authenticate first",
        })
        .ok(),
        ClientMsg::Subscribe {
            id,
            watch_id,
            dis,
            ids,
            poll_interval,
        } => {
            let interval_ms = poll_interval
                .unwrap_or(DEFAULT_POLL_INTERVAL_MS)
                .clamp(MIN_POLL_INTERVAL_MS, MAX_POLL_INTERVAL_MS);

            let subscribed_ids_set: HashSet<u32> = ids.iter().copied().collect();
            match engine.watch_sub(watch_id, dis, ids).await {
                Ok(resp) => {
                    let wid = resp.watch_id.clone();
                    watches.insert(
                        wid,
                        WatchMeta {
                            poll_interval: Duration::from_millis(interval_ms),
                            last_poll: Instant::now(),
                            subscribed_ids: subscribed_ids_set,
                            cov_pending: false,
                        },
                    );
                    serde_json::to_string(&ServerMsg::Subscribed {
                        id,
                        watch_id: &resp.watch_id,
                        lease: resp.lease,
                        poll_interval: interval_ms,
                        rows: &resp.rows,
                    })
                    .ok()
                }
                Err(e) => {
                    let code = if e.contains("limit") {
                        "WATCH_LIMIT"
                    } else if e.contains("channels") {
                        "CHANNEL_LIMIT"
                    } else {
                        "ENGINE_ERROR"
                    };
                    serde_json::to_string(&ServerMsg::Error {
                        id,
                        code,
                        message: &e,
                    })
                    .ok()
                }
            }
        }
        ClientMsg::Unsubscribe {
            id,
            watch_id,
            ids,
            close,
        } => {
            // Capture ids before passing them to the engine (moved there).
            let unsub_ids: Vec<u32> = ids.clone();
            match engine.watch_unsub(watch_id.clone(), close, ids).await {
            Ok(()) => {
                if close {
                    watches.remove(&watch_id);
                } else if let Some(meta) = watches.get_mut(&watch_id) {
                    // Remove partially-unsubscribed ids from the COV filter.
                    for pid in &unsub_ids {
                        meta.subscribed_ids.remove(pid);
                    }
                }
                serde_json::to_string(&ServerMsg::Unsubscribed {
                    id,
                    watch_id: &watch_id,
                    ok: true,
                })
                .ok()
            }
            Err(e) => serde_json::to_string(&ServerMsg::Error {
                id,
                code: "WATCH_NOT_FOUND",
                message: &e,
            })
            .ok(),
            }
        }
        ClientMsg::Refresh { id, watch_id } => {
            match engine.watch_poll(watch_id.clone(), true).await {
                Ok(resp) => {
                    let ts = timestamp_now();
                    serde_json::to_string(&ServerMsg::Snapshot {
                        id,
                        watch_id: &resp.watch_id,
                        ts: &ts,
                        rows: &resp.rows,
                    })
                    .ok()
                }
                Err(e) => serde_json::to_string(&ServerMsg::Error {
                    id,
                    code: "WATCH_NOT_FOUND",
                    message: &e,
                })
                .ok(),
            }
        }
    }
}

// ── Helpers ────────────────────────────────────────

fn timestamp_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple ISO 8601 UTC
    let (y, m, d, h, min, s) = epoch_to_parts(secs);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}

fn epoch_to_parts(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let s = secs % 60;
    let min = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Howard Hinnant's algorithm
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, h, min, s)
}

// ── COV bridge helpers (Phase 12.0D.WS) ────────────────────

/// Mark `cov_pending = true` on every watch whose `subscribed_ids`
/// contains `point_id`. Returns the number of watches marked.
fn apply_cov_event(watches: &mut HashMap<String, WatchMeta>, point_id: u32) -> usize {
    let mut marked = 0;
    for meta in watches.values_mut() {
        if meta.subscribed_ids.contains(&point_id) {
            meta.cov_pending = true;
            marked += 1;
        }
    }
    marked
}

/// Mark `cov_pending = true` on every watch. Used when the CovEvent
/// broadcast subscriber falls behind — we can't know which specific
/// points we missed, so we force a full resync on next tick.
fn mark_all_cov_pending(watches: &mut HashMap<String, WatchMeta>) {
    for meta in watches.values_mut() {
        meta.cov_pending = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_watch(ids: &[u32]) -> WatchMeta {
        WatchMeta {
            poll_interval: Duration::from_millis(1000),
            last_poll: Instant::now(),
            subscribed_ids: ids.iter().copied().collect(),
            cov_pending: false,
        }
    }

    #[test]
    fn apply_cov_event_marks_matching_watches() {
        let mut watches: HashMap<String, WatchMeta> = HashMap::new();
        watches.insert("w1".into(), fake_watch(&[100, 200]));
        watches.insert("w2".into(), fake_watch(&[200, 300]));
        watches.insert("w3".into(), fake_watch(&[400]));

        let marked = apply_cov_event(&mut watches, 200);
        assert_eq!(marked, 2);
        assert!(watches["w1"].cov_pending);
        assert!(watches["w2"].cov_pending);
        assert!(!watches["w3"].cov_pending);
    }

    #[test]
    fn apply_cov_event_no_match_marks_nothing() {
        let mut watches: HashMap<String, WatchMeta> = HashMap::new();
        watches.insert("w1".into(), fake_watch(&[100]));

        let marked = apply_cov_event(&mut watches, 999);
        assert_eq!(marked, 0);
        assert!(!watches["w1"].cov_pending);
    }

    #[test]
    fn mark_all_cov_pending_flips_every_watch() {
        let mut watches: HashMap<String, WatchMeta> = HashMap::new();
        watches.insert("w1".into(), fake_watch(&[1]));
        watches.insert("w2".into(), fake_watch(&[2]));

        mark_all_cov_pending(&mut watches);
        assert!(watches["w1"].cov_pending);
        assert!(watches["w2"].cov_pending);
    }

    #[test]
    fn epoch_known_date() {
        // 2024-01-01T00:00:00Z = 1704067200
        let (y, m, d, h, min, s) = epoch_to_parts(1_704_067_200);
        assert_eq!((y, m, d, h, min, s), (2024, 1, 1, 0, 0, 0));
    }

    #[test]
    fn epoch_unix_epoch() {
        let (y, m, d, h, min, s) = epoch_to_parts(0);
        assert_eq!((y, m, d, h, min, s), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn epoch_recent_date() {
        // 2026-03-04T12:30:45Z = 1772627445
        let (y, m, d, h, min, s) = epoch_to_parts(1_772_627_445);
        assert_eq!((y, m, d), (2026, 3, 4));
        assert_eq!((h, min, s), (12, 30, 45));
    }

    #[test]
    fn timestamp_now_format() {
        let ts = timestamp_now();
        // Should be ISO 8601 format: YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
    }

    #[test]
    fn server_msg_auth_ok_serializes() {
        let msg = ServerMsg::AuthOk {
            id: Some("1".into()),
            auth_token: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""op":"authOk""#));
        assert!(json.contains(r#""id":"1""#));
    }

    #[test]
    fn server_msg_auth_ok_no_id() {
        let msg = ServerMsg::AuthOk {
            id: None,
            auth_token: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""op":"authOk""#));
        assert!(!json.contains("id"));
    }

    #[test]
    fn server_msg_auth_ok_with_token() {
        let msg = ServerMsg::AuthOk {
            id: Some("1".into()),
            auth_token: Some("session-abc".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""op":"authOk""#));
        assert!(json.contains(r#""authToken":"session-abc""#));
    }

    #[test]
    fn client_msg_hello_deserializes() {
        let json = r#"{"op":"hello","username":"admin"}"#;
        let msg: ClientMsg = serde_json::from_str(json).unwrap();
        match msg {
            ClientMsg::Hello { username, id } => {
                assert_eq!(username, "admin");
                assert!(id.is_none());
            }
            _ => panic!("expected Hello"),
        }
    }

    #[test]
    fn client_msg_authenticate_deserializes() {
        let json = r#"{"op":"authenticate","handshakeToken":"abc","proof":"AAAA"}"#;
        let msg: ClientMsg = serde_json::from_str(json).unwrap();
        match msg {
            ClientMsg::Authenticate {
                handshake_token,
                proof,
                id,
            } => {
                assert_eq!(handshake_token, "abc");
                assert_eq!(proof, "AAAA");
                assert!(id.is_none());
            }
            _ => panic!("expected Authenticate"),
        }
    }

    #[test]
    fn server_msg_challenge_serializes() {
        let msg = ServerMsg::Challenge {
            id: Some("1".into()),
            handshake_token: "tok-123",
            hash: "SHA-256",
            salt: "c2FsdA==",
            iterations: 10000,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""op":"challenge""#));
        assert!(json.contains(r#""handshakeToken":"tok-123""#));
        assert!(json.contains(r#""hash":"SHA-256""#));
        assert!(json.contains(r#""iterations":10000"#));
    }

    #[test]
    fn server_msg_pong_serializes() {
        let msg = ServerMsg::Pong {
            id: Some("42".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""op":"pong""#));
        assert!(json.contains(r#""id":"42""#));
    }

    #[test]
    fn server_msg_error_serializes() {
        let msg = ServerMsg::Error {
            id: None,
            code: "AUTH_REQUIRED",
            message: "authenticate first",
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""op":"error""#));
        assert!(json.contains(r#""code":"AUTH_REQUIRED""#));
        assert!(json.contains(r#""message":"authenticate first""#));
    }

    #[test]
    fn client_msg_auth_deserializes() {
        let json = r#"{"op":"auth","token":"secret"}"#;
        let msg: ClientMsg = serde_json::from_str(json).unwrap();
        match msg {
            ClientMsg::Auth { token, id } => {
                assert_eq!(token, "secret");
                assert!(id.is_none());
            }
            _ => panic!("expected Auth"),
        }
    }

    #[test]
    fn client_msg_subscribe_deserializes() {
        let json = r#"{"op":"subscribe","ids":[1,2,3],"pollInterval":500}"#;
        let msg: ClientMsg = serde_json::from_str(json).unwrap();
        match msg {
            ClientMsg::Subscribe {
                ids, poll_interval, ..
            } => {
                assert_eq!(ids, vec![1, 2, 3]);
                assert_eq!(poll_interval, Some(500));
            }
            _ => panic!("expected Subscribe"),
        }
    }

    #[test]
    fn client_msg_ping_deserializes() {
        let json = r#"{"op":"ping","id":"req-1"}"#;
        let msg: ClientMsg = serde_json::from_str(json).unwrap();
        match msg {
            ClientMsg::Ping { id } => assert_eq!(id.as_deref(), Some("req-1")),
            _ => panic!("expected Ping"),
        }
    }

    #[test]
    fn client_msg_unsubscribe_close() {
        let json = r#"{"op":"unsubscribe","watchId":"w-1","close":true}"#;
        let msg: ClientMsg = serde_json::from_str(json).unwrap();
        match msg {
            ClientMsg::Unsubscribe {
                watch_id, close, ..
            } => {
                assert_eq!(watch_id, "w-1");
                assert!(close);
            }
            _ => panic!("expected Unsubscribe"),
        }
    }

    #[test]
    fn client_msg_refresh_deserializes() {
        let json = r#"{"op":"refresh","watchId":"w-2"}"#;
        let msg: ClientMsg = serde_json::from_str(json).unwrap();
        match msg {
            ClientMsg::Refresh { watch_id, id } => {
                assert_eq!(watch_id, "w-2");
                assert!(id.is_none());
            }
            _ => panic!("expected Refresh"),
        }
    }

    #[test]
    fn server_msg_update_serializes() {
        let rows = vec![super::super::ChannelValue {
            channel: 1100,
            status: "ok".into(),
            raw: 2048.0,
            cur: 72.5,
        }];
        let msg = ServerMsg::Update {
            watch_id: "w-1",
            ts: "2026-03-04T12:00:00Z",
            rows: &rows,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""op":"update""#));
        assert!(json.contains(r#""watchId":"w-1""#));
        assert!(json.contains(r#""channel":1100"#));
    }

    #[test]
    fn server_msg_subscribed_serializes() {
        let rows = vec![];
        let msg = ServerMsg::Subscribed {
            id: Some("req-1".into()),
            watch_id: "w-1",
            lease: 120,
            poll_interval: 1000,
            rows: &rows,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""op":"subscribed""#));
        assert!(json.contains(r#""lease":120"#));
        assert!(json.contains(r#""pollInterval":1000"#));
    }

    #[test]
    fn poll_interval_clamping() {
        // Below minimum
        let clamped = 50u64.clamp(MIN_POLL_INTERVAL_MS, MAX_POLL_INTERVAL_MS);
        assert_eq!(clamped, MIN_POLL_INTERVAL_MS);

        // Above maximum
        let clamped = 100_000u64.clamp(MIN_POLL_INTERVAL_MS, MAX_POLL_INTERVAL_MS);
        assert_eq!(clamped, MAX_POLL_INTERVAL_MS);

        // In range
        let clamped = 2000u64.clamp(MIN_POLL_INTERVAL_MS, MAX_POLL_INTERVAL_MS);
        assert_eq!(clamped, 2000);
    }

    #[test]
    fn invalid_json_returns_error() {
        // Verify that invalid JSON deserializes to error
        let bad = r#"{"not_valid"}"#;
        assert!(serde_json::from_str::<ClientMsg>(bad).is_err());
    }

    #[test]
    fn unknown_op_returns_error() {
        let unknown = r#"{"op":"foobar"}"#;
        assert!(serde_json::from_str::<ClientMsg>(unknown).is_err());
    }
}
