//! Phase 5.8 — Mock Production Soak Tests
//!
//! These integration tests simulate long-running production workloads using
//! MockHal and the existing TestServer harness. Each test exercises hundreds
//! to thousands of poll cycles and REST/WebSocket interactions to validate
//! stability, resource cleanup, and error recovery — without requiring real
//! hardware.
//!
//! NOTE: The global `metrics` counters (poll_count, hal_errors, etc.) are
//! static atomics shared across all test processes and NOT reset between
//! tests. These soak tests therefore validate behavior through REST API
//! responses (history, channel reads, watch responses) rather than relying
//! on exact metric values.
//!
//! Target: each test completes in < 10 seconds.

mod common;

use std::time::Duration;

use common::TestServer;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

// ══════════════════════════════════════════════════════════════
// 1. Sustained polling — 1000 PollNow cycles with stability checks
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_soak_1000_polls_stable_memory() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    for batch in 0..10u64 {
        // Fire 100 PollNow requests per batch
        for _ in 0..100 {
            let resp = client
                .post(server.url("/api/pollNow"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "PollNow should succeed");
        }

        // Verify all channels still respond after each batch
        let channels: Vec<serde_json::Value> = client
            .get(server.url("/api/channels"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            channels.len(),
            5,
            "all 5 demo channels must survive batch {batch}"
        );

        // Verify status endpoint is healthy
        let status: serde_json::Value = client
            .get(server.url("/api/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status["channelCount"], 5);
        assert_eq!(status["pollCount"], 3);
    }

    // History ring buffer: should have entries but capped at 100 per channel
    // (test harness uses HistoryStore::new(100))
    let history: Vec<serde_json::Value> = client
        .get(server.url("/api/history/1113?limit=200"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        history.len() <= 100,
        "history ring buffer should cap at 100, got {}",
        history.len()
    );
    assert!(
        !history.is_empty(),
        "history should have entries after 1000 polls"
    );

    // All polled channels should have history
    for ch_id in [1113, 1200, 612] {
        let h: Vec<serde_json::Value> = client
            .get(server.url(&format!("/api/history/{ch_id}?limit=200")))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            !h.is_empty(),
            "channel {ch_id} should have history entries after 1000 polls"
        );
        assert!(
            h.len() <= 100,
            "channel {ch_id} history should be capped at 100, got {}",
            h.len()
        );
    }

    // Verify channels are still readable
    for ch_id in [1113, 1200, 612, 2001, 2002] {
        let resp = client
            .get(server.url(&format!("/api/read?id={ch_id}")))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "channel {ch_id} should be readable after 1000 polls"
        );
    }
}

// ══════════════════════════════════════════════════════════════
// 2. Watch lifecycle under sustained load
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_soak_watch_lifecycle_under_load() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Create 10 watches, each subscribing to 2 channels
    let mut watch_ids = Vec::new();
    for _ in 0..10 {
        let resp = client
            .post(server.url("/api/watchSub"))
            .json(&serde_json::json!({ "ids": [1113, 1200] }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let wid = body["watchId"].as_str().unwrap().to_string();
        assert!(wid.starts_with("w-"), "watch ID should start with w-");
        watch_ids.push(wid);
    }

    // Verify all 10 watches can be polled (functional check, not metric)
    for wid in &watch_ids {
        let resp = client
            .post(server.url("/api/watchPoll"))
            .json(&serde_json::json!({ "watchId": wid, "refresh": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["rows"].as_array().unwrap().len(), 2);
    }

    // Poll 500 cycles
    for _ in 0..500 {
        let resp = client
            .post(server.url("/api/pollNow"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // Unsubscribe the first 5 watches
    for wid in &watch_ids[..5] {
        let resp = client
            .post(server.url("/api/watchUnsub"))
            .json(&serde_json::json!({ "watchId": wid, "close": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // Poll 500 more cycles — remaining watches should still work
    for _ in 0..500 {
        let resp = client
            .post(server.url("/api/pollNow"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // Poll remaining watches for data — they should return successfully
    for wid in &watch_ids[5..] {
        let resp = client
            .post(server.url("/api/watchPoll"))
            .json(&serde_json::json!({ "watchId": wid, "refresh": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            body["rows"].as_array().unwrap().len(),
            2,
            "remaining watch should still track 2 channels"
        );
    }

    // Closed watches should return errors
    for wid in &watch_ids[..5] {
        let resp = client
            .post(server.url("/api/watchPoll"))
            .json(&serde_json::json!({ "watchId": wid, "refresh": true }))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().as_u16() >= 400,
            "closed watch '{}' poll should return error, got {}",
            wid,
            resp.status()
        );
    }
}

// ══════════════════════════════════════════════════════════════
// 3. Concurrent REST requests during continuous polling
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_soak_concurrent_rest_during_poll() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Spawn a background task that fires continuous polls
    let poll_url = server.url("/api/pollNow");
    let poll_client = client.clone();
    let poll_task = tokio::spawn(async move {
        for _ in 0..200 {
            let resp = poll_client.post(&poll_url).send().await.unwrap();
            assert_eq!(resp.status(), 200);
        }
    });

    // Concurrently send 200 REST requests (mixed endpoints)
    let endpoints = [
        "/api/read?id=1113",
        "/api/channels",
        "/api/polls",
        "/api/status",
        "/api/metrics",
        "/api/about",
        "/api/read",
        "/api/history/1113",
        "/api/tables",
        "/health",
    ];

    let mut handles = Vec::new();
    for i in 0..200 {
        let url = server.url(endpoints[i % endpoints.len()]);
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            let resp = tokio::time::timeout(Duration::from_secs(5), c.get(&url).send())
                .await
                .expect("REST request timed out — possible deadlock")
                .unwrap();
            assert_eq!(
                resp.status(),
                200,
                "GET {} failed with status {}",
                url,
                resp.status()
            );
        }));
    }

    // Wait for all REST requests to complete
    for h in handles {
        h.await.unwrap();
    }

    // Wait for poll task
    poll_task.await.unwrap();

    // Final sanity: server still alive and responding
    let status: serde_json::Value = client
        .get(server.url("/api/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        status["channelCount"], 5,
        "server should still report 5 channels"
    );
}

// ══════════════════════════════════════════════════════════════
// 4. HAL error recovery — inject errors every 10th poll
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_soak_hal_error_recovery() {
    use sandstar_engine::channel::{Channel, ChannelDirection, ChannelType};
    use sandstar_engine::value::ValueConv;
    use sandstar_engine::Engine;
    use sandstar_hal::mock::MockHal;
    use sandstar_hal::HalError;

    // Build a custom engine with an error-prone channel
    let hal = MockHal::new();

    // Pre-load 200 reads for channel 1113 (analog device=0, addr=0):
    // every 10th read is an error, the rest are Ok.
    for i in 0..200 {
        if i % 10 == 9 {
            hal.set_analog(
                0,
                0,
                Err(HalError::Timeout {
                    device: 0,
                    address: 0,
                }),
            );
        } else {
            hal.set_analog(0, 0, Ok(2048.0 + i as f64));
        }
    }
    // Channel 1200 always succeeds (sticky mode from initial value)
    hal.set_analog(0, 1, Ok(3276.0));

    let mut engine = Engine::new(hal);
    let ch1 = Channel::new(
        1113,
        ChannelType::Analog,
        ChannelDirection::In,
        0,
        0,
        false,
        ValueConv::default(),
        "AI1 Error-Prone",
    );
    let ch2 = Channel::new(
        1200,
        ChannelType::Analog,
        ChannelDirection::In,
        0,
        1,
        false,
        ValueConv::default(),
        "AI2 Stable",
    );
    let _ = engine.channels.add(ch1);
    let _ = engine.channels.add(ch2);
    let _ = engine.polls.add(1113);
    let _ = engine.polls.add(1200);

    let server = TestServer::start_with(engine).await;
    let client = reqwest::Client::new();

    // Fire 200 PollNow cycles
    for _ in 0..200 {
        let resp = client
            .post(server.url("/api/pollNow"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "PollNow should succeed even when channels error"
        );
    }

    // Verify channel 1200 is unaffected — should be Ok with stable value
    let resp = client
        .get(server.url("/api/read?id=1200"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body[0]["status"], "Ok", "stable channel should stay Ok");

    // Verify channel 1113 is still queryable after intermittent errors.
    // After all 200 queued reads are consumed, MockHal sticky mode returns
    // the last good value, so the channel should recover.
    let resp = client
        .get(server.url("/api/read?id=1113"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(
        body[0]["channel"].as_u64().unwrap(),
        1113,
        "channel 1113 should still be queryable after errors"
    );

    // Server should still list both channels
    let channels: Vec<serde_json::Value> = client
        .get(server.url("/api/channels"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        channels.len(),
        2,
        "both channels should survive error injection"
    );

    // History should contain entries for both channels (some may be error status)
    let h1: Vec<serde_json::Value> = client
        .get(server.url("/api/history/1113?limit=200"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let h2: Vec<serde_json::Value> = client
        .get(server.url("/api/history/1200?limit=200"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !h1.is_empty(),
        "error-prone channel should still have history"
    );
    assert!(!h2.is_empty(), "stable channel should have history");
}

// ══════════════════════════════════════════════════════════════
// 5. Rapid config reload under load
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_soak_rapid_config_reload() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Spawn background polling
    let poll_url = server.url("/api/pollNow");
    let poll_client = client.clone();
    let poll_task = tokio::spawn(async move {
        for _ in 0..200 {
            let resp = poll_client.post(&poll_url).send().await.unwrap();
            assert_eq!(resp.status(), 200);
        }
    });

    // Fire 20 rapid reload requests. In demo mode, these will fail (no config dir)
    // but should NOT crash the server.
    for _ in 0..20 {
        let resp = client.post(server.url("/api/reload")).send().await.unwrap();
        // Demo mode returns an error — that's fine, we just verify no crash
        assert!(
            resp.status().as_u16() >= 400,
            "reload in demo mode should return error"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body["err"].as_str().unwrap().contains("demo mode"),
            "should indicate demo mode limitation"
        );
    }

    // Wait for polling to finish
    poll_task.await.unwrap();

    // Server still healthy — verify all channels respond
    let channels: Vec<serde_json::Value> = client
        .get(server.url("/api/channels"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        channels.len(),
        5,
        "all 5 channels should survive rapid reloads"
    );

    // Status endpoint still works
    let status: serde_json::Value = client
        .get(server.url("/api/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["channelCount"], 5);
    assert!(status["uptimeSecs"].as_u64().is_some());
}

// ══════════════════════════════════════════════════════════════
// 6. WebSocket subscribe storm — 10 connections, poll 200 cycles
// ══════════════════════════════════════════════════════════════

/// Read the next text-frame JSON from a WS stream (5s timeout).
async fn read_ws_json(
    ws_rx: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> serde_json::Value {
    let msg = tokio::time::timeout(Duration::from_secs(5), ws_rx.next())
        .await
        .expect("WS read timed out")
        .expect("WS stream ended")
        .expect("WS transport error");
    match msg {
        Message::Text(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            panic!("invalid JSON from WS: {e}\nraw: {text}");
        }),
        other => panic!("expected Text message, got {other:?}"),
    }
}

/// Send a JSON value as a WS text frame.
async fn send_ws_json(
    ws_tx: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    value: serde_json::Value,
) {
    ws_tx
        .send(Message::Text(value.to_string().into()))
        .await
        .expect("WS send failed");
}

#[tokio::test]
async fn test_soak_websocket_subscribe_storm() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Open 10 WebSocket connections, each subscribing to all input channels
    let channel_ids = [1113, 1200, 612];
    let mut connections = Vec::new();

    for i in 0..10 {
        let (ws, _) = connect_async(server.ws_url("/api/ws"))
            .await
            .unwrap_or_else(|e| panic!("WS connect {i} failed: {e}"));
        let (mut ws_tx, mut ws_rx) = ws.split();

        send_ws_json(
            &mut ws_tx,
            serde_json::json!({
                "op": "subscribe",
                "id": format!("sub-{i}"),
                "ids": channel_ids,
                "pollInterval": 200
            }),
        )
        .await;

        let sub = read_ws_json(&mut ws_rx).await;
        assert_eq!(sub["op"], "subscribed", "WS {i} subscribe failed: {sub}");
        assert_eq!(
            sub["rows"].as_array().unwrap().len(),
            3,
            "WS {i} should get 3 rows"
        );
        let watch_id = sub["watchId"].as_str().unwrap().to_string();
        connections.push((ws_tx, ws_rx, watch_id));
    }

    // Fire 200 poll cycles via REST (this also triggers WS push updates)
    for _ in 0..200 {
        let resp = client
            .post(server.url("/api/pollNow"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // Verify all connections can still refresh
    for (i, (ws_tx, ws_rx, watch_id)) in connections.iter_mut().enumerate() {
        send_ws_json(
            ws_tx,
            serde_json::json!({
                "op": "refresh",
                "watchId": watch_id.clone()
            }),
        )
        .await;

        // Read messages until we get a snapshot (skip any pending update pushes)
        let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let msg = read_ws_json(ws_rx).await;
                if msg["op"] == "snapshot" {
                    return msg;
                }
                // Skip update messages
            }
        })
        .await
        .unwrap_or_else(|_| panic!("WS {i} snapshot timed out"));

        assert_eq!(
            snapshot["rows"].as_array().unwrap().len(),
            3,
            "WS {i} refresh should return 3 rows"
        );
    }

    // Disconnect the first 5 connections by closing them
    for (ws_tx, _ws_rx, watch_id) in connections.drain(..5) {
        let mut tx = ws_tx;
        // Send unsubscribe + close
        send_ws_json(
            &mut tx,
            serde_json::json!({
                "op": "unsubscribe",
                "watchId": watch_id,
                "close": true
            }),
        )
        .await;
        // Close the WebSocket
        let _ = tx.close().await;
    }

    // Small delay for the server to process disconnections
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Remaining 5 connections should still work
    for (i, (ws_tx, ws_rx, _watch_id)) in connections.iter_mut().enumerate() {
        send_ws_json(
            ws_tx,
            serde_json::json!({
                "op": "ping",
                "id": format!("alive-{i}")
            }),
        )
        .await;

        // Read messages until we get a pong (skip any pending updates)
        let pong = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let msg = read_ws_json(ws_rx).await;
                if msg["op"] == "pong" {
                    return msg;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("WS {i} pong timed out"));

        assert_eq!(pong["id"], format!("alive-{i}"));
    }

    // Final sanity: REST API still works after all the WS activity
    let status: serde_json::Value = client
        .get(server.url("/api/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["channelCount"], 5, "server should still be healthy");
}
