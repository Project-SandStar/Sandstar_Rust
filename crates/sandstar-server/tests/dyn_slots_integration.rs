//! Phase 13 validation: end-to-end RoWS `readTags` / `setTags` / `deleteTag`
//! round-trip against a real Axum server.
//!
//! Phase 13.0f and 13.0g were marked "Not started" in the roadmap, but the
//! code + dispatcher are live (`rest/rows.rs:526–548`, 35 unit tests in that
//! file, filter integration in `rest/filter.rs`). What was missing was
//! proof that a client connecting to `/api/rows` on a running server can
//! actually do a setTags → readTags → deleteTag → readTags sequence and see
//! the expected state in between.
//!
//! This file mirrors the Phase 9 integration-test playbook: spin up an
//! ephemeral Axum server with the `/api/rows` route + a real
//! `RowsState { tree, manifest_db, dyn_store }`, connect tokio-tungstenite
//! as the client, exercise the protocol.

use std::sync::Arc;
use std::time::Duration;

use sandstar_server::sox::{
    dyn_slots::{DynSlotStore, DynValue},
    sox_handlers::{ComponentTree, ManifestDb},
};
use sandstar_server::rest::rows::{rows_ws_handler, RowsState};
use serde_json::{json, Value};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

// ── Helpers ────────────────────────────────────────────────

async fn bind_ephemeral() -> (tokio::net::TcpListener, u16) {
    let lst = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let port = lst.local_addr().expect("local_addr").port();
    (lst, port)
}

/// Build a RowsState backed by an empty ComponentTree + a DynSlotStore
/// with default limits.
fn fresh_rows_state() -> RowsState {
    RowsState {
        tree: Arc::new(std::sync::RwLock::new(ComponentTree::new())),
        manifest_db: Arc::new(ManifestDb::new()),
        dyn_store: Some(Arc::new(std::sync::RwLock::new(
            DynSlotStore::with_defaults(),
        ))),
    }
}

async fn start_rows_server(listener: tokio::net::TcpListener, state: RowsState) {
    use axum::routing::get;
    let app = axum::Router::new()
        .route("/api/rows", get(rows_ws_handler))
        .with_state(state);
    let _ = axum::serve(listener, app).await;
}

/// Send a JSON command and wait up to 2 s for the reply.
async fn request(
    ws_tx: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    ws_rx: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    payload: Value,
) -> Value {
    ws_tx
        .send(Message::Text(payload.to_string().into()))
        .await
        .expect("send ws");
    let msg = tokio::time::timeout(Duration::from_secs(2), ws_rx.next())
        .await
        .expect("ws recv timeout")
        .expect("ws stream ended")
        .expect("ws transport error");
    match msg {
        Message::Text(t) => serde_json::from_str::<Value>(&t).expect("invalid JSON reply"),
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── Tests ──────────────────────────────────────────────────

/// The core round-trip: setTags → readTags → deleteTag → readTags.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_set_read_delete_tags_round_trip() {
    let state = fresh_rows_state();
    let (lst, port) = bind_ephemeral().await;
    tokio::spawn(start_rows_server(lst, state));

    // Give the server a beat to start listening.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/api/rows"))
        .await
        .expect("ws connect");
    let (mut ws_tx, mut ws_rx) = ws.split();

    // 1. setTags — write three keys to comp_id 50.
    let r1 = request(
        &mut ws_tx,
        &mut ws_rx,
        json!({
            "op": "setTags",
            "compId": 50,
            "tags": {
                "modbusAddr": 40001,
                "dis": "Zone Temp",
                "group": "HVAC"
            },
            "id": "s1"
        }),
    )
    .await;
    // Reply shape: {"op": "result", "ok": true, "data": {...}, "id": "..."}
    assert_eq!(r1["id"], "s1", "setTags reply id");
    assert_eq!(r1["op"], "result");
    assert_eq!(r1["ok"], true);
    assert_eq!(r1["data"]["ok"], true);

    // 2. readTags — expect all three keys with the values we just wrote.
    // DynValue is serde-tagged as {"type": "Int"|"Str"|..., "val": <value>}.
    let r2 = request(
        &mut ws_tx,
        &mut ws_rx,
        json!({ "op": "readTags", "compId": 50, "id": "r1" }),
    )
    .await;
    assert_eq!(r2["id"], "r1");
    let tags = &r2["data"]["tags"];
    assert_eq!(tags["modbusAddr"]["type"], "Int");
    assert_eq!(tags["modbusAddr"]["val"], 40001);
    assert_eq!(tags["dis"]["type"], "Str");
    assert_eq!(tags["dis"]["val"], "Zone Temp");
    assert_eq!(tags["group"]["val"], "HVAC");

    // 3. deleteTag — remove "group".
    let r3 = request(
        &mut ws_tx,
        &mut ws_rx,
        json!({ "op": "deleteTag", "compId": 50, "key": "group", "id": "d1" }),
    )
    .await;
    assert_eq!(r3["id"], "d1");
    assert_eq!(r3["data"]["ok"], true);
    assert_eq!(r3["data"]["removed"], true);

    // 4. readTags again — "group" must be gone, others remain.
    let r4 = request(
        &mut ws_tx,
        &mut ws_rx,
        json!({ "op": "readTags", "compId": 50, "id": "r2" }),
    )
    .await;
    let tags2 = &r4["data"]["tags"];
    assert!(
        tags2.get("group").is_none(),
        "'group' should be deleted, got tags: {tags2}"
    );
    assert_eq!(tags2["modbusAddr"]["val"], 40001);
    assert_eq!(tags2["dis"]["val"], "Zone Temp");
}

/// readTags for an unknown compId returns an empty tags map (not an error).
/// This matches the semantic "no tags yet" rather than "component missing" —
/// DynSlotStore is a side-car that doesn't know about component existence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_read_tags_unknown_comp_is_empty() {
    let state = fresh_rows_state();
    let (lst, port) = bind_ephemeral().await;
    tokio::spawn(start_rows_server(lst, state));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/api/rows"))
        .await
        .expect("ws connect");
    let (mut ws_tx, mut ws_rx) = ws.split();

    let r = request(
        &mut ws_tx,
        &mut ws_rx,
        json!({ "op": "readTags", "compId": 9999, "id": "r" }),
    )
    .await;
    assert_eq!(r["op"], "result");
    let tags = &r["data"]["tags"];
    assert!(tags.is_object(), "expected object, got {r}");
    assert_eq!(
        tags.as_object().unwrap().len(),
        0,
        "expected empty tags map for unknown comp_id, got {tags}"
    );
}

/// setTags with missing required fields returns an error message rather
/// than corrupting the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_set_tags_missing_fields_returns_error() {
    let state = fresh_rows_state();
    let (lst, port) = bind_ephemeral().await;
    tokio::spawn(start_rows_server(lst, state));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/api/rows"))
        .await
        .expect("ws connect");
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Missing "tags" field.
    let r = request(
        &mut ws_tx,
        &mut ws_rx,
        json!({ "op": "setTags", "compId": 50, "id": "e1" }),
    )
    .await;
    assert_eq!(r["id"], "e1");
    assert_eq!(r["op"], "error");
    assert_eq!(r["code"], "BAD_REQUEST");
    assert!(
        r["message"].as_str().unwrap_or("").contains("tags"),
        "error should mention 'tags', got {r}"
    );
}

// ── 13.0g: filter-over-dyn-tags unit cross-check ──────────────────

/// Belt-and-suspenders unit check that the filter evaluator actually
/// consults a DynSlotStore's tag map for a component. The filter
/// integration is already covered by 77 tests in `rest/filter.rs`, but
/// this test explicitly composes the real DynSlotStore + filter path
/// to prove the glue hasn't regressed.
#[test]
fn filter_matches_dynamic_tag_via_store() {
    use sandstar_ipc::types::ChannelInfo;
    use sandstar_server::rest::filter::{matches_with_tags, parse};

    let mut store = DynSlotStore::with_defaults();
    store
        .set(42, "modbusAddr".into(), DynValue::Int(40001))
        .expect("set");
    store
        .set(42, "zone".into(), DynValue::Str("lobby".into()))
        .expect("set");

    // ChannelInfo matches the sandstar-ipc shape.
    let ch = ChannelInfo {
        id: 100,
        label: "Channel 100".into(),
        channel_type: "analog".into(),
        direction: "AI".into(),
        enabled: true,
        status: "Ok".into(),
        cur: 0.0,
        raw: 0.0,
    };

    let tags = store.get_all(42).cloned();

    // Matches on dynamic int tag.
    let expr = parse("modbusAddr==40001").expect("parse");
    assert!(matches_with_tags(&expr, &ch, tags.as_ref()));

    // Matches on dynamic string tag.
    let expr = parse(r#"zone=="lobby""#).expect("parse");
    assert!(matches_with_tags(&expr, &ch, tags.as_ref()));

    // Non-matching value correctly fails.
    let expr = parse("modbusAddr==99999").expect("parse");
    assert!(!matches_with_tags(&expr, &ch, tags.as_ref()));

    // Filter on static field still works when tags are present.
    let expr = parse("enabled").expect("parse");
    assert!(matches_with_tags(&expr, &ch, tags.as_ref()));
}
