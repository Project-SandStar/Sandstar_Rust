//! Route inventory for the auth layer (R-2, S-31).
//!
//! Builds the REST app the way `main.rs` does (core router + SOX + tags +
//! alerts + roxWarp) and checks every state-changing route:
//! - with auth configured, no token or a wrong token gets 401;
//! - with the right token the request reaches the handler (not 401);
//! - with no auth configured nothing gets 401 (field behavior unchanged).
//!
//! The SOX tree and `/file/*` reads sit behind the editor gate (R-55, S-33).
//!
//! Limitation: the route list is maintained by hand. A new write route that
//! is not added to `WRITE_ROUTES` is not covered by this test.

use std::sync::{Arc, Mutex, RwLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use sandstar_server::alerts::{alert_router, AlertConfig, AlertManager};
use sandstar_server::auth::{AuthState, AuthStore};
use sandstar_server::rest::{self, EngineCmd, EngineHandle};
use sandstar_server::roxwarp::cluster::{ClusterConfig, DeltaEngine};
use sandstar_server::roxwarp::RoxWarpState;
use sandstar_server::sox::dyn_slots::DynSlotStore;
use sandstar_server::sox::sox_handlers::{ComponentTree, ManifestDb};
use sandstar_server::sox::SharedComponentTree;
use tokio::sync::mpsc;
use tower::ServiceExt;

#[allow(dead_code)]
mod common;

const TOKEN: &str = "s31-test-token";

/// Every state-changing route on the 8085 app: (method, path, body).
const WRITE_ROUTES: &[(&str, &str, &str)] = &[
    // Haystack core
    (
        "POST",
        "/api/pointWrite",
        r#"{"channel":1113,"value":1.0,"level":16}"#,
    ),
    ("POST", "/api/pollNow", "{}"),
    ("POST", "/api/reload", "{}"),
    ("POST", "/api/watchSub", "{}"),
    ("POST", "/api/watchUnsub", "{}"),
    ("POST", "/api/watchPoll", "{}"),
    ("POST", "/api/hisRead", "{}"),
    ("POST", "/api/nav", "{}"),
    ("POST", "/api/invokeAction", "{}"),
    // Drivers
    ("POST", "/api/drivers", "{}"),
    ("DELETE", "/api/drivers/x", ""),
    ("POST", "/api/drivers/x/open", "{}"),
    ("POST", "/api/drivers/x/close", "{}"),
    ("POST", "/api/drivers/x/ping", "{}"),
    ("POST", "/api/drivers/x/message", "{}"),
    ("POST", "/api/drivers/x/write", "{}"),
    ("POST", "/api/syncCur", "{}"),
    // SOX component tree
    ("POST", "/api/sox/comp", "{}"),
    ("DELETE", "/api/sox/comp/9999", ""),
    ("PUT", "/api/sox/comp/9999/name", "{}"),
    ("PUT", "/api/sox/comp/9999/slot/0", "{}"),
    ("POST", "/api/sox/comp/9999/invoke/0", "{}"),
    ("POST", "/api/sox/link", "{}"),
    ("DELETE", "/api/sox/link", "{}"),
    ("PUT", "/api/sox/comp/9999/pos", "{}"),
    ("PUT", "/api/sox/comp/9999/reorder", "{}"),
    // File transfer
    ("PUT", "/file/s31-probe.txt", "x"),
    // Dynamic tags
    ("PUT", "/api/tags/9999", "{}"),
    ("DELETE", "/api/tags/9999/k", ""),
    // Alerts (SMTP config, recipients, test e-mail)
    ("PUT", "/api/alerts/config", "{}"),
    ("POST", "/api/alerts/recipients", "{}"),
    ("DELETE", "/api/alerts/recipients/a%40b.c", ""),
    ("POST", "/api/alerts/test", "{}"),
    // Cluster
    ("POST", "/api/cluster/query", "{}"),
];

/// Read routes that must stay public even with auth configured.
const READ_ROUTES: &[&str] = &[
    "/api/about",
    "/api/read?filter=point",
    "/api/drivers",
    "/api/tags",
    "/api/alerts/config",
    "/api/cluster/status",
];

fn build_app(auth: AuthStore) -> (Router, tokio::task::JoinHandle<()>) {
    // Keep `PUT /file/*` from writing anywhere: with no file root it answers
    // 503 before touching the disk.
    std::env::remove_var("SANDSTAR_FILE_ROOT");
    std::env::remove_var("SANDSTAR_WEB_DIR");
    let auth_state = AuthState::new(auth);

    let manifest_db = Arc::new(ManifestDb::load(""));
    let tree: SharedComponentTree = Arc::new(RwLock::new(ComponentTree::new_with_manifest(
        manifest_db.clone(),
    )));
    let sox_state = rest::sox_api::SoxApiState {
        tree,
        manifest_db,
        dyn_store: None,
    };

    let engine = common::setup_demo_engine();
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCmd>(64);
    let handle = EngineHandle::new(cmd_tx);
    let cmd_task = common::spawn_cmd_loop(engine, cmd_rx);

    let dyn_store = Arc::new(RwLock::new(DynSlotStore::with_defaults()));
    let alerts = Arc::new(Mutex::new(AlertManager::new(AlertConfig::default())));
    let rw_state = RoxWarpState {
        delta_engine: Arc::new(DeltaEngine::new("s31".into())),
        config: ClusterConfig {
            node_id: "s31".to_string(),
            port: 0,
            peers: vec![],
            heartbeat_interval_secs: 1,
            anti_entropy_interval_secs: 10,
            feed_interval_secs: 60,
            cert_path: None,
            key_path: None,
            ca_path: None,
            enable_mdns: false,
        },
        sox_tree: None,
    };

    // Same order as `main.rs`: every router first, global layers last.
    let app = rest::api_routes(handle, auth_state.clone(), None, Some(sox_state))
        .merge(rest::tags::router(dyn_store, auth_state.clone()))
        .merge(alert_router(alerts, auth_state.clone()))
        .merge(rest::roxwarp_router(rw_state, auth_state));
    (rest::with_global_layers(app, 0), cmd_task)
}

async fn status(
    app: &Router,
    method: &str,
    path: &str,
    body: &str,
    token: Option<&str>,
) -> StatusCode {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let req = req.body(Body::from(body.to_string())).unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn write_routes_require_a_token_when_auth_is_configured() {
    let (app, _task) = build_app(AuthStore::with_bearer_token(TOKEN.to_string()));
    let mut open = Vec::new();
    for (m, p, b) in WRITE_ROUTES {
        for token in [None, Some("wrong-token")] {
            let s = status(&app, m, p, b, token).await;
            if s != StatusCode::UNAUTHORIZED {
                open.push(format!("{m} {p} token={token:?} -> {s}"));
            }
        }
    }
    assert!(
        open.is_empty(),
        "write routes reachable without a valid token:\n{}",
        open.join("\n")
    );
}

#[tokio::test]
async fn write_routes_accept_the_configured_token() {
    let (app, _task) = build_app(AuthStore::with_bearer_token(TOKEN.to_string()));
    for (m, p, b) in WRITE_ROUTES {
        let s = status(&app, m, p, b, Some(TOKEN)).await;
        assert_ne!(
            s,
            StatusCode::UNAUTHORIZED,
            "{m} {p} rejected a valid token"
        );
        // 405 would mean the inventory names a method the router does not serve.
        assert_ne!(
            s,
            StatusCode::METHOD_NOT_ALLOWED,
            "{m} {p} is not a served route"
        );
    }
}

#[tokio::test]
async fn no_route_requires_a_token_when_auth_is_not_configured() {
    let (app, _task) = build_app(AuthStore::new());
    for (m, p, b) in WRITE_ROUTES {
        let s = status(&app, m, p, b, None).await;
        assert_ne!(
            s,
            StatusCode::UNAUTHORIZED,
            "{m} {p} needs a token with auth off"
        );
    }
}

#[tokio::test]
async fn read_routes_stay_public_when_auth_is_configured() {
    let (app, _task) = build_app(AuthStore::with_bearer_token(TOKEN.to_string()));
    for p in READ_ROUTES {
        let s = status(&app, "GET", p, "", None).await;
        assert_ne!(s, StatusCode::UNAUTHORIZED, "GET {p} needs a token");
    }
}

#[test]
fn bearer_comparison_rejects_prefixes_and_longer_tokens() {
    let store = AuthStore::with_bearer_token(TOKEN.to_string());
    assert!(store.check_bearer(TOKEN));
    assert!(!store.check_bearer(""));
    assert!(!store.check_bearer("s31-test-toke"));
    assert!(!store.check_bearer("s31-test-tokenX"));
    assert!(!store.check_bearer("s31-test-tokeN"));
    assert!(!AuthStore::new().check_bearer(""));
}

#[tokio::test]
async fn an_empty_configured_token_does_not_open_write_routes() {
    let (app, _task) = build_app(AuthStore::with_bearer_token(String::new()));
    for (m, p, b) in WRITE_ROUTES {
        for token in [None, Some("")] {
            let s = status(&app, m, p, b, token).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{m} {p} token={token:?}");
        }
    }
}

// ── SOX tree and file reads (R-55, S-33) ────────────────────

/// Reads that carry the component tree or device files: behind the editor
/// gate (bearer or session cookie, plus Origin).
/// Each entry is (path, status once the gate lets the request through);
/// exact so a route that is not mounted (404) cannot pass. `build_app`
/// unsets the file root, so `/file/*` answers 503 behind the gate.
const GATED_READS: &[(&str, StatusCode)] = &[
    ("/api/sox/tree", StatusCode::OK),
    // The test tree is empty: the handler answers 404 for comp 0.
    ("/api/sox/comp/0", StatusCode::NOT_FOUND),
    ("/api/sox/palette", StatusCode::OK),
    ("/api/sox/names", StatusCode::OK),
    (
        "/file/sedona/manifests/x/x.xml",
        StatusCode::SERVICE_UNAVAILABLE,
    ),
];

async fn head_with(app: &Router, path: &str, headers: &[(&str, String)]) -> StatusCode {
    let mut req = Request::builder().method("HEAD").uri(path);
    for (k, v) in headers {
        req = req.header(*k, v.as_str());
    }
    let req = req.body(Body::empty()).unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

async fn get_with(app: &Router, path: &str, headers: &[(&str, String)]) -> StatusCode {
    let mut req = Request::builder().method("GET").uri(path);
    for (k, v) in headers {
        req = req.header(*k, v.as_str());
    }
    let req = req.body(Body::empty()).unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn tree_and_file_reads_need_a_credential_when_auth_is_configured() {
    let (app, _task) = build_app(AuthStore::with_bearer_token(TOKEN.to_string()));
    let cookie = || ("cookie", format!("a=b; sandstar_session={TOKEN}"));
    for &(p, want) in GATED_READS {
        assert_eq!(
            head_with(&app, p, &[]).await,
            StatusCode::UNAUTHORIZED,
            "HEAD {p} none"
        );
        assert_eq!(
            get_with(&app, p, &[]).await,
            StatusCode::UNAUTHORIZED,
            "{p} none"
        );
        let bad = [("cookie", "sandstar_session=nope".to_string())];
        assert_eq!(
            get_with(&app, p, &bad).await,
            StatusCode::UNAUTHORIZED,
            "{p} bad cookie"
        );
        let wrong = [("authorization", "Bearer nope".to_string())];
        assert_eq!(
            get_with(&app, p, &wrong).await,
            StatusCode::UNAUTHORIZED,
            "{p} wrong"
        );
        // Same-origin editor fetch: the browser may send an Origin equal to Host.
        let same = vec![
            cookie(),
            ("host", "dev.local:8091".to_string()),
            ("origin", "http://dev.local:8091".to_string()),
        ];
        for ok in [vec![cookie()], vec![bearer()], same] {
            assert_eq!(get_with(&app, p, &ok).await, want, "{p} {ok:?}");
        }
        for cred in [bearer(), cookie()] {
            let evil = [cred, ("origin", "http://evil.example".to_string())];
            assert_eq!(
                get_with(&app, p, &evil).await,
                StatusCode::FORBIDDEN,
                "{p} {evil:?}"
            );
        }
    }
}

#[tokio::test]
async fn file_upload_stays_bearer_only() {
    // The read gate accepts the session cookie; the write layer must not.
    let (app, _task) = build_app(AuthStore::with_bearer_token(TOKEN.to_string()));
    let req = Request::builder()
        .method("PUT")
        .uri("/file/s33-probe.txt")
        .header("cookie", format!("sandstar_session={TOKEN}"))
        .body(Body::from("x"))
        .unwrap();
    let s = app.clone().oneshot(req).await.unwrap().status();
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn library_stubs_stay_open_when_auth_is_configured() {
    let (app, _task) = build_app(AuthStore::with_bearer_token(TOKEN.to_string()));
    for p in ["/library/", "/tags"] {
        assert_eq!(get_with(&app, p, &[]).await, StatusCode::OK, "{p}");
    }
}

#[tokio::test]
async fn tree_and_file_reads_stay_open_when_auth_is_not_configured() {
    let (app, _task) = build_app(AuthStore::new());
    for &(p, want) in GATED_READS {
        let evil = [("origin", "http://evil.example".to_string())];
        assert_eq!(get_with(&app, p, &evil).await, want, "{p}");
    }
}

// ── WebSocket and editor endpoints (R-52, S-32) ─────────────

/// Editor endpoints gated before the WebSocket upgrade.
const UPGRADE_ROUTES: &[&str] = &["/live", "/api/rows", "/api/ws"];

async fn serve(auth: AuthStore) -> (String, tokio::task::JoinHandle<()>) {
    let (app, task) = build_app(auth);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("127.0.0.1:{}", addr.port()), task)
}

/// Try a WebSocket handshake; returns the HTTP status (101 on success).
async fn ws_status(host: &str, path: &str, headers: &[(&str, String)]) -> u16 {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://{host}{path}").into_client_request().unwrap();
    for (k, v) in headers {
        req.headers_mut().insert(
            axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((_ws, resp)) => resp.status().as_u16(),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => resp.status().as_u16(),
        Err(e) => panic!("{path}: unexpected handshake error {e}"),
    }
}

fn bearer() -> (&'static str, String) {
    ("authorization", format!("Bearer {TOKEN}"))
}

#[tokio::test]
async fn editor_websockets_need_a_credential_when_auth_is_configured() {
    let (host, _t) = serve(AuthStore::with_bearer_token(TOKEN.to_string())).await;
    for p in ["/live", "/api/rows"] {
        assert_eq!(ws_status(&host, p, &[]).await, 401, "{p} without token");
        let wrong = [("authorization", "Bearer nope".to_string())];
        assert_eq!(ws_status(&host, p, &wrong).await, 401, "{p} wrong token");
        let cookie = [("cookie", "sandstar_session=nope".to_string())];
        assert_eq!(ws_status(&host, p, &cookie).await, 401, "{p} bad cookie");
    }
    for p in UPGRADE_ROUTES {
        assert_eq!(ws_status(&host, p, &[bearer()]).await, 101, "{p} bearer");
        let cookie = [("cookie", format!("a=b; sandstar_session={TOKEN}"))];
        assert_eq!(ws_status(&host, p, &cookie).await, 101, "{p} cookie");
    }
}

#[tokio::test]
async fn editor_websockets_refuse_a_foreign_origin() {
    let (host, _t) = serve(AuthStore::with_bearer_token(TOKEN.to_string())).await;
    for p in UPGRADE_ROUTES {
        let evil = [bearer(), ("origin", "http://evil.example".to_string())];
        assert_eq!(ws_status(&host, p, &evil).await, 403, "{p} foreign origin");
        let same = [bearer(), ("origin", format!("http://{host}"))];
        assert_eq!(ws_status(&host, p, &same).await, 101, "{p} same origin");
    }
}

#[tokio::test]
async fn null_origin_and_a_planted_cookie_are_handled() {
    let (host, _t) = serve(AuthStore::with_bearer_token(TOKEN.to_string())).await;
    let null = [bearer(), ("origin", "null".to_string())];
    assert_eq!(ws_status(&host, "/live", &null).await, 403);
    // A bad cookie with the same name must not hide the valid one.
    let two = [(
        "cookie",
        format!("sandstar_session=planted; sandstar_session={TOKEN}"),
    )];
    assert_eq!(ws_status(&host, "/live", &two).await, 101);
    let basic = [("authorization", format!("Basic {TOKEN}"))];
    assert_eq!(ws_status(&host, "/live", &basic).await, 401);
}

#[tokio::test]
async fn allowlisted_origin_is_accepted() {
    let mut store = AuthStore::with_bearer_token(TOKEN.to_string());
    store.set_allowed_origins(vec!["http://Editor.local:8080/".to_string()]);
    let (host, _t) = serve(store).await;
    let h = [bearer(), ("origin", "http://editor.local:8080".to_string())];
    assert_eq!(ws_status(&host, "/live", &h).await, 101);
}

#[tokio::test]
async fn editor_websockets_stay_open_when_auth_is_not_configured() {
    let (host, _t) = serve(AuthStore::new()).await;
    for p in UPGRADE_ROUTES {
        let evil = [("origin", "http://evil.example".to_string())];
        assert_eq!(ws_status(&host, p, &evil).await, 101, "{p}");
    }
}

#[tokio::test]
async fn sedona_state_needs_a_credential_when_auth_is_configured() {
    let (app, _task) = build_app(AuthStore::with_bearer_token(TOKEN.to_string()));
    let p = "/live/sedonaState";
    assert_eq!(
        status(&app, "GET", p, "", None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_ne!(
        status(&app, "GET", p, "", Some(TOKEN)).await,
        StatusCode::UNAUTHORIZED
    );
}

async fn post_json(
    app: &Router,
    path: &str,
    body: &serde_json::Value,
) -> (StatusCode, serde_json::Value, Option<String>) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let cookie = resp
        .headers()
        .get("set-cookie")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap(), cookie)
}

#[tokio::test]
async fn scram_login_sets_an_httponly_strict_session_cookie() {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut store = AuthStore::new();
    store.add_user("s32user", "s32-pass");
    let (app, _task) = build_app(store);

    let (client_first, nonce) = sandstar_server::auth::scram_client_first("s32user");
    let bare = client_first.strip_prefix("n,,").unwrap().to_string();
    let body = serde_json::json!({"action": "scramFirst", "data": b64.encode(&client_first)});
    let (_, v, cookie) = post_json(&app, "/api/auth", &body).await;
    assert!(cookie.is_none(), "no cookie before the handshake completes");
    let hs = v["handshakeToken"].as_str().unwrap().to_string();
    let server_first = String::from_utf8(b64.decode(v["data"].as_str().unwrap()).unwrap()).unwrap();

    let client_final =
        sandstar_server::auth::scram_client_final("s32-pass", &nonce, &bare, &server_first)
            .unwrap();
    let body = serde_json::json!({
        "action": "scramFinal", "handshakeToken": hs, "data": b64.encode(&client_final)
    });
    let (status, v, cookie) = post_json(&app, "/api/auth", &body).await;
    assert_eq!(status, StatusCode::OK);
    let cookie = cookie.expect("Set-Cookie on SCRAM success");
    let token = v["authToken"].as_str().unwrap();
    assert!(
        cookie.starts_with(&format!("sandstar_session={token};")),
        "{cookie}"
    );
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");

    // The cookie opens /live/sedonaState; a SCRAM-only setup is auth-on.
    let req = Request::builder()
        .uri("/live/sedonaState")
        .header("cookie", format!("sandstar_session={token}"))
        .body(Body::empty())
        .unwrap();
    let s = app.clone().oneshot(req).await.unwrap().status();
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        status_of(&app, "/live/sedonaState").await,
        StatusCode::UNAUTHORIZED
    );
}

async fn status_of(app: &Router, path: &str) -> StatusCode {
    status(app, "GET", path, "", None).await
}

#[tokio::test]
async fn api_ws_is_not_pre_authenticated_with_scram_only() {
    // Before S-32 a SCRAM-only setup marked every /api/ws connection as
    // authenticated (`ws.rs` `(None, _) => true`).
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let mut store = AuthStore::new();
    store.add_user("s32user", "s32-pass");
    let (host, _t) = serve(store).await;

    let evil = [("origin", "http://evil.example".to_string())];
    assert_eq!(ws_status(&host, "/api/ws", &evil).await, 403);

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{host}/api/ws"))
        .await
        .unwrap();
    let sub = r#"{"op":"subscribe","id":"s32","ids":[1113]}"#;
    ws.send(Message::Text(sub.into())).await.unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("reply in time")
        .expect("open socket")
        .unwrap();
    let text = reply.into_text().unwrap();
    assert!(text.contains("AUTH_REQUIRED"), "{text}");
}
