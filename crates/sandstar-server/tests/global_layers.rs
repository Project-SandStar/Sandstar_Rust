//! Global HTTP layers cover the late-merged routers (R-53, S-35).
//!
//! `main.rs` merges the tags, alerts and roxWarp routers into the core app.
//! The request counter, rate limit, 1 MiB body limit and CORS must wrap
//! those routes too, not only the routes built inside `rest`.
//!
//! Not covered here: the simulator router (`simulator-hal` feature only) and
//! `cluster_disabled_router` (merged instead of the roxWarp router when
//! clustering is off); both are merged at the same place in `main.rs`.

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
use tokio::sync::mpsc;
use tower::ServiceExt;

#[allow(dead_code)]
mod common;

const TOKEN: &str = "s35-test-token";

/// Late-merged GET routes that answer without a token.
const LATE_READS: &[&str] = &["/api/tags", "/api/alerts/active", "/api/cluster/status"];

fn build_app(rate_limit: u64) -> (Router, tokio::task::JoinHandle<()>) {
    std::env::remove_var("SANDSTAR_WEB_DIR");
    let auth_state = AuthState::new(AuthStore::with_bearer_token(TOKEN.to_string()));

    let engine = common::setup_demo_engine();
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCmd>(64);
    let handle = EngineHandle::new(cmd_tx);
    let cmd_task = common::spawn_cmd_loop(engine, cmd_rx);

    let dyn_store = Arc::new(RwLock::new(DynSlotStore::with_defaults()));
    let alerts = Arc::new(Mutex::new(AlertManager::new(AlertConfig::default())));
    let rw_state = RoxWarpState {
        delta_engine: Arc::new(DeltaEngine::new("s35".into())),
        config: ClusterConfig {
            node_id: "s35".to_string(),
            port: 0,
            peers: vec![],
            heartbeat_interval_secs: 1,
            anti_entropy_interval_secs: 10,
            feed_interval_secs: 60,
            cert_path: None,
            key_path: None,
            ca_path: None,
            enable_mdns: false,
            allow_plain: false,
        },
        sox_tree: None,
    };

    // Same order as `main.rs`: every router first, global layers last.
    let app = rest::api_routes(handle, auth_state.clone(), None, None)
        .merge(rest::tags::router(dyn_store, auth_state.clone()))
        .merge(alert_router(alerts, auth_state.clone()))
        .merge(rest::roxwarp_router(rw_state, auth_state));
    (rest::with_global_layers(app, rate_limit), cmd_task)
}

async fn get(app: &Router, path: &str) -> axum::response::Response {
    let req = Request::builder()
        .uri(path)
        .header("origin", "http://example.test")
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

#[tokio::test]
async fn late_routes_are_rate_limited() {
    for path in LATE_READS {
        let (app, _task) = build_app(1);
        let mut limited = 0;
        // Five requests span at most two one-second windows, so at most two
        // pass with a limit of one per second.
        for _ in 0..5 {
            if get(&app, path).await.status() == StatusCode::TOO_MANY_REQUESTS {
                limited += 1;
            }
        }
        assert!(limited >= 3, "{path}: only {limited} of 5 requests got 429");
    }
}

#[tokio::test]
async fn late_write_routes_are_rate_limited_before_auth() {
    // Token guessing on a late-merged write route hits the rate limit.
    let (app, _task) = build_app(1);
    let mut limited = 0;
    for i in 0..5 {
        let req = Request::builder()
            .method("PUT")
            .uri("/api/tags/1")
            .header("authorization", format!("Bearer guess-{i}"))
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let s = app.clone().oneshot(req).await.unwrap().status();
        if s == StatusCode::TOO_MANY_REQUESTS {
            limited += 1;
        } else {
            assert_eq!(s, StatusCode::UNAUTHORIZED, "guess {i}");
        }
    }
    assert!(limited >= 3, "only {limited} of 5 guesses got 429");
}

#[tokio::test]
async fn late_write_routes_enforce_the_body_limit() {
    let (app, _task) = build_app(0);
    // Valid JSON over 1 MiB and under axum's 2 MiB default.
    let big = format!("{{\"note\":\"{}\"}}", "x".repeat(1_536 * 1024));
    let req = Request::builder()
        .method("PUT")
        .uri("/api/tags/1")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(big))
        .unwrap();
    let s = app.oneshot(req).await.unwrap().status();
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn late_routes_get_cors_headers() {
    let (app, _task) = build_app(0);
    for path in LATE_READS {
        let res = get(&app, path).await;
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        assert_eq!(
            res.headers()
                .get("access-control-allow-origin")
                .map(|v| v.as_bytes()),
            Some(&b"*"[..]),
            "{path}: no CORS header"
        );
    }
}

#[tokio::test]
async fn late_routes_answer_cors_preflight() {
    let (app, _task) = build_app(0);
    for (path, method) in [
        ("/api/alerts/config", "PUT"),
        ("/api/tags/1", "PUT"),
        ("/api/cluster/query", "POST"),
    ] {
        let req = Request::builder()
            .method("OPTIONS")
            .uri(path)
            .header("origin", "http://example.test")
            .header("access-control-request-method", method)
            .header(
                "access-control-request-headers",
                "authorization,content-type",
            )
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        assert!(
            res.headers().contains_key("access-control-allow-methods"),
            "{path}: preflight without allow-methods"
        );
    }
}
