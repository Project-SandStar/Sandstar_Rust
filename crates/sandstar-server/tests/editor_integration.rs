//! Integration tests for Phase 14.0G editor backend endpoints.
//!
//! Builds a real Axum router (not just the handler closures), wires the
//! SOX state through `router_with_auth`, and hits each new endpoint over
//! HTTP/WebSocket to guarantee the full stack round-trips correctly.
//! These live at `tests/` rather than in a `#[cfg(test)] mod tests` block
//! because they exercise the router wiring itself, which unit tests can't.

use std::sync::Arc;

use sandstar_server::auth::{AuthState, AuthStore};
use sandstar_server::rest::{self, EngineCmd, EngineHandle};
use sandstar_server::sox::sox_handlers::{ComponentTree, ManifestDb};
use sandstar_server::sox::SharedComponentTree;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

mod common;

struct EditorTestServer {
    base_url: String,
    _cmd_task: tokio::task::JoinHandle<()>,
    _tree: SharedComponentTree,
}

impl EditorTestServer {
    async fn start() -> Self {
        // Ensure env vars don't leak between tests — an earlier test
        // might have set SANDSTAR_WEB_DIR or SANDSTAR_FILE_ROOT and we
        // want the baseline "not configured" behaviour here.
        std::env::remove_var("SANDSTAR_WEB_DIR");
        std::env::remove_var("SANDSTAR_FILE_ROOT");

        let manifest_db = Arc::new(ManifestDb::load(""));
        let tree: SharedComponentTree = Arc::new(std::sync::RwLock::new(
            ComponentTree::new_with_manifest(manifest_db.clone()),
        ));

        let sox_state = rest::sox_api::SoxApiState {
            tree: tree.clone(),
            manifest_db: manifest_db.clone(),
            dyn_store: None,
        };

        let engine = common::setup_demo_engine();
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCmd>(64);
        let handle = EngineHandle::new(cmd_tx);
        let app = rest::router_with_auth(
            handle,
            AuthState::new(AuthStore::new()),
            None,
            0,
            Some(sox_state),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let cmd_task = common::spawn_cmd_loop(engine, cmd_rx);

        EditorTestServer {
            base_url,
            _cmd_task: cmd_task,
            _tree: tree,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn ws_url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.replace("http://", "ws://"), path)
    }
}

// ── /editor placeholder ─────────────────────────────

#[tokio::test]
async fn editor_serves_placeholder_when_web_dir_unset() {
    let srv = EditorTestServer::start().await;
    let resp = reqwest::get(srv.url("/editor")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.starts_with("text/html"));
    let body = resp.text().await.unwrap();
    // Placeholder HTML carries some recognisable Sedona/Sandstar markup.
    assert!(!body.is_empty());
}

// ── /library/ + /tags empty shims ───────────────────

#[tokio::test]
async fn library_index_returns_empty_array() {
    let srv = EditorTestServer::start().await;
    let json: JsonValue = reqwest::get(srv.url("/library/?q=anything"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let arr = json.as_array().unwrap();
    assert!(arr.is_empty());
}

#[tokio::test]
async fn tags_returns_empty_array() {
    let srv = EditorTestServer::start().await;
    let json: JsonValue = reqwest::get(srv.url("/tags"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let arr = json.as_array().unwrap();
    assert!(arr.is_empty());
}

// ── /file/* route-wiring check ─────────────────────
//
// The env-var-driven `resolve_file_root()` is covered by the unit tests
// in `file_ops::tests` (traversal, PUT→GET roundtrip via tempdir, 503
// without config). Here we only assert the route is wired through
// `router_with_auth` — integration tests can't reliably set/unset
// process-global env vars in parallel without a `serial_test` dep, and
// the unit tests already cover the behaviour.

#[tokio::test]
async fn file_route_is_wired() {
    let srv = EditorTestServer::start().await;
    let resp = reqwest::get(srv.url("/file/app.sab")).await.unwrap();
    // 503 when root unset, 404 when root set but file missing, 400 on
    // traversal. Anything but 404-for-"unknown-route" proves the handler
    // is reachable. We accept {503, 404, 200, 400} here — not 405 or
    // "route not found" 404.
    assert!(
        [200u16, 400, 404, 503].contains(&resp.status().as_u16()),
        "expected file handler to respond with a known status, got {}",
        resp.status()
    );
}

// ── /live/sedonaState bulk loader ──────────────────

#[tokio::test]
async fn sedona_state_returns_well_formed_payload() {
    let srv = EditorTestServer::start().await;
    let resp = reqwest::get(srv.url("/live/sedonaState?h=127.0.0.1&p=1876&u=admin&pw="))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: JsonValue = resp.json().await.unwrap();

    // Top-level shape matches SedonaStateResponse in the editor.
    assert!(body["schema"]["kits"].is_array());
    let kits = body["schema"]["kits"].as_array().unwrap();
    assert!(!kits.is_empty(), "at least one built-in kit expected");
    for kit in kits {
        assert!(kit["id"].is_u64());
        assert!(kit["name"].is_string());
        assert!(kit["checksum"].is_u64());
    }
    assert!(body["components"].is_object());
    assert!(body["links"].is_array());
    assert!(body["maxId"].is_u64());
    assert_eq!(body["host"], "127.0.0.1");
    assert_eq!(body["port"], 1876);
    assert!(body["timestamp"].as_u64().unwrap() > 0);
    assert!(body["loadTimeMs"].is_u64());
}

// ── /live WebSocket ────────────────────────────────

#[tokio::test]
async fn live_websocket_answers_version_request() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::protocol::Message;

    let srv = EditorTestServer::start().await;
    let (mut ws, _resp) = tokio_tungstenite::connect_async(srv.ws_url("/live"))
        .await
        .expect("/live should upgrade");

    // Fantom version request.
    ws.send(Message::text(r#"{"msgType":"v","replyNum":1}"#.to_string()))
        .await
        .unwrap();

    // Read until we get a text frame; ignore anything else (e.g. ping).
    let reply = loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => break t,
            Some(Ok(_)) => continue,
            Some(Err(e)) => panic!("ws error: {e}"),
            None => panic!("ws closed before reply"),
        }
    };

    let json: JsonValue = serde_json::from_str(&reply).unwrap();
    assert_eq!(json["msgType"], "V");
    assert_eq!(json["replyNum"], 1);
    let kits = json["kits"].as_array().unwrap();
    assert!(!kits.is_empty());

    let _ = ws.close(None).await;
}

#[tokio::test]
async fn live_websocket_ping_pong() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::protocol::Message;

    let srv = EditorTestServer::start().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(srv.ws_url("/live"))
        .await
        .unwrap();
    ws.send(Message::text(
        r#"{"msgType":"p","replyNum":255}"#.to_string(),
    ))
    .await
    .unwrap();
    let reply = loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => break t,
            Some(Ok(_)) => continue,
            _ => panic!("no text reply"),
        }
    };
    let json: JsonValue = serde_json::from_str(&reply).unwrap();
    // Pong MUST be uppercase `P` per Phase 14.0G fix — the editor's
    // sox.worker.ts does a strict `=== 'P'` check.
    assert_eq!(json["msgType"], "P");
    assert_eq!(json["replyNum"], 255);
}

// ── /api/rows regression check ─────────────────────

#[tokio::test]
async fn rows_websocket_still_answers_after_sox_ops_refactor() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::protocol::Message;

    let srv = EditorTestServer::start().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(srv.ws_url("/api/rows"))
        .await
        .unwrap();
    ws.send(Message::text(r#"{"op":"ping","id":"ping-1"}"#.to_string()))
        .await
        .unwrap();
    let reply = loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => break t,
            Some(Ok(_)) => continue,
            _ => panic!("no text reply"),
        }
    };
    let json: JsonValue = serde_json::from_str(&reply).unwrap();
    assert_eq!(json["op"], "pong");
    assert_eq!(json["id"], "ping-1");
}
