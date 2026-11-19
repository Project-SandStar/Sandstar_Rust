//! Haystack-over-WebSocket: real-time channel value push.
//!
//! Clients connect to `GET /api/ws`, optionally authenticate, subscribe
//! to channels, and receive server-pushed value updates.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

use super::engine_round::EngineRound;
use super::{is_engine_timeout, EngineHandle, WatchResponse, ENGINE_REPLY_TIMEOUT};
use crate::auth::AuthState;
use crate::drivers::actor::DriverHandle;
use crate::drivers::CovEvent;

// ── Constants ──────────────────────────────────────

const MAX_WS_CONNECTIONS: i64 = 32;
const MIN_POLL_INTERVAL_MS: u64 = 200;
const MAX_POLL_INTERVAL_MS: u64 = 60_000;
const DEFAULT_POLL_INTERVAL_MS: u64 = 1000;
const CLIENT_TIMEOUT_SECS: u64 = 120;

/// Pause before the next push round of a connection after an engine
/// timeout, so a hung engine does not get a new (queued) poll every tick.
const PUSH_BACKOFF_AFTER_TIMEOUT: Duration = ENGINE_REPLY_TIMEOUT;

/// Capacity of the shared watch cleanup queue (watch ids). The engine holds
/// at most 64 watches (`cmd_handler.rs` `MAX_WATCHES`).
const CLEANUP_QUEUE_CAPACITY: usize = 256;

/// First pause before retrying a `watch_unsub` that timed out; doubles on
/// each further timeout up to [`CLEANUP_RETRY_MAX_DELAY`].
const CLEANUP_RETRY_DELAY: Duration = Duration::from_secs(10);

/// Longest pause between two cleanup attempts for the same watch.
const CLEANUP_RETRY_MAX_DELAY: Duration = Duration::from_secs(600);

/// Give up on a queued watch after this long: the engine has expired it by
/// then (`cmd_handler.rs` `WATCH_LEASE_SECS`).
const CLEANUP_MAX_AGE: Duration = Duration::from_secs(3600);

/// Minimum gap between two "cleanup queue full" warnings.
const CLEANUP_FULL_WARN_INTERVAL_SECS: u64 = 60;

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
    /// Shared task that closes the watches of finished connections.
    pub cleanup: WatchCleanup,
}

// ── Watch cleanup ──────────────────────────────────

/// Handle to the single task that closes the engine watches of finished
/// connections (R-28, S-22).
///
/// One task for all connections, with at most one `watch_unsub` in flight.
/// A timed-out unsub stays queued in the engine, so each retry can leave one
/// more stale command behind; retries therefore back off exponentially from
/// [`CLEANUP_RETRY_DELAY`] to [`CLEANUP_RETRY_MAX_DELAY`] (about ten attempts
/// per watch in the worst case). The watch is given up after
/// [`CLEANUP_MAX_AGE`], when the engine's lease has expired it anyway. The
/// task ends when every handle is dropped.
#[derive(Clone)]
pub struct WatchCleanup {
    tx: mpsc::Sender<(String, Instant)>,
    origin: Instant,
    /// Seconds since `origin` at the last "queue full" warning, plus one
    /// (0 = never).
    last_full_warn: Arc<AtomicU64>,
}

impl WatchCleanup {
    /// Start the cleanup task. Must be called inside a tokio runtime.
    pub fn spawn(engine: EngineHandle) -> Self {
        Self::spawn_with_capacity(engine, CLEANUP_QUEUE_CAPACITY)
    }

    fn spawn_with_capacity(engine: EngineHandle, capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity);
        tokio::spawn(run_cleanup(engine, rx));
        Self {
            tx,
            origin: Instant::now(),
            last_full_warn: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Queue watches for closing. Never waits; returns how many could not be
    /// queued (those expire with their lease).
    fn enqueue(&self, watch_ids: Vec<String>) -> usize {
        use tokio::sync::mpsc::error::TrySendError;
        let now = Instant::now();
        let (mut dropped, mut closed) = (0usize, 0usize);
        for id in watch_ids {
            match self.tx.try_send((id, now)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => dropped += 1,
                Err(TrySendError::Closed(_)) => closed += 1,
            }
        }
        if closed > 0 {
            // Only possible if the task has ended, which it does not while a
            // handle exists; report it rather than calling it "full".
            warn!(
                count = closed,
                "WS cleanup task is not running; these watches expire with their lease"
            );
        }
        if dropped > 0 {
            let secs = self.origin.elapsed().as_secs().saturating_add(1);
            let last = self.last_full_warn.load(Ordering::Relaxed);
            if (last == 0 || secs.saturating_sub(last) >= CLEANUP_FULL_WARN_INTERVAL_SECS)
                && self
                    .last_full_warn
                    .compare_exchange(last, secs, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                warn!(
                    dropped,
                    "WS cleanup queue full; these watches expire with their lease"
                );
            } else {
                debug!(dropped, "WS cleanup queue full");
            }
        }
        dropped + closed
    }
}

/// Body of the cleanup task: close queued watches one at a time.
async fn run_cleanup(engine: EngineHandle, mut rx: mpsc::Receiver<(String, Instant)>) {
    while let Some((watch_id, queued_at)) = rx.recv().await {
        let mut delay = CLEANUP_RETRY_DELAY;
        loop {
            if queued_at.elapsed() >= CLEANUP_MAX_AGE {
                warn!(
                    watch_id = %watch_id,
                    "WS cleanup: engine never confirmed the close; giving up (the lease has expired the watch)"
                );
                break;
            }
            match engine.watch_unsub(watch_id.clone(), true, vec![]).await {
                Ok(()) => break,
                // Unknown whether the engine ran the command (a timed-out
                // unsub stays queued; a dropped reply may mean it did not).
                // A retry after a successful close answers "not found".
                Err(e) if is_engine_timeout(&e) || e == "engine dropped reply" => {
                    debug!(watch_id = %watch_id, error = %e, retry_in_s = delay.as_secs(), "WS cleanup: retrying");
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2).min(CLEANUP_RETRY_MAX_DELAY);
                }
                // "not found" (already closed or expired), or the engine has
                // stopped: nothing left to clean for this watch.
                Err(e) => {
                    debug!(watch_id = %watch_id, error = %e, "WS cleanup: watch_unsub finished with an error");
                    break;
                }
            }
        }
    }
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
    /// Set after a push poll timed out. That poll stays queued and may still
    /// run, moving the engine's change baseline without the reply reaching
    /// the client, so the next push poll asks for all values (`refresh`).
    resync: bool,
    /// Changes when the client gets a newer full view of this watch
    /// (subscribe, refresh). A push round result taken before that is older
    /// than what the client has, and is dropped (see [`apply_round`]).
    generation: u64,
}

/// A fresh, never reused [`WatchMeta::generation`].
fn next_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// ── Push round ─────────────────────────────────────

/// One watch to poll in a push round.
struct PollJob {
    watch_id: String,
    /// Ask for all values (after a timed-out poll, see [`WatchMeta::resync`]).
    refresh: bool,
    /// [`WatchMeta::generation`] when the job was taken.
    generation: u64,
}

/// The outcome of one [`PollJob`], with the job's watch and generation.
struct RoundItem {
    watch_id: String,
    generation: u64,
    outcome: PollOutcome,
}

/// What happened to one [`PollJob`].
enum PollOutcome {
    Polled(Result<WatchResponse, String>),
    /// Not sent: an earlier poll in the round timed out.
    Skipped,
}

/// Pick the watches due now (interval elapsed or a pending CovEvent) and
/// mark them as polled.
fn due_jobs(watches: &mut HashMap<String, WatchMeta>, now: Instant) -> Vec<PollJob> {
    let mut jobs = Vec::new();
    for (watch_id, meta) in watches.iter_mut() {
        let interval_elapsed = now.duration_since(meta.last_poll) >= meta.poll_interval;
        if !interval_elapsed && !meta.cov_pending {
            continue;
        }
        meta.cov_pending = false;
        meta.last_poll = now;
        jobs.push(PollJob {
            watch_id: watch_id.clone(),
            refresh: meta.resync,
            generation: meta.generation,
        });
    }
    jobs
}

/// Poll the jobs in order, beside the connection's `select!` loop. After
/// the first engine timeout the rest are skipped: each would wait another
/// timeout.
async fn run_push_round(engine: EngineHandle, jobs: Vec<PollJob>) -> Vec<RoundItem> {
    let mut out = Vec::with_capacity(jobs.len());
    let mut timed_out = false;
    for job in jobs {
        let outcome = if timed_out {
            PollOutcome::Skipped
        } else {
            let result = engine.watch_poll(job.watch_id.clone(), job.refresh).await;
            timed_out = matches!(&result, Err(e) if is_engine_timeout(e));
            PollOutcome::Polled(result)
        };
        out.push(RoundItem {
            watch_id: job.watch_id,
            generation: job.generation,
            outcome,
        });
    }
    out
}

/// Apply a finished round to the connection's watches. Returns the
/// responses to push and whether the engine timed out.
fn apply_round(
    watches: &mut HashMap<String, WatchMeta>,
    items: Vec<RoundItem>,
) -> (Vec<WatchResponse>, bool) {
    let mut updates = Vec::new();
    let mut timed_out = false;
    for RoundItem {
        watch_id,
        generation,
        outcome,
    } in items
    {
        // A timeout counts for the back-off even if the watch is gone.
        if let PollOutcome::Polled(Err(e)) = &outcome {
            if is_engine_timeout(e) {
                timed_out = true;
                debug!(watch_id = %watch_id, error = %e, "watch poll timed out in WS push");
                // The poll stays queued and may still run, moving the
                // engine's change baseline without the reply reaching us:
                // resync, and poll again in the next round. This applies to
                // a re-subscribed watch with the same id as well.
                if let Some(meta) = watches.get_mut(&watch_id) {
                    meta.resync = true;
                    meta.cov_pending = true;
                }
                continue;
            }
        }
        // Closed, re-subscribed or refreshed by the client while the round
        // was running: the client already has a newer view.
        let Some(meta) = watches
            .get_mut(&watch_id)
            .filter(|m| m.generation == generation)
        else {
            continue;
        };
        match outcome {
            PollOutcome::Polled(Ok(resp)) => {
                meta.resync = false;
                if !resp.rows.is_empty() {
                    updates.push(resp);
                }
            }
            PollOutcome::Polled(Err(e)) => {
                warn!(watch_id = %watch_id, error = %e, "watch poll failed in WS push");
            }
            // Keep it due for the next round.
            PollOutcome::Skipped => meta.cov_pending = true,
        }
    }
    (updates, timed_out)
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
            state.cleanup,
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
    cleanup: WatchCleanup,
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

    // Watch polls run beside this loop, so pings, client messages and close
    // are handled while the engine is slow (R-28, S-22).
    let mut round: EngineRound<Vec<RoundItem>> = EngineRound::new();

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

                // Start a round for each watch whose interval has elapsed, OR
                // any watch with a pending CovEvent (Phase 12.0D.WS). The
                // result arrives in the arm below.
                if !authenticated || !round.is_idle() { continue; }
                let jobs = due_jobs(&mut watches, Instant::now());
                if !jobs.is_empty() {
                    round.start(run_push_round(engine.clone(), jobs));
                }
            }
            outcomes = round.next() => {
                let (updates, timed_out) = apply_round(&mut watches, outcomes);
                if timed_out {
                    round.back_off(PUSH_BACKOFF_AFTER_TIMEOUT);
                }
                let ts = timestamp_now();
                let mut disconnected = false;
                for resp in &updates {
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
                if disconnected { break; }
            }
        }
    }

    // The connection is gone: release its slot before the (possibly slow)
    // watch cleanup, so a hung engine does not hold connection slots.
    metrics
        .ws_active
        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    let watch_ids: Vec<String> = watches.into_keys().collect();
    info!(
        "WebSocket disconnected (watches to clean: {})",
        watch_ids.len()
    );
    if !watch_ids.is_empty() {
        cleanup.enqueue(watch_ids);
    }
}

/// WS error code for a failed engine command: `ENGINE_TIMEOUT` when the
/// engine did not answer in time (retry later), otherwise `fallback`.
fn engine_error_code(err: &str, fallback: &'static str) -> &'static str {
    if is_engine_timeout(err) {
        "ENGINE_TIMEOUT"
    } else {
        fallback
    }
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
                            resync: false,
                            generation: next_generation(),
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
                    let code = if is_engine_timeout(&e) {
                        "ENGINE_TIMEOUT"
                    } else if e.contains("limit") {
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
                    code: engine_error_code(&e, "WATCH_NOT_FOUND"),
                    message: &e,
                })
                .ok(),
            }
        }
        ClientMsg::Refresh { id, watch_id } => {
            match engine.watch_poll(watch_id.clone(), true).await {
                Ok(resp) => {
                    // The snapshot is newer than any push round still
                    // running for this watch.
                    if let Some(meta) = watches.get_mut(&watch_id) {
                        meta.generation = next_generation();
                        meta.resync = false;
                    }
                    let ts = timestamp_now();
                    serde_json::to_string(&ServerMsg::Snapshot {
                        id,
                        watch_id: &resp.watch_id,
                        ts: &ts,
                        rows: &resp.rows,
                    })
                    .ok()
                }
                Err(e) => {
                    // A timed-out refresh may still run and move the change
                    // baseline: the next push poll asks for all values.
                    if is_engine_timeout(&e) {
                        if let Some(meta) = watches.get_mut(&watch_id) {
                            meta.resync = true;
                        }
                    }
                    serde_json::to_string(&ServerMsg::Error {
                        id,
                        code: engine_error_code(&e, "WATCH_NOT_FOUND"),
                        message: &e,
                    })
                    .ok()
                }
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

    /// Engine that takes commands but never answers.
    fn silent_engine() -> EngineHandle {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<super::super::EngineCmd>(16);
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Some(cmd) = rx.recv().await {
                held.push(cmd);
            }
        });
        EngineHandle::new(tx)
    }

    async fn reply_code(text: &str, engine: &EngineHandle) -> String {
        let mut authenticated = true;
        let mut watches = HashMap::new();
        let reply = handle_client_msg(text, engine, &None, &None, &mut authenticated, &mut watches)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&reply).unwrap();
        v["code"].as_str().unwrap_or_default().to_string()
    }

    #[tokio::test(start_paused = true)]
    async fn engine_timeout_is_reported_as_engine_timeout() {
        let engine = silent_engine();
        let sub = r#"{"op":"subscribe","id":"1","ids":[1]}"#;
        let unsub = r#"{"op":"unsubscribe","id":"2","watchId":"w","ids":[],"close":true}"#;
        let refresh = r#"{"op":"refresh","id":"3","watchId":"w"}"#;
        for msg in [sub, unsub, refresh] {
            assert_eq!(reply_code(msg, &engine).await, "ENGINE_TIMEOUT", "{msg}");
        }
    }

    #[test]
    fn engine_error_code_keeps_fallback_for_other_errors() {
        assert_eq!(
            engine_error_code("watch w not found", "WATCH_NOT_FOUND"),
            "WATCH_NOT_FOUND"
        );
        assert_eq!(
            engine_error_code("engine stopped", "WATCH_NOT_FOUND"),
            "WATCH_NOT_FOUND"
        );
        assert_eq!(
            engine_error_code(
                "engine timeout; command may still be applied",
                "WATCH_NOT_FOUND"
            ),
            "ENGINE_TIMEOUT"
        );
    }

    use super::super::{ChannelValue, EngineCmd};
    use std::sync::atomic::AtomicUsize;

    /// Engine that hands the n-th command (0-based) to `answer`. `answer`
    /// replies and returns `None`, or returns the command to hold it
    /// unanswered (a hung engine). Also counts the commands.
    fn scripted_engine<F>(answer: F) -> (EngineHandle, Arc<AtomicUsize>)
    where
        F: Fn(usize, EngineCmd) -> Option<EngineCmd> + Send + 'static,
    {
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<EngineCmd>(16);
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Some(cmd) = rx.recv().await {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if let Some(cmd) = answer(n, cmd) {
                    held.push(cmd);
                }
            }
        });
        (EngineHandle::new(tx), seen)
    }

    fn one_row(watch_id: String) -> WatchResponse {
        WatchResponse {
            watch_id,
            lease: 3600,
            rows: vec![ChannelValue {
                channel: 1,
                status: "ok".into(),
                raw: 1.0,
                cur: 1.0,
            }],
        }
    }

    #[tokio::test(start_paused = true)]
    async fn push_round_resyncs_after_a_timeout() {
        let (flags_tx, mut flags_rx) = mpsc::channel::<bool>(8);
        let (engine, _) = scripted_engine(move |n, cmd| match cmd {
            EngineCmd::WatchPoll {
                watch_id,
                refresh,
                reply,
            } if n > 0 => {
                let _ = flags_tx.try_send(refresh);
                let _ = reply.send(Ok(one_row(watch_id)));
                None
            }
            // First poll: the engine hangs.
            other => Some(other),
        });
        let mut watches = HashMap::new();
        watches.insert("w".to_string(), fake_watch(&[1]));

        // Round 1 times out: resync is set and the caller is told.
        watches.get_mut("w").unwrap().cov_pending = true;
        let jobs = due_jobs(&mut watches, Instant::now());
        assert_eq!(jobs.len(), 1);
        assert!(!jobs[0].refresh);
        let (updates, timed_out) =
            apply_round(&mut watches, run_push_round(engine.clone(), jobs).await);
        assert!(updates.is_empty());
        assert!(timed_out);
        assert!(watches["w"].resync);

        // Round 2 asks for all values and clears resync.
        watches.get_mut("w").unwrap().cov_pending = true;
        let jobs = due_jobs(&mut watches, Instant::now());
        let (updates, timed_out) =
            apply_round(&mut watches, run_push_round(engine.clone(), jobs).await);
        assert!(
            flags_rx.recv().await.unwrap(),
            "poll after a timeout must refresh"
        );
        assert_eq!(updates.len(), 1);
        assert!(!timed_out);
        assert!(!watches["w"].resync);

        // Round 3 is a delta poll again.
        watches.get_mut("w").unwrap().cov_pending = true;
        let jobs = due_jobs(&mut watches, Instant::now());
        apply_round(&mut watches, run_push_round(engine, jobs).await);
        assert!(
            !flags_rx.recv().await.unwrap(),
            "later polls are deltas again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn push_round_skips_after_a_timeout_and_keeps_skipped_due() {
        let (engine, seen) = scripted_engine(|_, cmd| Some(cmd));
        let mut watches = HashMap::new();
        watches.insert("a".to_string(), fake_watch(&[1]));
        watches.insert("b".to_string(), fake_watch(&[2]));
        let jobs = vec![
            PollJob {
                watch_id: "a".into(),
                refresh: false,
                generation: watches["a"].generation,
            },
            PollJob {
                watch_id: "b".into(),
                refresh: false,
                generation: watches["b"].generation,
            },
        ];
        let outcomes = run_push_round(engine, jobs).await;
        // One engine command for the whole round.
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert!(matches!(outcomes[1].outcome, PollOutcome::Skipped));
        let (_, timed_out) = apply_round(&mut watches, outcomes);
        assert!(timed_out);
        assert!(watches["a"].resync);
        assert!(
            watches["a"].cov_pending,
            "timed-out watch is polled again next round"
        );
        assert!(!watches["b"].resync);
        assert!(watches["b"].cov_pending, "skipped watch stays due");
    }

    fn item(watch_id: &str, generation: u64, outcome: PollOutcome) -> RoundItem {
        RoundItem {
            watch_id: watch_id.into(),
            generation,
            outcome,
        }
    }

    #[test]
    fn apply_round_backs_off_even_if_the_timed_out_watch_was_closed() {
        let mut watches = HashMap::new();
        watches.insert("b".to_string(), fake_watch(&[2]));
        let gen_b = watches["b"].generation;
        let items = vec![
            item(
                "a",
                1,
                PollOutcome::Polled(Err(super::super::ENGINE_TIMEOUT_ERR.into())),
            ),
            item("b", gen_b, PollOutcome::Skipped),
        ];
        let (updates, timed_out) = apply_round(&mut watches, items);
        assert!(updates.is_empty());
        assert!(
            timed_out,
            "the back-off must not depend on the closed watch"
        );
        assert!(watches["b"].cov_pending);
    }

    #[test]
    fn apply_round_drops_a_result_older_than_the_clients_view() {
        let mut watches = HashMap::new();
        watches.insert("w".to_string(), fake_watch(&[1]));
        let old = watches["w"].generation;
        // The client refreshed (or re-subscribed) while the round ran.
        watches.get_mut("w").unwrap().generation = next_generation();
        let items = vec![item("w", old, PollOutcome::Polled(Ok(one_row("w".into()))))];
        let (updates, _) = apply_round(&mut watches, items);
        assert!(
            updates.is_empty(),
            "a stale update must not follow a newer snapshot"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_timeout_sets_resync() {
        let (engine, _) = scripted_engine(|_, cmd| Some(cmd));
        let mut watches = HashMap::new();
        watches.insert("w".to_string(), fake_watch(&[1]));
        let mut authenticated = true;
        let reply = handle_client_msg(
            r#"{"op":"refresh","id":"r","watchId":"w"}"#,
            &engine,
            &None,
            &None,
            &mut authenticated,
            &mut watches,
        )
        .await
        .unwrap();
        assert!(reply.contains("ENGINE_TIMEOUT"), "{reply}");
        assert!(watches["w"].resync);
    }

    #[tokio::test]
    async fn refresh_success_starts_a_new_generation() {
        let (engine, _) = scripted_engine(|_, cmd| match cmd {
            EngineCmd::WatchPoll {
                watch_id, reply, ..
            } => {
                let _ = reply.send(Ok(one_row(watch_id)));
                None
            }
            other => Some(other),
        });
        let mut watches = HashMap::new();
        watches.insert("w".to_string(), fake_watch(&[1]));
        let before = watches["w"].generation;
        let mut authenticated = true;
        handle_client_msg(
            r#"{"op":"refresh","id":"r","watchId":"w"}"#,
            &engine,
            &None,
            &None,
            &mut authenticated,
            &mut watches,
        )
        .await
        .unwrap();
        assert_ne!(watches["w"].generation, before);
    }

    #[test]
    fn apply_round_ignores_a_watch_closed_during_the_round() {
        let mut watches: HashMap<String, WatchMeta> = HashMap::new();
        let outcomes = vec![item(
            "gone",
            1,
            PollOutcome::Polled(Ok(one_row("gone".to_string()))),
        )];
        let (updates, timed_out) = apply_round(&mut watches, outcomes);
        assert!(updates.is_empty());
        assert!(!timed_out);
        assert!(watches.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn round_runs_beside_the_loop() {
        // While a round waits on a hung engine, other select! arms still run.
        let (engine, _) = scripted_engine(|_, cmd| Some(cmd));
        let mut round: EngineRound<Vec<RoundItem>> = EngineRound::new();
        round.start(run_push_round(
            engine,
            vec![PollJob {
                watch_id: "w".into(),
                refresh: false,
                generation: 1,
            }],
        ));
        let start = Instant::now();
        tokio::select! {
            _ = round.next() => panic!("round must still be waiting"),
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        assert_eq!(start.elapsed(), Duration::from_millis(200));
        // The round then ends with the engine timeout.
        let outcomes = round.next().await;
        assert!(
            matches!(&outcomes[0].outcome, PollOutcome::Polled(Err(e)) if is_engine_timeout(e))
        );
    }

    /// Wait (in paused time) until `seen` reaches `n`, at most `limit`.
    async fn wait_for(seen: &AtomicUsize, n: usize, limit: Duration) {
        let start = Instant::now();
        while seen.load(Ordering::SeqCst) < n && start.elapsed() < limit {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_retries_a_timed_out_unsub() {
        let (engine, seen) = scripted_engine(|n, cmd| match cmd {
            EngineCmd::WatchUnsub { reply, .. } if n > 0 => {
                let _ = reply.send(Ok(()));
                None
            }
            other => Some(other),
        });
        let cleanup = WatchCleanup::spawn(engine);
        assert_eq!(cleanup.enqueue(vec!["a".into()]), 0);
        wait_for(&seen, 2, Duration::from_secs(120)).await;
        assert_eq!(seen.load(Ordering::SeqCst), 2, "one timeout, one retry");
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(
            seen.load(Ordering::SeqCst),
            2,
            "no more retries after success"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_treats_not_found_as_done() {
        let (engine, seen) = scripted_engine(|_, cmd| match cmd {
            EngineCmd::WatchUnsub {
                watch_id, reply, ..
            } => {
                let _ = reply.send(Err(format!("watch {watch_id} not found")));
                None
            }
            other => Some(other),
        });
        let cleanup = WatchCleanup::spawn(engine);
        cleanup.enqueue(vec!["a".into(), "b".into()]);
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(seen.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_gives_up_after_the_lease() {
        let (engine, seen) = scripted_engine(|_, cmd| Some(cmd));
        let cleanup = WatchCleanup::spawn(engine);
        cleanup.enqueue(vec!["a".into()]);
        tokio::time::sleep(CLEANUP_MAX_AGE + Duration::from_secs(60)).await;
        let attempts = seen.load(Ordering::SeqCst);
        // Exponential back-off: 10, 20, 40, ... 600 s between attempts, so
        // about ten attempts within the hour (not one every 20 s), then stop.
        assert!((5..=12).contains(&attempts), "attempts = {attempts}");
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert_eq!(seen.load(Ordering::SeqCst), attempts);
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_queue_full_reports_dropped_watches() {
        let (engine, _) = scripted_engine(|_, cmd| Some(cmd));
        let cleanup = WatchCleanup::spawn_with_capacity(engine, 1);
        let dropped = cleanup.enqueue(vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(dropped, 2);
    }

    fn fake_watch(ids: &[u32]) -> WatchMeta {
        WatchMeta {
            poll_interval: Duration::from_millis(1000),
            last_poll: Instant::now(),
            subscribed_ids: ids.iter().copied().collect(),
            cov_pending: false,
            resync: false,
            generation: next_generation(),
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
