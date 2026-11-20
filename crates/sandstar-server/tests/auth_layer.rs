//! Route inventory for the auth layer (R-2, S-31).
//!
//! Builds the REST app the way `main.rs` does (core router + SOX + tags +
//! alerts + roxWarp) and checks every state-changing route:
//! - with auth configured, no token or a wrong token gets 401;
//! - with the right token the request reaches the handler (not 401);
//! - with no auth configured nothing gets 401 (field behavior unchanged).
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
    "/api/sox/tree",
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

    let app = rest::router_with_auth(handle, auth_state.clone(), None, 0, Some(sox_state))
        .merge(rest::tags::router(dyn_store, auth_state.clone()))
        .merge(alert_router(alerts, auth_state.clone()))
        .merge(rest::roxwarp_router(rw_state, auth_state));
    (app, cmd_task)
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
