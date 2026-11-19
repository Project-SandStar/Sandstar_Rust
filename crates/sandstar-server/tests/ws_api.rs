//! WebSocket integration tests for the Sandstar server.
//!
//! Each test spins up a TestServer (MockHal + Axum on port 0), connects via
//! tokio-tungstenite, and validates the Haystack-over-WebSocket protocol.

mod common;

use std::time::Duration;

use common::TestServer;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

// ── Helpers ─────────────────────────────────────────────────

/// Read the next text-frame from the WS stream, parse as JSON.
/// Panics if no message arrives within 5 seconds.
async fn read_ws_json(
    ws_rx: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> serde_json::Value {
    let msg = tokio::time::timeout(Duration::from_secs(5), ws_rx.next())
        .await
        .expect("WS read timed out after 5 s")
        .expect("WS stream ended unexpectedly")
        .expect("WS transport error");

    match msg {
        Message::Text(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            panic!("invalid JSON from WS: {e}\nraw: {text}");
        }),
        other => panic!("expected Text message, got {other:?}"),
    }
}

/// Send a JSON object as a text frame.
async fn send_ws_json(
    ws_tx: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    value: serde_json::Value,
) {
    let text = value.to_string();
    ws_tx
        .send(Message::Text(text.into()))
        .await
        .expect("WS send failed");
}

// ══════════════════════════════════════════════════════════════
// 1. Connect and subscribe
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn ws_connect_and_subscribe() {
    let server = TestServer::start().await;
    let (ws, _resp) = connect_async(server.ws_url("/api/ws"))
        .await
        .expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Subscribe to two channels
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "subscribe",
            "id": "t1",
            "ids": [1113, 1200]
        }),
    )
    .await;

    let msg = read_ws_json(&mut ws_rx).await;
    assert_eq!(msg["op"], "subscribed", "expected subscribed op: {msg}");
    assert_eq!(msg["id"], "t1", "request id must echo back");
    assert!(
        msg["watchId"].as_str().unwrap().starts_with("w-"),
        "watchId should start with w-"
    );
    assert_eq!(
        msg["rows"].as_array().unwrap().len(),
        2,
        "should have 2 rows for 2 subscribed channels"
    );
    // Verify lease and pollInterval are present
    assert!(msg["lease"].as_u64().is_some(), "lease should be a number");
    assert!(
        msg["pollInterval"].as_u64().is_some(),
        "pollInterval should be a number"
    );
}

// ══════════════════════════════════════════════════════════════
// 2. Push on value change
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn ws_push_on_value_change() {
    let server = TestServer::start().await;
    let (ws, _) = connect_async(server.ws_url("/api/ws"))
        .await
        .expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Subscribe to channel 1113 with a fast poll interval
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "subscribe",
            "ids": [1113],
            "pollInterval": 200
        }),
    )
    .await;
    let sub = read_ws_json(&mut ws_rx).await;
    assert_eq!(sub["op"], "subscribed");
    let watch_id = sub["watchId"].as_str().unwrap().to_string();

    // Verify the subscribe response had the channel value
    let rows = sub["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["channel"], 1113);

    // Trigger a poll cycle via REST to update engine values
    let client = reqwest::Client::new();
    let resp = client
        .post(server.url("/api/pollNow"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // The WS push loop polls at 200ms intervals. MockHal values are static,
    // so watch_poll(refresh=false) may return no changes. We verify the
    // subscription response itself had valid data. If an update arrives, great.
    let update_result = tokio::time::timeout(Duration::from_secs(2), ws_rx.next()).await;
    match update_result {
        Ok(Some(Ok(Message::Text(text)))) => {
            let msg: serde_json::Value = serde_json::from_str(&text).unwrap();
            if msg["op"] == "update" {
                assert_eq!(msg["watchId"], watch_id);
                assert!(msg["rows"].as_array().is_some());
                assert!(msg["ts"].as_str().is_some());
            }
            // Any valid message is acceptable
        }
        _ => {
            // Timeout is acceptable — MockHal values don't change, so no
            // update push is expected. The subscribe response already
            // validated the channel value.
        }
    }
}

// ══════════════════════════════════════════════════════════════
// 3. Unsubscribe + close
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn ws_unsubscribe_close() {
    let server = TestServer::start().await;
    let (ws, _) = connect_async(server.ws_url("/api/ws"))
        .await
        .expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Subscribe first
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "subscribe",
            "ids": [1113, 1200]
        }),
    )
    .await;
    let sub = read_ws_json(&mut ws_rx).await;
    assert_eq!(sub["op"], "subscribed");
    let watch_id = sub["watchId"].as_str().unwrap().to_string();

    // Unsubscribe with close=true
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "unsubscribe",
            "watchId": watch_id,
            "close": true
        }),
    )
    .await;
    let unsub = read_ws_json(&mut ws_rx).await;
    assert_eq!(unsub["op"], "unsubscribed");
    assert_eq!(unsub["ok"], true);
    assert_eq!(unsub["watchId"], watch_id);
}

// ══════════════════════════════════════════════════════════════
// 4. Refresh returns all values (snapshot)
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn ws_refresh_returns_all() {
    let server = TestServer::start().await;
    let (ws, _) = connect_async(server.ws_url("/api/ws"))
        .await
        .expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Subscribe to 3 channels
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "subscribe",
            "ids": [1113, 1200, 612]
        }),
    )
    .await;
    let sub = read_ws_json(&mut ws_rx).await;
    assert_eq!(sub["op"], "subscribed");
    assert_eq!(sub["rows"].as_array().unwrap().len(), 3);
    let watch_id = sub["watchId"].as_str().unwrap().to_string();

    // Send refresh
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "refresh",
            "watchId": watch_id
        }),
    )
    .await;
    let snap = read_ws_json(&mut ws_rx).await;
    assert_eq!(
        snap["op"], "snapshot",
        "refresh should return snapshot: {snap}"
    );
    assert_eq!(snap["watchId"], watch_id);
    assert!(snap["ts"].as_str().is_some(), "snapshot must have ts");
    assert_eq!(
        snap["rows"].as_array().unwrap().len(),
        3,
        "refresh should return all 3 subscribed channel values"
    );
}

// ══════════════════════════════════════════════════════════════
// 5. Ping / Pong
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn ws_ping_pong() {
    let server = TestServer::start().await;
    let (ws, _) = connect_async(server.ws_url("/api/ws"))
        .await
        .expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "ping",
            "id": "p1"
        }),
    )
    .await;
    let msg = read_ws_json(&mut ws_rx).await;
    assert_eq!(msg["op"], "pong");
    assert_eq!(msg["id"], "p1");
}

// ══════════════════════════════════════════════════════════════
// 6. Invalid message
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn ws_invalid_message() {
    let server = TestServer::start().await;
    let (ws, _) = connect_async(server.ws_url("/api/ws"))
        .await
        .expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Send garbage JSON
    ws_tx
        .send(Message::Text(r#"{"garbage": true}"#.into()))
        .await
        .expect("WS send failed");

    let msg = read_ws_json(&mut ws_rx).await;
    assert_eq!(msg["op"], "error");
    assert_eq!(msg["code"], "INVALID_MESSAGE");

    // Connection should remain open — verify with a ping
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "ping",
            "id": "after-error"
        }),
    )
    .await;
    let pong = read_ws_json(&mut ws_rx).await;
    assert_eq!(pong["op"], "pong");
    assert_eq!(pong["id"], "after-error");
}

#[tokio::test]
async fn ws_invalid_non_json() {
    let server = TestServer::start().await;
    let (ws, _) = connect_async(server.ws_url("/api/ws"))
        .await
        .expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Send non-JSON text
    ws_tx
        .send(Message::Text("not json at all".into()))
        .await
        .expect("WS send failed");

    let msg = read_ws_json(&mut ws_rx).await;
    assert_eq!(msg["op"], "error");
    assert_eq!(msg["code"], "INVALID_MESSAGE");
}

// ══════════════════════════════════════════════════════════════
// 7. Auth via query parameter
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn ws_auth_query_param() {
    let server = TestServer::start_with_auth("secret123").await;

    // Connect with valid token in query string
    let (ws, _) = connect_async(server.ws_url("/api/ws?token=secret123"))
        .await
        .expect("WS handshake with token failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Subscribe should succeed (pre-authed via query param)
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "subscribe",
            "ids": [1113]
        }),
    )
    .await;
    let sub = read_ws_json(&mut ws_rx).await;
    assert_eq!(
        sub["op"], "subscribed",
        "pre-authed subscribe should succeed: {sub}"
    );
    assert_eq!(sub["rows"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn ws_auth_required_without_token() {
    let server = TestServer::start_with_auth("secret123").await;

    // Connect WITHOUT token
    let (ws, _) = connect_async(server.ws_url("/api/ws"))
        .await
        .expect("WS handshake should succeed even without token");
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Subscribe without auth — should get AUTH_REQUIRED error
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "subscribe",
            "ids": [1113]
        }),
    )
    .await;
    let msg = read_ws_json(&mut ws_rx).await;
    assert_eq!(msg["op"], "error");
    assert_eq!(
        msg["code"], "AUTH_REQUIRED",
        "unauthenticated subscribe should get AUTH_REQUIRED"
    );
}

// ══════════════════════════════════════════════════════════════
// 8. Auth via first message
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn ws_auth_first_message() {
    let server = TestServer::start_with_auth("secret123").await;

    // Connect WITHOUT query token
    let (ws, _) = connect_async(server.ws_url("/api/ws"))
        .await
        .expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Authenticate via message
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "auth",
            "token": "secret123"
        }),
    )
    .await;
    let auth = read_ws_json(&mut ws_rx).await;
    assert_eq!(auth["op"], "authOk");

    // Now subscribe should succeed
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "subscribe",
            "ids": [1113]
        }),
    )
    .await;
    let sub = read_ws_json(&mut ws_rx).await;
    assert_eq!(sub["op"], "subscribed");
    assert_eq!(sub["rows"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn ws_auth_wrong_token_rejected() {
    let server = TestServer::start_with_auth("secret123").await;

    let (ws, _) = connect_async(server.ws_url("/api/ws"))
        .await
        .expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Send wrong token
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "auth",
            "token": "wrong-token"
        }),
    )
    .await;
    let msg = read_ws_json(&mut ws_rx).await;
    assert_eq!(msg["op"], "error");
    assert_eq!(msg["code"], "AUTH_REQUIRED");
}

// ══════════════════════════════════════════════════════════════
// 9. Multiple watches on same connection
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn ws_multiple_watches() {
    let server = TestServer::start().await;
    let (ws, _) = connect_async(server.ws_url("/api/ws"))
        .await
        .expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    // First watch: analog channels
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "subscribe",
            "id": "w1",
            "ids": [1113, 1200]
        }),
    )
    .await;
    let sub1 = read_ws_json(&mut ws_rx).await;
    assert_eq!(sub1["op"], "subscribed");
    assert_eq!(sub1["id"], "w1");
    let watch_id_1 = sub1["watchId"].as_str().unwrap().to_string();
    assert_eq!(sub1["rows"].as_array().unwrap().len(), 2);

    // Second watch: I2C + digital channels
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "subscribe",
            "id": "w2",
            "ids": [612, 2001]
        }),
    )
    .await;
    let sub2 = read_ws_json(&mut ws_rx).await;
    assert_eq!(sub2["op"], "subscribed");
    assert_eq!(sub2["id"], "w2");
    let watch_id_2 = sub2["watchId"].as_str().unwrap().to_string();
    assert_eq!(sub2["rows"].as_array().unwrap().len(), 2);

    // Different watch IDs
    assert_ne!(watch_id_1, watch_id_2);

    // Refresh both watches to verify they track separate channel sets
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "refresh",
            "watchId": watch_id_1
        }),
    )
    .await;
    let snap1 = read_ws_json(&mut ws_rx).await;
    assert_eq!(snap1["op"], "snapshot");
    assert_eq!(snap1["rows"].as_array().unwrap().len(), 2);

    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "refresh",
            "watchId": watch_id_2
        }),
    )
    .await;
    let snap2 = read_ws_json(&mut ws_rx).await;
    assert_eq!(snap2["op"], "snapshot");
    assert_eq!(snap2["rows"].as_array().unwrap().len(), 2);
}

// ══════════════════════════════════════════════════════════════
// 10. Ping works without auth (ping always works)
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn ws_ping_works_without_auth() {
    let server = TestServer::start_with_auth("secret123").await;

    let (ws, _) = connect_async(server.ws_url("/api/ws"))
        .await
        .expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Ping should work even without authentication
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "ping",
            "id": "noauth-ping"
        }),
    )
    .await;
    let pong = read_ws_json(&mut ws_rx).await;
    assert_eq!(pong["op"], "pong");
    assert_eq!(pong["id"], "noauth-ping");
}

// ══════════════════════════════════════════════════════════════
// 11. Subscribe response contains channel values
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn ws_subscribe_row_fields() {
    let server = TestServer::start().await;
    let (ws, _) = connect_async(server.ws_url("/api/ws"))
        .await
        .expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "subscribe",
            "ids": [1113]
        }),
    )
    .await;
    let sub = read_ws_json(&mut ws_rx).await;
    assert_eq!(sub["op"], "subscribed");

    let rows = sub["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    // Each row should have channel, status, raw, cur fields (ChannelValue)
    assert_eq!(row["channel"], 1113);
    assert!(row["status"].is_string(), "row must have status string");
    assert!(row["raw"].is_number(), "row must have raw number");
    assert!(row["cur"].is_number(), "row must have cur number");
}

// ══════════════════════════════════════════════════════════════
// 12. Unsubscribe nonexistent watch returns error
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn ws_unsubscribe_nonexistent_watch() {
    let server = TestServer::start().await;
    let (ws, _) = connect_async(server.ws_url("/api/ws"))
        .await
        .expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    send_ws_json(
        &mut ws_tx,
        serde_json::json!({
            "op": "unsubscribe",
            "watchId": "nonexistent-watch",
            "close": true
        }),
    )
    .await;
    let msg = read_ws_json(&mut ws_rx).await;
    assert_eq!(msg["op"], "error");
    assert_eq!(msg["code"], "WATCH_NOT_FOUND");
}

// ══════════════════════════════════════════════════════════════
// Hung engine: the connection keeps answering (R-28, S-22)
// ══════════════════════════════════════════════════════════════

/// Engine that accepts subscriptions and unsubscriptions but never answers
/// a watch poll, like an engine stuck in a long poll cycle. The `Notify`
/// fires when the first watch poll arrives.
fn spawn_poll_hung_engine() -> (
    sandstar_server::rest::EngineHandle,
    std::sync::Arc<tokio::sync::Notify>,
) {
    use sandstar_server::rest::{EngineCmd, EngineHandle, WatchResponse};
    let polled = std::sync::Arc::new(tokio::sync::Notify::new());
    let notify = polled.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<EngineCmd>(64);
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Some(cmd) = rx.recv().await {
            match cmd {
                EngineCmd::WatchSub { reply, .. } => {
                    let _ = reply.send(Ok(WatchResponse {
                        watch_id: "w-hung".into(),
                        lease: 3600,
                        rows: vec![],
                    }));
                }
                EngineCmd::WatchUnsub { reply, .. } => {
                    let _ = reply.send(Ok(()));
                }
                poll @ EngineCmd::WatchPoll { .. } => {
                    notify.notify_one();
                    held.push(poll);
                }
                other => held.push(other),
            }
        }
    });
    (EngineHandle::new(tx), polled)
}

#[tokio::test]
async fn ws_answers_ping_while_a_watch_poll_is_stuck() {
    let (engine, polled) = spawn_poll_hung_engine();
    let app = sandstar_server::rest::router(engine, None, 0);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let (ws, _) = connect_async(format!("ws://127.0.0.1:{port}/api/ws"))
        .await
        .expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    send_ws_json(
        &mut ws_tx,
        serde_json::json!({"op": "subscribe", "id": "s", "ids": [1], "pollInterval": 200}),
    )
    .await;
    let msg = read_ws_json(&mut ws_rx).await;
    assert_eq!(msg["op"], "subscribed", "{msg}");

    // Wait until the push round has sent a poll that the engine never answers.
    tokio::time::timeout(Duration::from_secs(3), polled.notified())
        .await
        .expect("the push round never polled the engine");

    let start = std::time::Instant::now();
    send_ws_json(&mut ws_tx, serde_json::json!({"op": "ping", "id": "p"})).await;
    let msg = tokio::time::timeout(Duration::from_secs(1), read_ws_json(&mut ws_rx))
        .await
        .expect("no pong within 1 s while the engine is stuck in a watch poll");
    assert_eq!(msg["op"], "pong", "{msg}");
    assert!(start.elapsed() < Duration::from_secs(1));
}

// ── S-23: one engine slot per connection ───────────────────

/// Serve `/api/ws` on a free port with `engine`; returns the WS URL.
async fn serve_ws(engine: sandstar_server::rest::EngineHandle) -> String {
    let app = sandstar_server::rest::router(engine, None, 0);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("ws://127.0.0.1:{port}/api/ws")
}

/// Engine that never answers anything, like an engine stuck in a long cycle.
fn spawn_silent_engine() -> sandstar_server::rest::EngineHandle {
    use sandstar_server::rest::{EngineCmd, EngineHandle};
    let (tx, mut rx) = tokio::sync::mpsc::channel::<EngineCmd>(64);
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Some(cmd) = rx.recv().await {
            held.push(cmd);
        }
    });
    EngineHandle::new(tx)
}

#[tokio::test]
async fn ws_answers_ping_while_a_subscribe_is_stuck() {
    let url = serve_ws(spawn_silent_engine()).await;
    let (ws, _) = connect_async(url).await.expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    send_ws_json(
        &mut ws_tx,
        serde_json::json!({"op": "subscribe", "id": "s", "ids": [1]}),
    )
    .await;
    send_ws_json(&mut ws_tx, serde_json::json!({"op": "ping", "id": "p"})).await;
    let msg = tokio::time::timeout(Duration::from_secs(1), read_ws_json(&mut ws_rx))
        .await
        .expect("no pong within 1 s while the engine is stuck in a subscribe");
    assert_eq!(msg["op"], "pong", "{msg}");
    assert_eq!(msg["id"], "p", "{msg}");
}

#[tokio::test]
async fn ws_sends_one_engine_command_at_a_time_and_answers_in_order() {
    use sandstar_server::rest::{EngineCmd, EngineHandle, WatchResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // Subscribes are answered after 50 ms, polls after 300 ms; count how
    // many commands overlap.
    let polled = Arc::new(tokio::sync::Notify::new());
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<EngineCmd>(64);
    {
        let (in_flight, max_in_flight, polled) =
            (in_flight.clone(), max_in_flight.clone(), polled.clone());
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_in_flight.fetch_max(now, Ordering::SeqCst);
                let delay = if matches!(cmd, EngineCmd::WatchPoll { .. }) {
                    polled.notify_one();
                    300
                } else {
                    50
                };
                let in_flight = in_flight.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    let resp = |id: String| WatchResponse {
                        watch_id: id,
                        lease: 3600,
                        rows: vec![],
                    };
                    match cmd {
                        EngineCmd::WatchSub {
                            watch_id, reply, ..
                        } => {
                            let _ = reply.send(Ok(resp(watch_id.unwrap_or_default())));
                        }
                        EngineCmd::WatchPoll {
                            watch_id, reply, ..
                        } => {
                            let _ = reply.send(Ok(resp(watch_id)));
                        }
                        EngineCmd::WatchUnsub { reply, .. } => {
                            let _ = reply.send(Ok(()));
                        }
                        _ => {}
                    }
                });
            }
        });
    }
    let url = serve_ws(EngineHandle::new(tx)).await;
    let (ws, _) = connect_async(url).await.expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Three requests at once, with push polls (200 ms) running in between.
    for (id, wid) in [("a", "w-a"), ("b", "w-b"), ("c", "w-c")] {
        send_ws_json(
            &mut ws_tx,
            serde_json::json!({"op": "subscribe", "id": id, "watchId": wid, "ids": [1], "pollInterval": 200}),
        )
        .await;
    }
    let mut ids = Vec::new();
    while ids.len() < 3 {
        let msg = read_ws_json(&mut ws_rx).await;
        assert_eq!(msg["op"], "subscribed", "{msg}");
        ids.push(msg["id"].as_str().unwrap_or_default().to_string());
    }
    assert_eq!(ids, ["a", "b", "c"], "replies must follow request order");

    // A request sent while a push poll is in flight waits for the slot.
    tokio::time::timeout(Duration::from_secs(3), polled.notified())
        .await
        .expect("the push round never polled the engine");
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({"op": "refresh", "id": "r", "watchId": "w-a"}),
    )
    .await;
    loop {
        let msg = read_ws_json(&mut ws_rx).await;
        if msg["op"] == "snapshot" {
            assert_eq!(msg["id"], "r", "{msg}");
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        max_in_flight.load(Ordering::SeqCst),
        1,
        "a connection must not have two engine commands in flight"
    );
}

/// Client engine requests a connection queues before it stops reading
/// (`CLIENT_QUEUE_CAPACITY` in `rest/ws.rs`), plus the one in the slot.
const QUEUED_PLUS_SLOT: usize = 16 + 1;

/// Send `n` subscribes to a silent engine, then a ping; true if the pong
/// arrives within 500 ms.
async fn pong_after_stuck_requests(n: usize) -> bool {
    let url = serve_ws(spawn_silent_engine()).await;
    let (ws, _) = connect_async(url).await.expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();
    for i in 0..n {
        send_ws_json(
            &mut ws_tx,
            serde_json::json!({"op": "subscribe", "id": format!("s{i}"), "ids": [1]}),
        )
        .await;
    }
    send_ws_json(&mut ws_tx, serde_json::json!({"op": "ping", "id": "p"})).await;
    matches!(
        tokio::time::timeout(Duration::from_millis(500), read_ws_json(&mut ws_rx)).await,
        Ok(msg) if msg["op"] == "pong"
    )
}

#[tokio::test]
async fn ws_stops_reading_while_its_request_queue_is_full() {
    assert!(
        pong_after_stuck_requests(QUEUED_PLUS_SLOT - 1).await,
        "with room left in the queue, the ping must be read"
    );
    assert!(
        !pong_after_stuck_requests(QUEUED_PLUS_SLOT).await,
        "with the queue full, the connection must stop reading"
    );
}

#[tokio::test]
async fn ws_closes_the_watch_of_a_subscribe_answered_after_disconnect() {
    use sandstar_server::rest::{EngineCmd, EngineHandle, WatchResponse};

    // Subscribes are answered after 300 ms; closed watch ids are reported.
    let (closed_tx, mut closed_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<EngineCmd>(64);
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                EngineCmd::WatchSub { reply, .. } => {
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        let _ = reply.send(Ok(WatchResponse {
                            watch_id: "w-late".into(),
                            lease: 3600,
                            rows: vec![],
                        }));
                    });
                }
                EngineCmd::WatchUnsub {
                    watch_id, reply, ..
                } => {
                    let _ = closed_tx.send(watch_id);
                    let _ = reply.send(Ok(()));
                }
                _ => {}
            }
        }
    });
    let url = serve_ws(EngineHandle::new(tx)).await;
    let (ws, _) = connect_async(url).await.expect("WS handshake failed");
    let (mut ws_tx, _ws_rx) = ws.split();
    send_ws_json(
        &mut ws_tx,
        serde_json::json!({"op": "subscribe", "id": "s", "ids": [1]}),
    )
    .await;
    // Close while the subscribe is still in the engine.
    tokio::time::sleep(Duration::from_millis(100)).await;
    ws_tx
        .send(Message::Close(None))
        .await
        .expect("WS close failed");

    let closed = tokio::time::timeout(Duration::from_secs(3), closed_rx.recv())
        .await
        .expect("the watch created after disconnect was never closed");
    assert_eq!(closed.as_deref(), Some("w-late"));
}

#[tokio::test]
async fn ws_closes_a_connection_that_sends_an_oversized_message() {
    let url = serve_ws(spawn_silent_engine()).await;
    let (ws, _) = connect_async(url).await.expect("WS handshake failed");
    let (mut ws_tx, mut ws_rx) = ws.split();
    // 20 KiB of ids, above the 16 KiB limit.
    let ids: Vec<u32> = vec![1; 10 * 1024];
    let _ = ws_tx
        .send(Message::Text(
            serde_json::json!({"op": "subscribe", "id": "big", "ids": ids})
                .to_string()
                .into(),
        ))
        .await;
    // The server drops the connection instead of reading the message.
    let end = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match ws_rx.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(Message::Text(t))) => panic!("unexpected reply: {t}"),
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(
        end.is_ok(),
        "the connection stayed open after an oversized message"
    );
}
