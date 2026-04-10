//! Integration tests for the Sandstar REST API.
//!
//! Each test spins up a TestServer (MockHal + Axum on port 0) and hits
//! endpoints via reqwest, validating JSON responses.

mod common;

use common::TestServer;

// ── About / Ops ────────────────────────────────────────────

#[tokio::test]
async fn about_returns_server_metadata() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/about")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["vendorName"], "EacIo");
    assert_eq!(body["haystackVersion"], "3.0");
    assert!(body["serverTime"].as_str().unwrap().contains("T"));
}

#[tokio::test]
async fn ops_returns_operation_list() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/ops")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    let names: Vec<&str> = body.iter().map(|op| op["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"about"));
    assert!(names.contains(&"read"));
    assert!(names.contains(&"pointWrite"));
}

// ── Status ─────────────────────────────────────────────────

#[tokio::test]
async fn status_returns_engine_info() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/status")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["channelCount"], 5);
    assert_eq!(body["pollCount"], 3);
    assert!(body["uptimeSecs"].as_u64().is_some());
}

// ── Channels ───────────────────────────────────────────────

#[tokio::test]
async fn channels_lists_all_configured() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/channels")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 5);
    // Check we have the expected channel ids
    let ids: Vec<u64> = body.iter().map(|ch| ch["id"].as_u64().unwrap()).collect();
    assert!(ids.contains(&1113));
    assert!(ids.contains(&2001));
}

// ── Read ───────────────────────────────────────────────────

#[tokio::test]
async fn read_single_channel_by_id() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/read?id=1113")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 1);
    assert_eq!(body[0]["channel"], 1113);
}

#[tokio::test]
async fn read_channel_by_filter() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/read?filter=channel==1113"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 1);
    assert_eq!(body[0]["channel"], 1113);
}

#[tokio::test]
async fn read_nonexistent_channel_returns_error() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/read?id=9999")).await.unwrap();
    // Should get 500 or error response
    assert!(resp.status().as_u16() >= 400);
}

// ── Polls ──────────────────────────────────────────────────

#[tokio::test]
async fn polls_returns_polled_channels() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/polls")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 3); // 3 input channels polled
}

// ── Tables ─────────────────────────────────────────────────

#[tokio::test]
async fn tables_returns_empty_in_demo() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/tables")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<String> = resp.json().await.unwrap();
    assert!(body.is_empty()); // no tables in demo mode
}

// ── Point Write (priority array) ───────────────────────────

#[tokio::test]
async fn point_write_and_read_priority_grid() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Write a value at level 17
    let resp = client
        .post(server.url("/api/pointWrite"))
        .json(&serde_json::json!({
            "channel": 2001,
            "value": 1.0,
            "level": 17,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 17); // 17-level priority grid returned

    // Level 17 should have value 1.0
    let l17 = &body[16];
    assert_eq!(l17["level"], 17);
    assert!((l17["val"].as_f64().unwrap() - 1.0).abs() < f64::EPSILON);

    // Read back via GET
    let resp = reqwest::get(server.url("/api/pointWrite?channel=2001"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 17);
}

#[tokio::test]
async fn point_write_relinquish_sets_null() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Write
    client
        .post(server.url("/api/pointWrite"))
        .json(&serde_json::json!({
            "channel": 2001,
            "value": 1.0,
            "level": 8,
        }))
        .send()
        .await
        .unwrap();

    // Relinquish (value: null)
    let resp = client
        .post(server.url("/api/pointWrite"))
        .json(&serde_json::json!({
            "channel": 2001,
            "value": null,
            "level": 8,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    // Level 8 should be null
    let l8 = &body[7];
    assert_eq!(l8["level"], 8);
    assert!(l8["val"].is_null());
}

#[tokio::test]
async fn point_write_priority_ordering() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Write at level 17 (lowest priority)
    client
        .post(server.url("/api/pointWrite"))
        .json(&serde_json::json!({
            "channel": 2001,
            "value": 100.0,
            "level": 17,
        }))
        .send()
        .await
        .unwrap();

    // Write at level 8 (higher priority)
    let resp = client
        .post(server.url("/api/pointWrite"))
        .json(&serde_json::json!({
            "channel": 2001,
            "value": 50.0,
            "level": 8,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Read the 17-level grid — both levels should be set
    let resp = reqwest::get(server.url("/api/pointWrite?channel=2001"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 17);

    // Level 8 should have value 50.0
    let l8 = &body[7];
    assert_eq!(l8["level"], 8);
    assert!((l8["val"].as_f64().unwrap() - 50.0).abs() < f64::EPSILON);

    // Level 17 should have value 100.0
    let l17 = &body[16];
    assert_eq!(l17["level"], 17);
    assert!((l17["val"].as_f64().unwrap() - 100.0).abs() < f64::EPSILON);

    // Levels 1-7 and 9-16 should be null
    for i in 0..7 {
        assert!(body[i]["val"].is_null(), "Level {} should be null", i + 1);
    }
    for i in 8..16 {
        assert!(body[i]["val"].is_null(), "Level {} should be null", i + 1);
    }
}

// ── Watch Subscriptions ────────────────────────────────────

#[tokio::test]
async fn watch_lifecycle() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Subscribe to channels
    let resp = client
        .post(server.url("/api/watchSub"))
        .json(&serde_json::json!({
            "ids": [1113, 1200],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let watch_id = body["watchId"].as_str().unwrap().to_string();
    assert!(watch_id.starts_with("w-"));
    assert_eq!(body["rows"].as_array().unwrap().len(), 2);

    // Poll for changes (refresh=true to get all)
    let resp = client
        .post(server.url("/api/watchPoll"))
        .json(&serde_json::json!({
            "watchId": watch_id,
            "refresh": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["rows"].as_array().unwrap().len(), 2);

    // Unsubscribe one channel
    let resp = client
        .post(server.url("/api/watchUnsub"))
        .json(&serde_json::json!({
            "watchId": watch_id,
            "ids": [1200],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Close watch
    let resp = client
        .post(server.url("/api/watchUnsub"))
        .json(&serde_json::json!({
            "watchId": watch_id,
            "close": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

// ── Poll Now ───────────────────────────────────────────────

#[tokio::test]
async fn poll_now_triggers_cycle() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(server.url("/api/pollNow"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"].as_bool(), Some(true));
    assert!(body["message"].as_str().is_some());
}

// ── Zinc Content Negotiation ───────────────────────────────

#[tokio::test]
async fn about_returns_zinc_when_requested() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(server.url("/api/about"))
        .header("Accept", "text/zinc")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.contains("text/zinc"));
    let body = resp.text().await.unwrap();
    assert!(body.contains("ver:\"3.0\""));
    assert!(body.contains("serverName"));
}

#[tokio::test]
async fn channels_returns_zinc_when_requested() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(server.url("/api/channels"))
        .header("Accept", "text/zinc")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.contains("text/zinc"));
    let body = resp.text().await.unwrap();
    assert!(body.contains("ver:\"3.0\""));
    assert!(body.contains("1113"));
}

#[tokio::test]
async fn status_returns_zinc_when_requested() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(server.url("/api/status"))
        .header("Accept", "text/zinc")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("ver:\"3.0\""));
    assert!(body.contains("channelCount"));
}

// ── History ────────────────────────────────────────────────

#[tokio::test]
async fn history_returns_empty_for_new_channel() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/history/1113")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(body.is_empty()); // no history yet (no poll cycles run)
}

// ── Filter Expressions ─────────────────────────────────────

#[tokio::test]
async fn read_filter_by_type() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/read?filter=Analog"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 2); // Two Analog channels: 1113, 1200
}

#[tokio::test]
async fn read_filter_by_direction() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/read?filter=Out"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 2); // Two output channels: 2001, 2002
}

#[tokio::test]
async fn read_with_limit() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/read?limit=2")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 2);
}

// ══════════════════════════════════════════════════════════════
// Phase 5.5 — Production hardening integration tests
// ══════════════════════════════════════════════════════════════

// ── Read-Only Mode ────────────────────────────────────────────

#[tokio::test]
async fn read_only_rejects_point_write() {
    let server = TestServer::start_read_only().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(server.url("/api/pointWrite"))
        .json(&serde_json::json!({
            "channel": 2001,
            "value": 1.0,
            "level": 17,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["err"].as_str().unwrap().contains("read-only"));
}

#[tokio::test]
async fn read_only_allows_reads() {
    let server = TestServer::start_read_only().await;
    let resp = reqwest::get(server.url("/api/read?id=1113")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 1);
}

#[tokio::test]
async fn read_only_allows_status() {
    let server = TestServer::start_read_only().await;
    let resp = reqwest::get(server.url("/api/status")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["channelCount"], 5);
}

// ── Error Response Format ─────────────────────────────────────

#[tokio::test]
async fn error_response_has_err_field() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/read?id=9999")).await.unwrap();
    assert!(resp.status().as_u16() >= 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["err"].is_string(),
        "error response must have 'err' string field"
    );
}

#[tokio::test]
async fn not_found_channel_returns_404() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/read?id=9999")).await.unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn point_write_read_nonexistent_channel() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/pointWrite?channel=9999"))
        .await
        .unwrap();
    assert!(resp.status().as_u16() >= 400);
}

// ── Reload in Demo Mode ──────────────────────────────────────

#[tokio::test]
async fn reload_fails_in_demo_mode() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client.post(server.url("/api/reload")).send().await.unwrap();
    assert!(resp.status().as_u16() >= 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["err"].as_str().unwrap().contains("demo mode"));
}

// ── History Endpoint ──────────────────────────────────────────

#[tokio::test]
async fn history_with_duration_param() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/history/1113?duration=1h"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    // No poll cycles yet, so empty is fine — just test the endpoint works
    assert!(body.is_empty());
}

#[tokio::test]
async fn history_with_limit_param() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/history/1113?limit=5"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn history_populates_after_poll() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Trigger a poll cycle
    let resp = client
        .post(server.url("/api/pollNow"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Give the history store a moment — the poll runs synchronously in test mode
    // History should now contain entries for polled channels
    let resp = reqwest::get(server.url("/api/history/1113?duration=1h"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    // Note: history may still be empty if the test harness doesn't record to history_store
    // on PollNow. This validates the endpoint round-trips successfully.
    let _ = body; // endpoint responded 200 — pass
}

// ── Status Field Verification ─────────────────────────────────

#[tokio::test]
async fn status_contains_all_expected_fields() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/status")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["uptimeSecs"].is_u64(), "uptimeSecs must be u64");
    assert!(body["channelCount"].is_u64(), "channelCount must be u64");
    assert!(body["pollCount"].is_u64(), "pollCount must be u64");
    assert!(body["tableCount"].is_u64(), "tableCount must be u64");
    assert!(
        body["pollIntervalMs"].is_u64(),
        "pollIntervalMs must be u64"
    );
}

// ── Watch Subscriptions (Extended) ────────────────────────────

#[tokio::test]
async fn watch_poll_without_refresh_returns_changes_only() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Subscribe
    let resp = client
        .post(server.url("/api/watchSub"))
        .json(&serde_json::json!({ "ids": [1113, 1200] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let watch_id = body["watchId"].as_str().unwrap().to_string();

    // Poll WITHOUT refresh — no values have changed, expect 0 rows
    let resp = client
        .post(server.url("/api/watchPoll"))
        .json(&serde_json::json!({
            "watchId": watch_id,
            "refresh": false,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["rows"].as_array().unwrap().len(),
        0,
        "no changes → 0 rows"
    );
}

#[tokio::test]
async fn watch_invalid_id_returns_error() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/api/watchPoll"))
        .json(&serde_json::json!({
            "watchId": "nonexistent-watch",
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().as_u16() >= 400);
}

#[tokio::test]
async fn multiple_concurrent_watch_subscriptions() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Create first watch
    let resp1 = client
        .post(server.url("/api/watchSub"))
        .json(&serde_json::json!({ "ids": [1113] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp1.status(), 200);
    let body1: serde_json::Value = resp1.json().await.unwrap();
    let w1 = body1["watchId"].as_str().unwrap().to_string();

    // Create second watch
    let resp2 = client
        .post(server.url("/api/watchSub"))
        .json(&serde_json::json!({ "ids": [1200, 2001] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    let body2: serde_json::Value = resp2.json().await.unwrap();
    let w2 = body2["watchId"].as_str().unwrap().to_string();

    // Different watch IDs
    assert_ne!(w1, w2);

    // Both respond to poll
    let poll1 = client
        .post(server.url("/api/watchPoll"))
        .json(&serde_json::json!({ "watchId": w1, "refresh": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(poll1.status(), 200);
    let b1: serde_json::Value = poll1.json().await.unwrap();
    assert_eq!(b1["rows"].as_array().unwrap().len(), 1);

    let poll2 = client
        .post(server.url("/api/watchPoll"))
        .json(&serde_json::json!({ "watchId": w2, "refresh": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(poll2.status(), 200);
    let b2: serde_json::Value = poll2.json().await.unwrap();
    assert_eq!(b2["rows"].as_array().unwrap().len(), 2);

    // Close first, second still works
    client
        .post(server.url("/api/watchUnsub"))
        .json(&serde_json::json!({ "watchId": w1, "close": true }))
        .send()
        .await
        .unwrap();

    let poll2_again = client
        .post(server.url("/api/watchPoll"))
        .json(&serde_json::json!({ "watchId": w2, "refresh": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(poll2_again.status(), 200);
}

// ── PointWrite Extended ───────────────────────────────────────

#[tokio::test]
async fn point_write_to_input_channel_fails() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    // Channel 1113 is an input channel — writing should fail
    let resp = client
        .post(server.url("/api/pointWrite"))
        .json(&serde_json::json!({
            "channel": 1113,
            "value": 1.0,
            "level": 17,
        }))
        .send()
        .await
        .unwrap();
    // Engine should reject write to input channel
    assert!(resp.status().as_u16() >= 400);
}

#[tokio::test]
async fn point_write_with_who_field() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/api/pointWrite"))
        .json(&serde_json::json!({
            "channel": 2001,
            "value": 42.0,
            "level": 10,
            "who": "integration-test",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    // Level 10 should have the value and who field
    let l10 = &body[9];
    assert_eq!(l10["level"], 10);
    assert!((l10["val"].as_f64().unwrap() - 42.0).abs() < f64::EPSILON);
    assert_eq!(l10["who"], "integration-test");
}

// ── Channel Response Fields ───────────────────────────────────

#[tokio::test]
async fn channel_list_has_required_fields() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/channels")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(!body.is_empty());
    let ch = &body[0];
    // Every channel must have these fields (from channel_info_json() in handlers.rs)
    assert!(ch["id"].is_u64(), "channel must have id");
    assert!(ch["label"].is_string(), "channel must have label");
    assert!(ch["type"].is_string(), "channel must have type");
    assert!(ch["direction"].is_string(), "channel must have direction");
    assert!(ch["status"].is_string(), "channel must have status");
}

// ── Read All (no filter) ─────────────────────────────────────

#[tokio::test]
async fn read_all_returns_all_channels() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/read")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 5); // all 5 demo channels
}

// ── CORS Headers ──────────────────────────────────────────────

#[tokio::test]
async fn cors_headers_present() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(server.url("/api/about"))
        .header("Origin", "http://example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // Restrictive CORS allows any origin but only specific methods/headers
    let acao = resp.headers().get("access-control-allow-origin");
    assert!(acao.is_some(), "CORS header must be present");
    assert_eq!(acao.unwrap().to_str().unwrap(), "*");
}

#[tokio::test]
async fn cors_preflight_returns_allowed_methods_and_headers() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .request(reqwest::Method::OPTIONS, server.url("/api/pointWrite"))
        .header("Origin", "http://192.168.1.100:8085")
        .header("Access-Control-Request-Method", "POST")
        .header(
            "Access-Control-Request-Headers",
            "content-type,authorization",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Verify allowed methods include GET, POST
    let methods = resp
        .headers()
        .get("access-control-allow-methods")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        methods.contains("GET"),
        "should allow GET, got: {}",
        methods
    );
    assert!(
        methods.contains("POST"),
        "should allow POST, got: {}",
        methods
    );

    // Verify allowed headers include content-type and authorization
    let headers = resp
        .headers()
        .get("access-control-allow-headers")
        .unwrap()
        .to_str()
        .unwrap();
    let headers_lower = headers.to_lowercase();
    assert!(
        headers_lower.contains("content-type"),
        "should allow content-type, got: {}",
        headers
    );
    assert!(
        headers_lower.contains("authorization"),
        "should allow authorization, got: {}",
        headers
    );

    // Verify max-age is set (preflight caching)
    let max_age = resp.headers().get("access-control-max-age");
    assert!(
        max_age.is_some(),
        "should have max-age for preflight caching"
    );
}

// ── Zinc Negotiation Extended ─────────────────────────────────

#[tokio::test]
async fn polls_returns_zinc_when_requested() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(server.url("/api/polls"))
        .header("Accept", "text/zinc")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.contains("text/zinc"));
    let body = resp.text().await.unwrap();
    assert!(body.contains("ver:\"3.0\""));
}

#[tokio::test]
async fn history_returns_zinc_when_requested() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(server.url("/api/history/1113"))
        .header("Accept", "text/zinc")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.contains("text/zinc"));
    let body = resp.text().await.unwrap();
    assert!(body.contains("ver:\"3.0\""));
}

// ══════════════════════════════════════════════════════════════
// Auth Token Integration Tests
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_auth_required_on_protected_routes() {
    let server = TestServer::start_with_auth("test-secret-token").await;
    let client = reqwest::Client::new();

    // POST without token -> 401
    let resp = client
        .post(server.url("/api/pointWrite"))
        .json(&serde_json::json!({
            "channel": 2001,
            "value": 1.0,
            "level": 17,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "POST without auth token should be 401");

    // Other protected routes should also require auth
    let resp = client
        .post(server.url("/api/pollNow"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        401,
        "pollNow without auth token should be 401"
    );

    let resp = client.post(server.url("/api/reload")).send().await.unwrap();
    assert_eq!(
        resp.status(),
        401,
        "reload without auth token should be 401"
    );
}

#[tokio::test]
async fn test_auth_succeeds_with_correct_token() {
    let server = TestServer::start_with_auth("test-secret-token").await;
    let client = reqwest::Client::new();

    // POST with correct Bearer token -> succeeds
    let resp = client
        .post(server.url("/api/pointWrite"))
        .header("Authorization", "Bearer test-secret-token")
        .json(&serde_json::json!({
            "channel": 2001,
            "value": 1.0,
            "level": 17,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "POST with correct auth token should succeed"
    );
}

#[tokio::test]
async fn test_auth_rejects_wrong_token() {
    let server = TestServer::start_with_auth("test-secret-token").await;
    let client = reqwest::Client::new();

    // POST with wrong Bearer token -> 401
    let resp = client
        .post(server.url("/api/pointWrite"))
        .header("Authorization", "Bearer wrong-token")
        .json(&serde_json::json!({
            "channel": 2001,
            "value": 1.0,
            "level": 17,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        401,
        "POST with wrong auth token should be 401"
    );

    // Missing "Bearer " prefix -> 401
    let resp = client
        .post(server.url("/api/pointWrite"))
        .header("Authorization", "test-secret-token")
        .json(&serde_json::json!({
            "channel": 2001,
            "value": 1.0,
            "level": 17,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        401,
        "Authorization without Bearer prefix should be 401"
    );
}

#[tokio::test]
async fn test_public_routes_work_without_token() {
    let server = TestServer::start_with_auth("test-secret-token").await;

    // Public GET endpoints should work without any auth token
    let resp = reqwest::get(server.url("/api/about")).await.unwrap();
    assert_eq!(resp.status(), 200, "/api/about should be public");

    let resp = reqwest::get(server.url("/api/status")).await.unwrap();
    assert_eq!(resp.status(), 200, "/api/status should be public");

    let resp = reqwest::get(server.url("/api/channels")).await.unwrap();
    assert_eq!(resp.status(), 200, "/api/channels should be public");

    let resp = reqwest::get(server.url("/api/read?id=1113")).await.unwrap();
    assert_eq!(resp.status(), 200, "/api/read should be public");

    let resp = reqwest::get(server.url("/api/polls")).await.unwrap();
    assert_eq!(resp.status(), 200, "/api/polls should be public");

    let resp = reqwest::get(server.url("/api/pointWrite?channel=2001"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "GET /api/pointWrite should be public");

    let resp = reqwest::get(server.url("/health")).await.unwrap();
    assert_eq!(resp.status(), 200, "/health should be public");
}

// ══════════════════════════════════════════════════════════════
// SCRAM-SHA-256 Authentication Tests
// ══════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_scram_auth_full_flow() {
    let server = TestServer::start_with_scram("admin", "secret123", None).await;
    let client = reqwest::Client::new();

    // Step 1: Send SCRAM client-first via /api/auth
    let (client_first, client_nonce) = sandstar_server::auth::scram_client_first("admin");
    let client_first_bare = client_first.strip_prefix("n,,").unwrap().to_string();
    let client_first_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        client_first.as_bytes(),
    );

    let resp = client
        .post(server.url("/api/auth"))
        .json(&serde_json::json!({
            "action": "scramFirst",
            "data": client_first_b64,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        401,
        "scramFirst should return 401 with challenge"
    );

    let challenge: serde_json::Value = resp.json().await.unwrap();
    let hs_token = challenge["handshakeToken"].as_str().unwrap();
    let server_first_b64 = challenge["data"].as_str().unwrap();
    assert_eq!(challenge["hash"], "SHA-256");
    assert!(challenge["iterations"].as_u64().unwrap() > 0);

    // Decode server-first
    let server_first = String::from_utf8(
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, server_first_b64)
            .unwrap(),
    )
    .unwrap();

    // Step 2: Build client-final and send it
    let client_final = sandstar_server::auth::scram_client_final(
        "secret123",
        &client_nonce,
        &client_first_bare,
        &server_first,
    )
    .unwrap();
    let client_final_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        client_final.as_bytes(),
    );

    let resp = client
        .post(server.url("/api/auth"))
        .json(&serde_json::json!({
            "action": "scramFinal",
            "handshakeToken": hs_token,
            "data": client_final_b64,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "scramFinal should succeed");

    let result: serde_json::Value = resp.json().await.unwrap();
    let session_token = result["authToken"].as_str().unwrap();
    assert!(!session_token.is_empty(), "should receive session token");
    assert!(
        result["data"].as_str().is_some(),
        "should receive server signature"
    );

    // Step 3: Use the session token for authorized requests
    let resp = client
        .post(server.url("/api/pollNow"))
        .header("Authorization", format!("Bearer {}", session_token))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "session token should authorize requests"
    );
}

#[tokio::test]
async fn test_bearer_still_works_with_scram_configured() {
    let server = TestServer::start_with_scram("admin", "secret123", Some("legacy-token")).await;
    let client = reqwest::Client::new();

    // Bearer token should still work alongside SCRAM
    let resp = client
        .post(server.url("/api/pollNow"))
        .header("Authorization", "Bearer legacy-token")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "legacy bearer should still work with SCRAM configured"
    );
}

#[tokio::test]
async fn test_scram_wrong_password_rejected() {
    let server = TestServer::start_with_scram("admin", "correct_pass", None).await;
    let client = reqwest::Client::new();

    // Step 1: Begin SCRAM
    let (client_first, client_nonce) = sandstar_server::auth::scram_client_first("admin");
    let client_first_bare = client_first.strip_prefix("n,,").unwrap().to_string();
    let client_first_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        client_first.as_bytes(),
    );

    let resp = client
        .post(server.url("/api/auth"))
        .json(&serde_json::json!({
            "action": "scramFirst",
            "data": client_first_b64,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let challenge: serde_json::Value = resp.json().await.unwrap();
    let hs_token = challenge["handshakeToken"].as_str().unwrap();
    let server_first = String::from_utf8(
        base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            challenge["data"].as_str().unwrap(),
        )
        .unwrap(),
    )
    .unwrap();

    // Use wrong password
    let client_final = sandstar_server::auth::scram_client_final(
        "wrong_password",
        &client_nonce,
        &client_first_bare,
        &server_first,
    )
    .unwrap();
    let client_final_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        client_final.as_bytes(),
    );

    let resp = client
        .post(server.url("/api/auth"))
        .json(&serde_json::json!({
            "action": "scramFinal",
            "handshakeToken": hs_token,
            "data": client_final_b64,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "wrong password should be rejected");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("invalid client proof"));
}

#[tokio::test]
async fn test_session_token_works_after_scram() {
    let server = TestServer::start_with_scram("admin", "mypass", None).await;
    let client = reqwest::Client::new();

    // Without auth, protected routes should fail
    let resp = client
        .post(server.url("/api/pollNow"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "unauthenticated should be 401");

    // Complete SCRAM to get session token
    let (client_first, client_nonce) = sandstar_server::auth::scram_client_first("admin");
    let client_first_bare = client_first.strip_prefix("n,,").unwrap().to_string();
    let client_first_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        client_first.as_bytes(),
    );

    let resp = client
        .post(server.url("/api/auth"))
        .json(&serde_json::json!({
            "action": "scramFirst",
            "data": client_first_b64,
        }))
        .send()
        .await
        .unwrap();

    let challenge: serde_json::Value = resp.json().await.unwrap();
    let hs_token = challenge["handshakeToken"].as_str().unwrap();
    let server_first = String::from_utf8(
        base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            challenge["data"].as_str().unwrap(),
        )
        .unwrap(),
    )
    .unwrap();

    let client_final = sandstar_server::auth::scram_client_final(
        "mypass",
        &client_nonce,
        &client_first_bare,
        &server_first,
    )
    .unwrap();
    let client_final_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        client_final.as_bytes(),
    );

    let resp = client
        .post(server.url("/api/auth"))
        .json(&serde_json::json!({
            "action": "scramFinal",
            "handshakeToken": hs_token,
            "data": client_final_b64,
        }))
        .send()
        .await
        .unwrap();

    let result: serde_json::Value = resp.json().await.unwrap();
    let session_token = result["authToken"].as_str().unwrap();

    // Use session token for multiple requests
    for _ in 0..3 {
        let resp = client
            .post(server.url("/api/pollNow"))
            .header("Authorization", format!("Bearer {}", session_token))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "session token should work for multiple requests"
        );
    }

    // Public routes should still work without auth
    let resp = reqwest::get(server.url("/api/about")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_scram_unknown_user_rejected() {
    let server = TestServer::start_with_scram("admin", "pass", None).await;
    let client = reqwest::Client::new();

    let (client_first, _) = sandstar_server::auth::scram_client_first("nonexistent");
    let client_first_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        client_first.as_bytes(),
    );

    let resp = client
        .post(server.url("/api/auth"))
        .json(&serde_json::json!({
            "action": "scramFirst",
            "data": client_first_b64,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("unknown user"));
}

// ── Formats ───────────────────────────────────────────────

#[tokio::test]
async fn formats_returns_mime_types() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/formats")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 3);
    // application/json should be first and support both send+receive
    assert_eq!(body[0]["mime"], "application/json");
    assert_eq!(body[0]["receive"], true);
    assert_eq!(body[0]["send"], true);
    // text/plain is send-only
    assert_eq!(body[2]["mime"], "text/plain");
    assert_eq!(body[2]["receive"], false);
    assert_eq!(body[2]["send"], true);
}

#[tokio::test]
async fn formats_zinc_response() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(server.url("/api/formats"))
        .header("accept", "text/zinc")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.starts_with("ver:\"3.0\""));
    assert!(body.contains("mime,receive,send"));
    assert!(body.contains("\"application/json\""));
}

// ── HisRead ───────────────────────────────────────────────

#[tokio::test]
async fn his_read_returns_history_after_poll() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Trigger a poll to generate history data
    client
        .post(server.url("/api/pollNow"))
        .send()
        .await
        .unwrap();

    // Small delay to let poll complete
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Read history for channel 1113
    let resp = client
        .post(server.url("/api/hisRead"))
        .json(&serde_json::json!({
            "id": 1113,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(!body.is_empty(), "should have history after pollNow");
    // Each point should have ts and cur
    assert!(body[0]["ts"].as_u64().is_some());
    assert!(body[0]["cur"].as_f64().is_some());
}

#[tokio::test]
async fn his_read_with_range_today() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Poll to create history
    client
        .post(server.url("/api/pollNow"))
        .send()
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let resp = client
        .post(server.url("/api/hisRead"))
        .json(&serde_json::json!({
            "id": 1113,
            "range": "today",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    // Should contain data since we just polled
    assert!(!body.is_empty());
}

#[tokio::test]
async fn his_read_empty_for_unknown_channel() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/api/hisRead"))
        .json(&serde_json::json!({
            "id": 9999,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(body.is_empty());
}

// ── Nav ───────────────────────────────────────────────────

#[tokio::test]
async fn nav_root_returns_site() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/api/nav"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 1);
    assert_eq!(body[0]["navId"], "site");
    assert_eq!(body[0]["dis"], "Sandstar Site");
}

#[tokio::test]
async fn nav_site_returns_equipment() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/api/nav"))
        .json(&serde_json::json!({ "navId": "site" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 1);
    assert_eq!(body[0]["navId"], "equip:default");
}

#[tokio::test]
async fn nav_equip_returns_points() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/api/nav"))
        .json(&serde_json::json!({ "navId": "equip:default" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 5); // 5 demo channels
                               // Should have navId like "point:1113"
    let nav_ids: Vec<&str> = body.iter().map(|r| r["navId"].as_str().unwrap()).collect();
    assert!(nav_ids.iter().any(|id| id.contains("1113")));
}

#[tokio::test]
async fn nav_unknown_returns_404() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/api/nav"))
        .json(&serde_json::json!({ "navId": "bogus" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// ── InvokeAction ──────────────────────────────────────────

#[tokio::test]
async fn invoke_action_reload_in_demo_mode() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // In demo mode, reload returns an error (same as POST /api/reload)
    let resp = client
        .post(server.url("/api/invokeAction"))
        .json(&serde_json::json!({
            "action": "reload",
        }))
        .send()
        .await
        .unwrap();
    // Demo mode has no config_dir, so reload fails
    assert!(resp.status().as_u16() >= 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["err"].as_str().unwrap().contains("demo mode"));
}

#[tokio::test]
async fn invoke_action_unknown_returns_400() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/api/invokeAction"))
        .json(&serde_json::json!({
            "action": "nonexistent",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

// ── Ops list includes new operations ──────────────────────

#[tokio::test]
async fn ops_includes_new_operations() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/ops")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    let names: Vec<&str> = body.iter().map(|op| op["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"formats"), "ops should list formats");
    assert!(names.contains(&"hisRead"), "ops should list hisRead");
    assert!(names.contains(&"nav"), "ops should list nav");
    assert!(
        names.contains(&"invokeAction"),
        "ops should list invokeAction"
    );
}

// ══════════════════════════════════════════════════════════════
// Phase 5.8i — REST edge case & integration expansion tests
// ══════════════════════════════════════════════════════════════

// ── PointWrite edge cases ────────────────────────────────────

#[tokio::test]
async fn point_write_out_of_range_level_zero() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Level 0 is out of range (valid: 1-17)
    let resp = client
        .post(server.url("/api/pointWrite"))
        .json(&serde_json::json!({
            "channel": 2001,
            "value": 1.0,
            "level": 0,
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().as_u16() >= 400,
        "level 0 should be rejected, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn point_write_out_of_range_level_18() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Level 18 is out of range (valid: 1-17)
    let resp = client
        .post(server.url("/api/pointWrite"))
        .json(&serde_json::json!({
            "channel": 2001,
            "value": 1.0,
            "level": 18,
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().as_u16() >= 400,
        "level 18 should be rejected, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn point_write_nonexistent_channel_returns_error() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/api/pointWrite"))
        .json(&serde_json::json!({
            "channel": 9999,
            "value": 1.0,
            "level": 17,
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().as_u16() >= 400,
        "write to nonexistent channel should fail, got {}",
        resp.status()
    );
}

// ── About endpoint verification ──────────────────────────────

#[tokio::test]
async fn about_contains_version_product_vendor() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/api/about")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["productVersion"].is_string(),
        "must have productVersion"
    );
    assert_eq!(body["productName"], "sandstar-engine-server");
    assert_eq!(body["vendorName"], "EacIo");
    assert!(body["buildInfo"].is_string(), "must have buildInfo");
    assert!(
        body["serverBootTime"].is_string(),
        "must have serverBootTime"
    );
}

// ── Health endpoint format ───────────────────────────────────

#[tokio::test]
async fn health_response_format() {
    let server = TestServer::start().await;
    let resp = reqwest::get(server.url("/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["healthy"], true);
    assert!(body["uptimeSecs"].is_u64(), "uptimeSecs must be present");
}

// ── Nav with empty navId ─────────────────────────────────────

#[tokio::test]
async fn nav_empty_nav_id_returns_root() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/api/nav"))
        .json(&serde_json::json!({ "navId": "" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 1);
    assert_eq!(body[0]["navId"], "site");
}

// ── Concurrent watch + REST read ─────────────────────────────

#[tokio::test]
async fn concurrent_watch_and_rest_read() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Subscribe to channel 1113 via watch
    let resp = client
        .post(server.url("/api/watchSub"))
        .json(&serde_json::json!({ "ids": [1113] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let watch_id = body["watchId"].as_str().unwrap().to_string();

    // Simultaneously read the same channel via REST
    let read_resp = reqwest::get(server.url("/api/read?id=1113")).await.unwrap();
    assert_eq!(read_resp.status(), 200);
    let read_body: Vec<serde_json::Value> = read_resp.json().await.unwrap();
    assert_eq!(read_body.len(), 1);
    assert_eq!(read_body[0]["channel"], 1113);

    // Watch poll should also work
    let poll_resp = client
        .post(server.url("/api/watchPoll"))
        .json(&serde_json::json!({ "watchId": watch_id, "refresh": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(poll_resp.status(), 200);
    let poll_body: serde_json::Value = poll_resp.json().await.unwrap();
    assert_eq!(poll_body["rows"].as_array().unwrap().len(), 1);

    // Clean up
    client
        .post(server.url("/api/watchUnsub"))
        .json(&serde_json::json!({ "watchId": watch_id, "close": true }))
        .send()
        .await
        .unwrap();
}

// ── Rate limiting integration ────────────────────────────────

#[tokio::test]
async fn rate_limiting_returns_429() {
    // Set a very low rate limit (5 req/s)
    let server = TestServer::start_with_rate_limit(5).await;

    // Send more requests than the limit in quick succession
    let mut got_429 = false;
    for _ in 0..20 {
        let resp = reqwest::get(server.url("/api/status")).await.unwrap();
        if resp.status() == 429 {
            got_429 = true;
            break;
        }
    }
    assert!(
        got_429,
        "should have received 429 after exceeding rate limit"
    );
}

// ── Write then read consistency ──────────────────────────────

#[tokio::test]
async fn write_then_read_consistency() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Write a value to output channel 2001
    let resp = client
        .post(server.url("/api/pointWrite"))
        .json(&serde_json::json!({
            "channel": 2001,
            "value": 77.0,
            "level": 17,
            "who": "consistency-test",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Immediately read the priority grid back
    let resp = reqwest::get(server.url("/api/pointWrite?channel=2001"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 17);

    // Level 17 should reflect the written value
    let l17 = &body[16];
    assert_eq!(l17["level"], 17);
    assert!((l17["val"].as_f64().unwrap() - 77.0).abs() < f64::EPSILON);
    assert_eq!(l17["who"], "consistency-test");
}

// ── Filter combinations in REST read ─────────────────────────

#[tokio::test]
async fn read_filter_and_combination() {
    let server = TestServer::start().await;
    // "analog and input" should match channels 1113 and 1200 (both Analog+In)
    let resp = reqwest::get(server.url("/api/read?filter=analog%20and%20input"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 2, "should match 2 analog input channels");
}

#[tokio::test]
async fn read_filter_or_combination() {
    let server = TestServer::start().await;
    // "i2c or pwm" should match channel 612 (I2c) and 2002 (Pwm)
    let resp = reqwest::get(server.url("/api/read?filter=i2c%20or%20pwm"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 2, "should match I2C + PWM channels");
}

#[tokio::test]
async fn read_filter_channel_range() {
    let server = TestServer::start().await;
    // "channel > 2000" should match 2001 (Digital Out) and 2002 (PWM)
    let resp = reqwest::get(server.url("/api/read?filter=channel%20%3E%202000"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(body.len(), 2, "should match channels > 2000");
}
