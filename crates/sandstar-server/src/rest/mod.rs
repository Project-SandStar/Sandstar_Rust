//! Haystack REST API built on Axum.
//!
//! Uses the command-channel pattern: REST handlers send [`EngineCmd`] messages
//! through an mpsc channel to the main `select!` loop, which executes them
//! against the engine and replies via oneshot.
//!
//! `EngineHandle` is `Clone + Send + Sync`, so it works as Axum `State`
//! without requiring `Rc<RefCell<>>` or `Arc<Mutex<>>`.

mod error;
pub mod filter;
pub mod handlers;
pub mod rows;
#[cfg(feature = "simulator-hal")]
pub mod sim;
pub mod sox_api;
pub mod tags;
pub mod trio;
mod types;
pub mod ws;
pub mod zinc_format;
pub mod zinc_grid;

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{DefaultBodyLimit, Json, Request, State};
use axum::http::{HeaderName, Method, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::{mpsc, oneshot};
use tower_http::cors::CorsLayer;

use crate::auth::AuthState;
use crate::history::HistoryPoint;
use sandstar_ipc::types::{ChannelInfo, PollInfo, StatusInfo};

// ── Engine command channel ───────────────────────────────────

/// Commands sent from REST handlers to the engine main loop.
pub enum EngineCmd {
    Status {
        reply: oneshot::Sender<StatusInfo>,
    },
    ListChannels {
        reply: oneshot::Sender<Vec<ChannelInfo>>,
    },
    ListPolls {
        reply: oneshot::Sender<Vec<PollInfo>>,
    },
    ListTables {
        reply: oneshot::Sender<Vec<String>>,
    },
    ReadChannel {
        channel: u32,
        reply: oneshot::Sender<Result<ChannelValue, String>>,
    },
    WriteChannel {
        channel: u32,
        value: Option<f64>,
        level: u8,
        who: String,
        duration: f64,
        reply: oneshot::Sender<Result<(), String>>,
    },
    GetWriteLevels {
        channel: u32,
        reply: oneshot::Sender<Result<Vec<sandstar_ipc::types::WriteLevelInfo>, String>>,
    },
    PollNow {
        reply: oneshot::Sender<Result<String, String>>,
    },
    ReloadConfig {
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Server boot time + current time (for /api/about).
    AboutInfo {
        reply: oneshot::Sender<(u64, u64)>, // (boot_epoch_secs, current_epoch_secs)
    },
    /// Create or extend a watch subscription.
    WatchSub {
        watch_id: Option<String>,
        display_name: Option<String>,
        channels: Vec<u32>,
        reply: oneshot::Sender<Result<WatchResponse, String>>,
    },
    /// Unsubscribe channels or close a watch.
    WatchUnsub {
        watch_id: String,
        close: bool,
        channels: Vec<u32>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Poll for changed values since last poll.
    WatchPoll {
        watch_id: String,
        refresh: bool,
        reply: oneshot::Sender<Result<WatchResponse, String>>,
    },
    /// Query channel history from the in-memory ring buffer.
    GetHistory {
        channel: u32,
        since_unix: u64,
        limit: usize,
        reply: oneshot::Sender<Vec<HistoryPoint>>,
    },
    /// Get engine diagnostics (poll timing, channel health, I2C backoff).
    Diagnostics {
        reply: oneshot::Sender<sandstar_ipc::types::DiagnosticsInfo>,
    },
    Shutdown,
}

/// A single channel value reading (returned by ReadChannel).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChannelValue {
    pub channel: u32,
    pub status: String,
    pub raw: f64,
    pub cur: f64,
}

/// Watch subscription response (returned by WatchSub / WatchPoll).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchResponse {
    pub watch_id: String,
    pub lease: u32,
    pub rows: Vec<ChannelValue>,
}

/// Cloneable, Send+Sync handle to the engine command channel.
///
/// Passed to Axum as `State<EngineHandle>`. Each handler method
/// sends a command and awaits the oneshot reply.
#[derive(Clone)]
pub struct EngineHandle {
    tx: mpsc::Sender<EngineCmd>,
}

impl EngineHandle {
    pub fn new(tx: mpsc::Sender<EngineCmd>) -> Self {
        Self { tx }
    }

    pub async fn status(&self) -> Result<StatusInfo, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(EngineCmd::Status { reply })
            .await
            .map_err(|_| "engine stopped".to_string())?;
        rx.await.map_err(|_| "engine dropped reply".to_string())
    }

    pub async fn list_channels(&self) -> Result<Vec<ChannelInfo>, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(EngineCmd::ListChannels { reply })
            .await
            .map_err(|_| "engine stopped".to_string())?;
        rx.await.map_err(|_| "engine dropped reply".to_string())
    }

    pub async fn list_polls(&self) -> Result<Vec<PollInfo>, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(EngineCmd::ListPolls { reply })
            .await
            .map_err(|_| "engine stopped".to_string())?;
        rx.await.map_err(|_| "engine dropped reply".to_string())
    }

    pub async fn list_tables(&self) -> Result<Vec<String>, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(EngineCmd::ListTables { reply })
            .await
            .map_err(|_| "engine stopped".to_string())?;
        rx.await.map_err(|_| "engine dropped reply".to_string())
    }

    pub async fn read_channel(&self, channel: u32) -> Result<ChannelValue, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(EngineCmd::ReadChannel { channel, reply })
            .await
            .map_err(|_| "engine stopped".to_string())?;
        rx.await.map_err(|_| "engine dropped reply".to_string())?
    }

    pub async fn write_channel(
        &self,
        channel: u32,
        value: Option<f64>,
        level: u8,
        who: String,
        duration: f64,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(EngineCmd::WriteChannel {
                channel,
                value,
                level,
                who,
                duration,
                reply,
            })
            .await
            .map_err(|_| "engine stopped".to_string())?;
        rx.await.map_err(|_| "engine dropped reply".to_string())?
    }

    pub async fn get_write_levels(
        &self,
        channel: u32,
    ) -> Result<Vec<sandstar_ipc::types::WriteLevelInfo>, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(EngineCmd::GetWriteLevels { channel, reply })
            .await
            .map_err(|_| "engine stopped".to_string())?;
        rx.await.map_err(|_| "engine dropped reply".to_string())?
    }

    pub async fn poll_now(&self) -> Result<String, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(EngineCmd::PollNow { reply })
            .await
            .map_err(|_| "engine stopped".to_string())?;
        rx.await.map_err(|_| "engine dropped reply".to_string())?
    }

    pub async fn reload_config(&self) -> Result<String, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(EngineCmd::ReloadConfig { reply })
            .await
            .map_err(|_| "engine stopped".to_string())?;
        rx.await.map_err(|_| "engine dropped reply".to_string())?
    }

    pub async fn about_info(&self) -> Result<(u64, u64), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(EngineCmd::AboutInfo { reply })
            .await
            .map_err(|_| "engine stopped".to_string())?;
        rx.await.map_err(|_| "engine dropped reply".to_string())
    }

    pub async fn watch_sub(
        &self,
        watch_id: Option<String>,
        display_name: Option<String>,
        channels: Vec<u32>,
    ) -> Result<WatchResponse, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(EngineCmd::WatchSub {
                watch_id,
                display_name,
                channels,
                reply,
            })
            .await
            .map_err(|_| "engine stopped".to_string())?;
        rx.await.map_err(|_| "engine dropped reply".to_string())?
    }

    pub async fn watch_unsub(
        &self,
        watch_id: String,
        close: bool,
        channels: Vec<u32>,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(EngineCmd::WatchUnsub {
                watch_id,
                close,
                channels,
                reply,
            })
            .await
            .map_err(|_| "engine stopped".to_string())?;
        rx.await.map_err(|_| "engine dropped reply".to_string())?
    }

    pub async fn watch_poll(
        &self,
        watch_id: String,
        refresh: bool,
    ) -> Result<WatchResponse, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(EngineCmd::WatchPoll {
                watch_id,
                refresh,
                reply,
            })
            .await
            .map_err(|_| "engine stopped".to_string())?;
        rx.await.map_err(|_| "engine dropped reply".to_string())?
    }

    pub async fn diagnostics(&self) -> Result<sandstar_ipc::types::DiagnosticsInfo, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(EngineCmd::Diagnostics { reply })
            .await
            .map_err(|_| "engine stopped".to_string())?;
        rx.await.map_err(|_| "engine dropped reply".to_string())
    }

    pub async fn get_history(
        &self,
        channel: u32,
        since_unix: u64,
        limit: usize,
    ) -> Result<Vec<HistoryPoint>, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(EngineCmd::GetHistory {
                channel,
                since_unix,
                limit,
                reply,
            })
            .await
            .map_err(|_| "engine stopped".to_string())?;
        rx.await.map_err(|_| "engine dropped reply".to_string())
    }
}

// ── Router construction ──────────────────────────────────────

/// Middleware that increments the REST request counter on every request.
async fn count_requests(request: Request, next: middleware::Next) -> Response {
    crate::metrics::metrics()
        .rest_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    next.run(request).await
}

/// Middleware that checks auth on protected (mutating) routes.
///
/// Supports three auth methods (checked in order):
/// 1. **No auth configured** -- pass through (current behavior)
/// 2. **Bearer token** -- `Authorization: Bearer <token>` (legacy or session token)
/// 3. **SCRAM initiation** -- returns 401 with instructions (client should use /api/auth)
async fn check_auth(
    axum::extract::State(auth_state): axum::extract::State<AuthState>,
    request: Request,
    next: middleware::Next,
) -> Result<Response, StatusCode> {
    if !auth_state.store.is_auth_required() {
        return Ok(next.run(request).await);
    }

    let auth_header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Check Bearer token (legacy bearer or session token from SCRAM)
    if let Some(bearer) = auth_header.strip_prefix("Bearer ") {
        if auth_state.check_token(bearer) {
            return Ok(next.run(request).await);
        }
    }

    Err(StatusCode::UNAUTHORIZED)
}

// ── SCRAM Auth Endpoint ─────────────────────────────────────

/// JSON request/response types for the /api/auth SCRAM endpoint.
#[derive(serde::Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
enum AuthRequest {
    /// Step 1: Client sends hello with username.
    Hello { username: String },
    /// Step 2: Client sends client-first-message (SCRAM).
    #[serde(rename_all = "camelCase")]
    ScramFirst {
        data: String, // base64(client-first-message)
    },
    /// Step 3: Client sends client-final-message.
    #[serde(rename_all = "camelCase")]
    ScramFinal {
        handshake_token: String,
        data: String, // base64(client-final-message)
    },
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthChallengeResponse {
    handshake_token: String,
    hash: &'static str,
    salt: String, // base64
    iterations: u32,
    data: String, // base64(server-first-message)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthTokenResponse {
    auth_token: String,
    data: String, // base64(server-final/server-signature)
}

#[derive(serde::Serialize)]
struct AuthErrorResponse {
    error: String,
}

/// POST /api/auth -- SCRAM-SHA-256 authentication endpoint.
///
/// This endpoint handles the multi-step SCRAM handshake via JSON POST.
async fn auth_endpoint(
    State(auth_state): State<AuthState>,
    Json(req): Json<AuthRequest>,
) -> Response {
    match req {
        AuthRequest::Hello { username } => handle_auth_hello(&auth_state, &username),
        AuthRequest::ScramFirst { data } => handle_scram_first(&auth_state, &data),
        AuthRequest::ScramFinal {
            handshake_token,
            data,
        } => handle_scram_final(&auth_state, &handshake_token, &data),
    }
}

fn handle_auth_hello(auth_state: &AuthState, username: &str) -> Response {
    // Look up the user's salt and iterations (for the client to derive keys)
    match auth_state.store.get_credential(username) {
        Some(cred) => {
            let salt_b64 =
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &cred.salt);
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "hash": "SHA-256",
                    "salt": salt_b64,
                    "iterations": cred.iterations,
                })),
            )
                .into_response()
        }
        None => (
            StatusCode::UNAUTHORIZED,
            Json(AuthErrorResponse {
                error: "unknown user".to_string(),
            }),
        )
            .into_response(),
    }
}

fn handle_scram_first(auth_state: &AuthState, data_b64: &str) -> Response {
    let client_first =
        match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(AuthErrorResponse {
                            error: "invalid UTF-8 in client-first".to_string(),
                        }),
                    )
                        .into_response()
                }
            },
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(AuthErrorResponse {
                        error: "invalid base64".to_string(),
                    }),
                )
                    .into_response()
            }
        };

    match auth_state.begin_scram(&client_first) {
        Ok((hs_token, server_first)) => {
            // Parse server-first to extract salt and iterations for the response
            let mut salt_b64 = String::new();
            let mut iterations = 0u32;
            for part in server_first.split(',') {
                if let Some(s) = part.strip_prefix("s=") {
                    salt_b64 = s.to_string();
                } else if let Some(i) = part.strip_prefix("i=") {
                    iterations = i.parse().unwrap_or(0);
                }
            }
            let server_first_b64 = base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                server_first.as_bytes(),
            );
            (
                StatusCode::UNAUTHORIZED,
                Json(AuthChallengeResponse {
                    handshake_token: hs_token,
                    hash: "SHA-256",
                    salt: salt_b64,
                    iterations,
                    data: server_first_b64,
                }),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::UNAUTHORIZED,
            Json(AuthErrorResponse { error: e }),
        )
            .into_response(),
    }
}

fn handle_scram_final(auth_state: &AuthState, handshake_token: &str, data_b64: &str) -> Response {
    let client_final =
        match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(AuthErrorResponse {
                            error: "invalid UTF-8 in client-final".to_string(),
                        }),
                    )
                        .into_response()
                }
            },
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(AuthErrorResponse {
                        error: "invalid base64".to_string(),
                    }),
                )
                    .into_response()
            }
        };

    match auth_state.complete_scram(handshake_token, &client_final) {
        Ok((session_token, server_sig)) => (
            StatusCode::OK,
            Json(AuthTokenResponse {
                auth_token: session_token,
                data: server_sig,
            }),
        )
            .into_response(),
        Err(e) => (
            StatusCode::UNAUTHORIZED,
            Json(AuthErrorResponse { error: e }),
        )
            .into_response(),
    }
}

// ── Rate limiting ────────────────────────────────────────────

/// Simple sliding-window rate limiter using atomics.
///
/// Tracks request count within a 1-second window. When the window
/// expires, the counter resets. This is a global (not per-IP) limiter,
/// suitable for an embedded device with a single API.
pub struct RateLimiter {
    /// Number of requests seen in the current window.
    count: AtomicU64,
    /// Start of the current window (epoch milliseconds).
    window_start: AtomicI64,
    /// Maximum requests allowed per 1-second window.
    max_per_second: u64,
}

impl RateLimiter {
    /// Create a new rate limiter with the given requests-per-second cap.
    pub fn new(max_per_second: u64) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        Self {
            count: AtomicU64::new(0),
            window_start: AtomicI64::new(now),
            max_per_second,
        }
    }

    /// Check whether a request should be allowed.
    ///
    /// Returns `true` if the request is within the rate limit.
    /// If the 1-second window has elapsed, resets the counter.
    ///
    /// Note: There is a small race window when resetting the counter,
    /// but on an embedded device with modest concurrency this is
    /// acceptable. The worst case is allowing a few extra requests
    /// at the window boundary.
    pub fn check(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let window = self.window_start.load(Ordering::Relaxed);
        if now - window >= 1000 {
            // New window — reset counter. Small race is OK (see doc comment).
            self.window_start.store(now, Ordering::Relaxed);
            self.count.store(1, Ordering::Relaxed);
            true
        } else {
            let prev = self.count.fetch_add(1, Ordering::Relaxed);
            prev < self.max_per_second
        }
    }
}

/// Axum middleware that enforces the rate limit.
///
/// Returns 429 Too Many Requests when the limit is exceeded.
async fn rate_limit_middleware(
    State(limiter): State<Arc<RateLimiter>>,
    request: Request,
    next: middleware::Next,
) -> Result<Response, StatusCode> {
    if !limiter.check() {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    Ok(next.run(request).await)
}

/// Embedded web dashboard served at GET /.
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Embedded DDC editor placeholder served at GET /editor.
const EDITOR_HTML: &str = include_str!("editor.html");

async fn dashboard() -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        DASHBOARD_HTML,
    )
        .into_response()
}

async fn editor() -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        EDITOR_HTML,
    )
        .into_response()
}

/// Build the Axum router for the Haystack REST API.
///
/// `rate_limit` is the maximum requests per second (0 = unlimited).
/// Accepts `AuthState` for SCRAM + bearer auth, or pass `AuthState::new(AuthStore::new())` for no auth.
///
/// **Backward compat**: `router_legacy` wraps this for callers that still use `Option<String>`.
pub fn router(handle: EngineHandle, auth_token: Option<String>, rate_limit: u64) -> Router {
    use crate::auth::AuthStore;
    let auth_store = match auth_token {
        Some(ref t) => AuthStore::with_bearer_token(t.clone()),
        None => AuthStore::new(),
    };
    let auth_state = AuthState::new(auth_store);
    router_with_auth(handle, auth_state, auth_token, rate_limit, None)
}

/// Build the router with full `AuthState` (SCRAM + bearer + sessions).
///
/// If `sox_state` is provided, SOX API endpoints are mounted under `/api/sox/*`
/// and the editor page is available at `/editor`.
pub fn router_with_auth(
    handle: EngineHandle,
    auth_state: AuthState,
    auth_token: Option<String>,
    rate_limit: u64,
    sox_state: Option<sox_api::SoxApiState>,
) -> Router {
    // Public read-only routes (no auth required)
    let public = Router::new()
        .route("/", get(dashboard))
        .route("/editor", get(editor))
        .route("/api/about", get(handlers::about))
        .route("/api/ops", get(handlers::ops))
        .route("/api/formats", get(handlers::formats))
        .route("/api/read", get(handlers::read))
        .route("/api/status", get(handlers::status))
        .route("/health", get(handlers::health))
        .route("/api/metrics", get(handlers::metrics_endpoint))
        .route("/api/diagnostics", get(handlers::diagnostics))
        .route("/api/channels", get(handlers::channels))
        .route("/api/polls", get(handlers::polls))
        .route("/api/tables", get(handlers::tables))
        .route("/api/pointWrite", get(handlers::point_write_read))
        .route("/api/history/{channel}", get(handlers::history))
        .with_state(handle.clone());

    // SCRAM auth endpoint (public — handles its own auth flow)
    let auth_route = Router::new()
        .route("/api/auth", post(auth_endpoint))
        .with_state(auth_state.clone());

    // WebSocket route (handles its own auth via query param, message, or SCRAM)
    let ws_route = Router::new()
        .route("/api/ws", get(ws::ws_upgrade))
        .with_state(ws::WsState {
            engine: handle.clone(),
            auth_token,
            auth_state: Some(auth_state.clone()),
        });

    // Protected mutating routes (auth required if token/SCRAM is configured)
    let protected = Router::new()
        .route("/api/pointWrite", post(handlers::point_write))
        .route("/api/pollNow", post(handlers::poll_now))
        .route("/api/reload", post(handlers::reload))
        .route("/api/watchSub", post(handlers::watch_sub))
        .route("/api/watchUnsub", post(handlers::watch_unsub))
        .route("/api/watchPoll", post(handlers::watch_poll))
        .route("/api/hisRead", post(handlers::his_read))
        .route("/api/nav", post(handlers::nav))
        .route("/api/invokeAction", post(handlers::invoke_action))
        .route_layer(middleware::from_fn_with_state(auth_state, check_auth))
        .with_state(handle.clone());

    // CORS policy: allow any origin (embedded device accessed by various IPs)
    // but restrict methods and headers to only what we actually use.
    // This is safer than CorsLayer::permissive() which allows arbitrary
    // headers and methods.
    let cors = CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            HeaderName::from_static("content-type"),
            HeaderName::from_static("authorization"),
            HeaderName::from_static("accept"),
        ])
        .max_age(Duration::from_secs(3600));

    // Driver framework REST endpoints — backed by the async DriverHandle actor.
    // BACnet drivers (and future async drivers) are loaded from environment config
    // asynchronously in the background so they don't block router construction.
    let driver_handle = crate::drivers::actor::spawn_driver_actor(64);
    {
        let driver_handle_clone = driver_handle.clone();
        let engine_handle_clone = handle.clone();
        tokio::spawn(async move {
            load_bacnet_drivers(&driver_handle_clone, &engine_handle_clone).await;
        });
    }
    {
        let driver_handle_clone = driver_handle.clone();
        let engine_handle_clone = handle.clone();
        tokio::spawn(async move {
            load_mqtt_drivers(&driver_handle_clone, &engine_handle_clone).await;
        });
    }
    let driver_routes = crate::drivers::driver_router(driver_handle);

    // Merge and apply global middleware.
    // Layer order (axum applies bottom-up): CORS → body limit → rate limit → count.
    let mut app = public
        .merge(protected)
        .merge(ws_route)
        .merge(auth_route)
        .merge(driver_routes);

    // SOX API endpoints (component tree REST interface for DDC visual editor).
    // RoWS WebSocket endpoint (real-time component tree operations + COV push).
    if let Some(sox) = sox_state {
        let rows_route = Router::new()
            .route("/api/rows", get(rows::rows_ws_handler))
            .with_state(rows::RowsState {
                tree: sox.tree.clone(),
                manifest_db: sox.manifest_db.clone(),
                dyn_store: sox.dyn_store.clone(),
            });
        // Make the DynSlotStore available as an Extension so that the
        // Haystack filter evaluator in /api/read can check dynamic tags.
        if let Some(ref ds) = sox.dyn_store {
            app = app.layer(axum::Extension(ds.clone()));
        }
        app = app
            .merge(sox_api::public_router(sox.clone()))
            .merge(sox_api::protected_router(sox))
            .merge(rows_route);
    }

    let mut app = app.layer(middleware::from_fn(count_requests));

    // Apply rate limiting only when configured (rate_limit > 0).
    if rate_limit > 0 {
        let limiter = Arc::new(RateLimiter::new(rate_limit));
        app = app.layer(middleware::from_fn_with_state(
            limiter,
            rate_limit_middleware,
        ));
    }

    app.layer(DefaultBodyLimit::max(1_048_576)).layer(cors)
}

// ── BACnet driver loader ─────────────────────────────────────

/// Load BACnet drivers from the `SANDSTAR_BACNET_CONFIGS` environment variable.
///
/// The variable should be a JSON array of `BacnetConfig` objects, e.g.:
/// ```json
/// [{"id":"bac-1","port":47808,"broadcast":"255.255.255.255","objects":[]}]
/// ```
///
/// Errors are logged but do not prevent server startup.
async fn load_bacnet_drivers(
    handle: &crate::drivers::actor::DriverHandle,
    engine_handle: &EngineHandle,
) {
    let json_str = match std::env::var("SANDSTAR_BACNET_CONFIGS") {
        Ok(s) => s,
        Err(_) => return, // Not configured — skip silently.
    };

    let configs: Vec<crate::drivers::bacnet::BacnetConfig> = match serde_json::from_str(&json_str) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "SANDSTAR_BACNET_CONFIGS: failed to parse JSON");
            return;
        }
    };

    let mut registered_any = false;
    // Track each driver's configured points so we can wire them into the
    // poll scheduler after open_all() succeeds.
    let mut driver_points: Vec<(String, Vec<u32>)> = Vec::new();
    for config in configs {
        let id = config.id.clone();
        let point_ids: Vec<u32> = config.objects.iter().map(|o| o.point_id).collect();
        let driver = crate::drivers::bacnet::BacnetDriver::from_config(config);
        let any_driver = crate::drivers::async_driver::AnyDriver::Async(Box::new(driver));
        match handle.register(any_driver).await {
            Ok(()) => {
                tracing::info!(driver = %id, "BACnet driver registered");
                registered_any = true;
                driver_points.push((id, point_ids));
            }
            Err(e) => {
                tracing::error!(driver = %id, error = %e, "failed to register BACnet driver")
            }
        }
    }

    // Open all registered drivers so they bind sockets and run Who-Is discovery.
    // Without this, drivers stay in Pending status and `learn()` returns empty.
    if registered_any {
        // Register each configured object's point_id with its driver, and add
        // a poll bucket so sync_cur() gets called every 5s. Without this,
        // BACnet values never flow into Sandstar channels even though the
        // driver is open and discovery has run.
        for (driver_id, point_ids) in &driver_points {
            if point_ids.is_empty() {
                continue;
            }
            for pid in point_ids {
                if let Err(e) = handle.register_point(*pid, driver_id).await {
                    tracing::warn!(driver = %driver_id, point_id = *pid, error = %e,
                        "BACnet register_point failed");
                }
            }
            let points: Vec<crate::drivers::DriverPointRef> = point_ids
                .iter()
                .map(|&pid| crate::drivers::DriverPointRef {
                    point_id: pid,
                    address: String::new(),
                })
                .collect();
            match handle
                .add_poll_bucket(driver_id, Duration::from_secs(5), points)
                .await
            {
                Ok(()) => tracing::info!(
                    driver = %driver_id,
                    points = point_ids.len(),
                    "BACnet poll bucket added (5s interval)"
                ),
                Err(e) => tracing::warn!(
                    driver = %driver_id,
                    error = %e,
                    "BACnet add_poll_bucket failed"
                ),
            }
        }

        match handle.open_all().await {
            Ok(metas) => {
                tracing::info!(count = metas.len(), "BACnet drivers opened");
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to open BACnet drivers");
            }
        }

        // Spawn a periodic tick task that calls sync_all() every 5s with the
        // BACnet points configured above. Without this, the poll buckets we
        // just added sit idle — the driver actor's loop is command-driven
        // and has no internal scheduler. This task is what actually makes
        // sync_cur fire in production.
        //
        // Stage 1: results are only logged. Wiring them back into engine
        // channels (so /read returns them) is Stage 2.
        let tick_points: std::collections::HashMap<
            crate::drivers::DriverId,
            Vec<crate::drivers::DriverPointRef>,
        > = driver_points
            .into_iter()
            .filter(|(_, pids)| !pids.is_empty())
            .map(|(id, pids)| {
                let refs = pids
                    .into_iter()
                    .map(|pid| crate::drivers::DriverPointRef {
                        point_id: pid,
                        address: String::new(),
                    })
                    .collect();
                (id, refs)
            })
            .collect();
        if !tick_points.is_empty() {
            let handle_tick = handle.clone();
            let engine_tick = engine_handle.clone();
            tokio::spawn(async move {
                // BACnet driver writes are low-priority (16 of 16) so operator
                // or manual writes at lower levels always take precedence.
                // Duration is 30s — longer than our 5s poll so values don't gap,
                // but short enough to expire if the driver stops reporting.
                const BACNET_WRITE_LEVEL: u8 = 16;
                const BACNET_WRITE_DURATION_SECS: f64 = 30.0;

                let mut ticker = tokio::time::interval(Duration::from_secs(5));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // Discard the immediate first tick so we don't race open_all.
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    let results = match handle_tick.sync_all(tick_points.clone()).await {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(error = %e, "BACnet poll tick: sync_all failed");
                            continue;
                        }
                    };
                    let (mut ok_count, mut err_count, mut write_err_count) =
                        (0usize, 0usize, 0usize);
                    for (driver_id, point_id, res) in &results {
                        match res {
                            Ok(v) => {
                                ok_count += 1;
                                tracing::info!(
                                    driver = %driver_id,
                                    point_id,
                                    value = v,
                                    "BACnet sync_cur -> write_channel"
                                );
                                // Stage 2: write the value into the engine channel so
                                // /read?filter=channel==N returns it. Fails silently
                                // (logged as warn) if the point_id doesn't correspond
                                // to a configured virtual channel.
                                let who = format!("bacnet:{driver_id}");
                                if let Err(e) = engine_tick
                                    .write_channel(
                                        *point_id,
                                        Some(*v),
                                        BACNET_WRITE_LEVEL,
                                        who,
                                        BACNET_WRITE_DURATION_SECS,
                                    )
                                    .await
                                {
                                    write_err_count += 1;
                                    tracing::warn!(
                                        driver = %driver_id,
                                        point_id,
                                        error = %e,
                                        "BACnet engine write_channel failed — \
                                         is point_id a configured virtual channel?"
                                    );
                                }
                            }
                            Err(e) => {
                                err_count += 1;
                                tracing::warn!(
                                    driver = %driver_id,
                                    point_id,
                                    error = %e,
                                    "BACnet sync_cur failed"
                                );
                            }
                        }
                    }
                    tracing::info!(
                        ok = ok_count,
                        err = err_count,
                        write_err = write_err_count,
                        "BACnet poll tick complete"
                    );
                }
            });
            tracing::info!("BACnet poll tick task spawned (5s interval)");
        }
    }
}

// ── MQTT driver loader ──────────────────────────────────────

/// Load MQTT drivers from the `SANDSTAR_MQTT_CONFIGS` environment variable.
///
/// The variable should be a JSON array of `MqttConfig` objects, e.g.:
/// ```json
/// [{"id":"mqtt-local","host":"broker","port":1883,"client_id":"sandstar-1","objects":[]}]
/// ```
///
/// Errors are logged but do not prevent server startup.
async fn load_mqtt_drivers(
    handle: &crate::drivers::actor::DriverHandle,
    engine_handle: &EngineHandle,
) {
    let json_str = match std::env::var("SANDSTAR_MQTT_CONFIGS") {
        Ok(s) => s,
        Err(_) => return, // Not configured — skip silently.
    };

    let configs: Vec<crate::drivers::mqtt::MqttConfig> = match serde_json::from_str(&json_str) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "SANDSTAR_MQTT_CONFIGS: failed to parse JSON");
            return;
        }
    };

    let mut registered_any = false;
    // Track each driver's configured points so we can wire them into the
    // poll scheduler after open_all() succeeds.
    let mut driver_points: Vec<(String, Vec<u32>)> = Vec::new();
    for config in configs {
        let id = config.id.clone();
        let point_ids: Vec<u32> = config.objects.iter().map(|o| o.point_id).collect();
        let driver = crate::drivers::mqtt::MqttDriver::from_config(config);
        let any_driver = crate::drivers::async_driver::AnyDriver::Async(Box::new(driver));
        match handle.register(any_driver).await {
            Ok(()) => {
                tracing::info!(driver = %id, "MQTT driver registered");
                registered_any = true;
                driver_points.push((id, point_ids));
            }
            Err(e) => {
                tracing::error!(driver = %id, error = %e, "failed to register MQTT driver")
            }
        }
    }

    // Open all registered drivers so they connect to brokers and subscribe
    // to configured topics. Without this, drivers stay in Pending status
    // and `learn()` returns empty.
    if registered_any {
        // Register each configured object's point_id with its driver, and add
        // a poll bucket so sync_cur() gets called every 5s. Even though MQTT
        // is event-driven (values arrive via the event-loop task into the
        // value cache), the periodic tick is how cached values flow into
        // Sandstar channels via write_channel.
        for (driver_id, point_ids) in &driver_points {
            if point_ids.is_empty() {
                continue;
            }
            for pid in point_ids {
                if let Err(e) = handle.register_point(*pid, driver_id).await {
                    tracing::warn!(driver = %driver_id, point_id = *pid, error = %e,
                        "MQTT register_point failed");
                }
            }
            let points: Vec<crate::drivers::DriverPointRef> = point_ids
                .iter()
                .map(|&pid| crate::drivers::DriverPointRef {
                    point_id: pid,
                    address: String::new(),
                })
                .collect();
            match handle
                .add_poll_bucket(driver_id, Duration::from_secs(5), points)
                .await
            {
                Ok(()) => tracing::info!(
                    driver = %driver_id,
                    points = point_ids.len(),
                    "MQTT poll bucket added (5s interval)"
                ),
                Err(e) => tracing::warn!(
                    driver = %driver_id,
                    error = %e,
                    "MQTT add_poll_bucket failed"
                ),
            }
        }

        match handle.open_all().await {
            Ok(metas) => {
                tracing::info!(count = metas.len(), "MQTT drivers opened");
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to open MQTT drivers");
            }
        }

        // Spawn a periodic tick task that calls sync_all() every 5s with the
        // MQTT points configured above. Without this, the poll buckets we
        // just added sit idle — the driver actor's loop is command-driven
        // and has no internal scheduler. This task is what actually makes
        // cached values flow into engine channels in production.
        let tick_points: std::collections::HashMap<
            crate::drivers::DriverId,
            Vec<crate::drivers::DriverPointRef>,
        > = driver_points
            .into_iter()
            .filter(|(_, pids)| !pids.is_empty())
            .map(|(id, pids)| {
                let refs = pids
                    .into_iter()
                    .map(|pid| crate::drivers::DriverPointRef {
                        point_id: pid,
                        address: String::new(),
                    })
                    .collect();
                (id, refs)
            })
            .collect();
        if !tick_points.is_empty() {
            let handle_tick = handle.clone();
            let engine_tick = engine_handle.clone();
            tokio::spawn(async move {
                // MQTT driver writes are low-priority (16 of 16) so operator
                // or manual writes at lower levels always take precedence.
                // Duration is 30s — longer than our 5s poll so values don't gap,
                // but short enough to expire if the driver stops reporting.
                const MQTT_WRITE_LEVEL: u8 = 16;
                const MQTT_WRITE_DURATION_SECS: f64 = 30.0;

                let mut ticker = tokio::time::interval(Duration::from_secs(5));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // Discard the immediate first tick so we don't race open_all.
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    let results = match handle_tick.sync_all(tick_points.clone()).await {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(error = %e, "MQTT poll tick: sync_all failed");
                            continue;
                        }
                    };
                    let (mut ok_count, mut err_count, mut write_err_count) =
                        (0usize, 0usize, 0usize);
                    for (driver_id, point_id, res) in &results {
                        match res {
                            Ok(v) => {
                                ok_count += 1;
                                tracing::info!(
                                    driver = %driver_id,
                                    point_id,
                                    value = v,
                                    "MQTT sync_cur -> write_channel"
                                );
                                // Write the value into the engine channel so
                                // /read?filter=channel==N returns it. Fails silently
                                // (logged as warn) if the point_id doesn't correspond
                                // to a configured virtual channel.
                                let who = format!("mqtt:{driver_id}");
                                if let Err(e) = engine_tick
                                    .write_channel(
                                        *point_id,
                                        Some(*v),
                                        MQTT_WRITE_LEVEL,
                                        who,
                                        MQTT_WRITE_DURATION_SECS,
                                    )
                                    .await
                                {
                                    write_err_count += 1;
                                    tracing::warn!(
                                        driver = %driver_id,
                                        point_id,
                                        error = %e,
                                        "MQTT engine write_channel failed — \
                                         is point_id a configured virtual channel?"
                                    );
                                }
                            }
                            Err(e) => {
                                err_count += 1;
                                tracing::warn!(
                                    driver = %driver_id,
                                    point_id,
                                    error = %e,
                                    "MQTT sync_cur failed"
                                );
                            }
                        }
                    }
                    tracing::info!(
                        ok = ok_count,
                        err = err_count,
                        write_err = write_err_count,
                        "MQTT poll tick complete"
                    );
                }
            });
            tracing::info!("MQTT poll tick task spawned (5s interval)");
        }
    }
}

// ── roxWarp cluster routes ──────────────────────────────────

/// Fallback router when clustering is disabled — always returns a status response.
pub fn cluster_disabled_router() -> Router {
    Router::new().route("/api/cluster/status", get(cluster_disabled_handler))
}

async fn cluster_disabled_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "enabled": false,
            "message": "Clustering not enabled. Start with --cluster flag to enable roxWarp.",
            "requirements": {
                "flag": "--cluster",
                "configFile": "--cluster-config <path> (optional)",
                "nodeId": "--node-id <id> (optional, auto-generated from hostname)",
                "peers": "Configure peer addresses in the cluster config JSON file"
            }
        })),
    )
}

/// Build the Axum router for roxWarp cluster endpoints:
/// - `GET /roxwarp` — WebSocket upgrade for peer gossip
/// - `GET /api/cluster/status` — cluster overview (JSON)
/// - `POST /api/cluster/query` — distributed Haystack filter query
pub fn roxwarp_router(state: crate::roxwarp::RoxWarpState) -> Router {
    use crate::roxwarp::handler;

    let ws_route = Router::new()
        .route("/roxwarp", get(handler::roxwarp_upgrade))
        .with_state(state.clone());

    let api_routes = Router::new()
        .route("/api/cluster/status", get(cluster_status_handler))
        .route("/api/cluster/query", post(cluster_query_handler))
        .with_state(state);

    ws_route.merge(api_routes)
}

/// GET /api/cluster/status — return cluster status as JSON.
async fn cluster_status_handler(
    State(state): State<crate::roxwarp::RoxWarpState>,
) -> impl IntoResponse {
    // Build a snapshot of delta engine state
    let de = &state.delta_engine;
    let vv = de.get_version_vector().await;
    let (version, points) = de.full_state().await;

    let status = serde_json::json!({
        "nodeId": de.node_id,
        "version": version,
        "pointCount": points.len(),
        "versionVector": vv,
    });
    (StatusCode::OK, Json(status))
}

/// POST /api/cluster/query — execute a distributed Haystack filter query.
///
/// Request body: `{"filter": "point and temp", "limit": 100}`
/// Response: `{"results": [...], "nodeCount": 1, "totalResults": N}`
async fn cluster_query_handler(
    State(state): State<crate::roxwarp::RoxWarpState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let filter = match body.get("filter").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "missing 'filter' field"})),
            );
        }
    };
    let limit = body.get("limit").and_then(|v| v.as_u64()).map(|n| n as u32);

    // Evaluate filter against local delta engine points
    let (_, all_points) = state.delta_engine.full_state().await;
    let max = limit.unwrap_or(u32::MAX) as usize;

    let results: Vec<crate::roxwarp::QueryPoint> = all_points
        .iter()
        .filter(|p| crate::roxwarp::handler::evaluate_point_filter(filter, p))
        .take(max)
        .map(|p| crate::roxwarp::QueryPoint {
            channel: p.channel,
            value: p.value,
            unit: p.unit.clone(),
            status: p.status.clone(),
            node_id: state.delta_engine.node_id.clone(),
        })
        .collect();

    let total = results.len();
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "results": results,
            "nodeCount": 1,
            "totalResults": total,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_allows_under_limit() {
        let limiter = RateLimiter::new(10);
        // First 10 requests within the same window should be allowed
        for i in 0..10 {
            assert!(limiter.check(), "request {} should be allowed", i);
        }
    }

    #[test]
    fn rate_limiter_blocks_over_limit() {
        let limiter = RateLimiter::new(5);
        // Exhaust the limit
        for _ in 0..5 {
            assert!(limiter.check());
        }
        // 6th request should be blocked
        assert!(!limiter.check(), "request over limit should be blocked");
        assert!(
            !limiter.check(),
            "subsequent requests should also be blocked"
        );
    }

    #[test]
    fn rate_limiter_resets_after_window() {
        let limiter = RateLimiter::new(3);
        // Exhaust the limit
        for _ in 0..3 {
            assert!(limiter.check());
        }
        assert!(!limiter.check(), "over limit");

        // Simulate window expiry by rewinding the window start
        limiter.window_start.store(0, Ordering::Relaxed);

        // Should be allowed again after window reset
        assert!(limiter.check(), "should allow after window reset");
    }

    #[test]
    fn rate_limiter_single_request_allowed() {
        let limiter = RateLimiter::new(1);
        assert!(limiter.check());
        assert!(!limiter.check());
    }

    #[test]
    fn rate_limiter_high_limit() {
        let limiter = RateLimiter::new(10_000);
        // All requests well under the limit should pass
        for _ in 0..1_000 {
            assert!(limiter.check());
        }
    }
}
