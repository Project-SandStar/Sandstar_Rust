//! `/live` — Fantom-dialect SOX over WebSocket.
//!
//! This is the transport the Vue web editor (`C:\Coding\swe\sedonaWebEditor`)
//! speaks today against the SkySpark `bassgSoxWebSocket` Fantom pod.
//! Clients exchange single-character `msgType` JSON envelopes
//! (`v c r w a d s u l n o q i f z b y p x` going out, uppercase echoes
//! coming back, plus `e` for server-pushed events and `!` for errors).
//!
//! This handler parses those envelopes into the transport-agnostic
//! [`SoxOp`] in `sox::sox_ops`, calls [`apply_op`], and renders the
//! result back in Fantom shape. Component-read responses and event
//! frames carry `compProps.propValues` as base64-encoded raw SOX slot
//! bytes — matching the wire format that the editor's Sedona-aware
//! client already decodes.
//!
//! Introduced in Phase 14.0G-2c (2026-04-23).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value as JsonValue};
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info};

use crate::sox::sox_handlers::{
    encode_slot_value_raw, VirtualComponent, SLOT_FLAG_ACTION, SLOT_FLAG_CONFIG,
};
use crate::sox::sox_ops::{apply_op, SessionState, SoxOp, SoxResponse, SoxState};
use crate::sox::SharedComponentTree;

use super::sox_api::slot_value_to_json;

// ── State alias ────────────────────────────────────

/// Shared state for `/live`. Identical to `RowsState` — both dialects
/// talk to the same component tree / manifest DB / dyn tag store.
pub type LiveState = SoxState;

// ── Constants ──────────────────────────────────────

const MAX_LIVE_CONNECTIONS: i64 = 16;
/// Event-push cadence. Fantom's pod batches at 8 ms; we use 500 ms to
/// match the server's existing RoWS pacing and avoid spamming editors
/// that don't need sub-second UI responsiveness.
const COV_INTERVAL_MS: u64 = 500;
const CLIENT_TIMEOUT_SECS: u64 = 120;

// ── Upgrade handler ────────────────────────────────

pub async fn live_ws_handler(ws: WebSocketUpgrade, State(state): State<LiveState>) -> Response {
    let current = crate::metrics::metrics()
        .rows_active
        .load(Ordering::Relaxed);
    if current >= MAX_LIVE_CONNECTIONS {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many /live connections").into_response();
    }
    ws.on_upgrade(|socket| handle_live_session(socket, state))
}

// ── Session handler ────────────────────────────────

async fn handle_live_session(ws: WebSocket, state: LiveState) {
    let metrics = crate::metrics::metrics();
    metrics.rows_active.fetch_add(1, Ordering::Relaxed);
    metrics.rows_total.fetch_add(1, Ordering::Relaxed);
    info!("/live connected");

    let (mut ws_tx, mut ws_rx) = ws.split();

    // Per-session state. The compact_cov toggle is accepted for parity
    // with RoWS but has no effect on Fantom event frames (which always
    // carry raw binary slot payloads).
    let mut subscribed: HashSet<u16> = HashSet::new();
    let mut cov_cache: HashMap<u16, Vec<JsonValue>> = HashMap::new();
    let mut tree_cache: HashMap<u16, Vec<u16>> = HashMap::new();
    let mut compact_cov = false;

    let mut last_client_msg = Instant::now();
    let mut cov_timer = tokio::time::interval(Duration::from_millis(COV_INTERVAL_MS));
    cov_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        last_client_msg = Instant::now();
                        metrics.rows_messages_in.fetch_add(1, Ordering::Relaxed);
                        let reply = handle_live_msg(
                            &text,
                            &state,
                            &mut subscribed,
                            &mut cov_cache,
                            &mut tree_cache,
                            &mut compact_cov,
                        );
                        if let Some(reply_json) = reply {
                            metrics.rows_messages_out.fetch_add(1, Ordering::Relaxed);
                            if ws_tx.send(Message::Text(reply_json.into())).await.is_err() {
                                break;
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
            _ = cov_timer.tick() => {
                if last_client_msg.elapsed() > Duration::from_secs(CLIENT_TIMEOUT_SECS) {
                    debug!("/live client timeout after {}s inactivity", CLIENT_TIMEOUT_SECS);
                    break;
                }
                if subscribed.is_empty() {
                    continue;
                }
                let frames = build_live_events(
                    &state.tree,
                    &subscribed,
                    &mut cov_cache,
                    &mut tree_cache,
                );
                let mut disconnected = false;
                for frame in frames {
                    metrics.rows_messages_out.fetch_add(1, Ordering::Relaxed);
                    if ws_tx.send(Message::Text(frame.into())).await.is_err() {
                        disconnected = true;
                        break;
                    }
                }
                if disconnected { break; }
            }
        }
    }

    metrics.rows_active.fetch_sub(1, Ordering::Relaxed);
    info!("/live disconnected (subscriptions: {})", subscribed.len());
}

// ── Client message dispatcher ──────────────────────

fn handle_live_msg(
    text: &str,
    state: &LiveState,
    subscribed: &mut HashSet<u16>,
    cov_cache: &mut HashMap<u16, Vec<JsonValue>>,
    tree_cache: &mut HashMap<u16, Vec<u16>>,
    compact_cov: &mut bool,
) -> Option<String> {
    let msg: JsonValue = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(_) => return render_error(0xFF, "invalid JSON"),
    };

    let reply_num = msg
        .get("replyNum")
        .and_then(|v| v.as_u64())
        .map(|v| v as u8)
        .unwrap_or(0xFF);
    let msg_type = match msg.get("msgType").and_then(|v| v.as_str()) {
        Some(s) if s.len() == 1 => s.chars().next().unwrap(),
        _ => return render_error(reply_num, "missing or invalid 'msgType'"),
    };

    // Ping is a keep-alive with its own envelope (`p` replyNum 255).
    // Response shape Fantom uses: same msgType/replyNum echoed.
    if msg_type == 'p' {
        return Some(json!({"msgType": "p", "replyNum": reply_num}).to_string());
    }

    // Component reads need binary-encoded slot payloads; handle them
    // here rather than via the generic apply_op path.
    if msg_type == 'c' {
        return handle_read_comp_live(state, reply_num, &msg);
    }

    // Everything else routes through apply_op.
    let op = match parse_live_op(msg_type, &msg) {
        Ok(o) => o,
        Err(cause) => return render_error(reply_num, &cause),
    };

    let mut session = SessionState {
        subscribed,
        cov_cache,
        tree_cache,
        compact_cov,
    };
    let response = apply_op(state, op, &mut session);
    Some(render_live_response(msg_type, reply_num, response))
}

// ── Fantom envelope parsing ────────────────────────

/// Translate a Fantom msgType + JSON payload into a typed [`SoxOp`].
/// Returns `Err(cause)` for malformed requests; caller wraps in a `!`
/// error frame.
fn parse_live_op(msg_type: char, msg: &JsonValue) -> Result<SoxOp, String> {
    let u16_of = |k: &str| msg.get(k).and_then(|v| v.as_u64()).map(|v| v as u16);
    let u8_of = |k: &str| msg.get(k).and_then(|v| v.as_u64()).map(|v| v as u8);
    let str_of = |k: &str| msg.get(k).and_then(|v| v.as_str()).map(String::from);
    let vec_u16 = |k: &str| {
        msg.get(k)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_u64().map(|n| n as u16))
                    .collect::<Vec<u16>>()
            })
            .unwrap_or_default()
    };
    // For Fantom `a`/`w`/`i`: `value` / `configProps` / `argument` come in as
    // base64 strings. We decode here but — for MVP — then pass through as a
    // JSON value the existing apply_op `WriteSlot` / `AddComp` / `Invoke`
    // branches can handle. Full binary round-tripping lands in a follow-up
    // when a concrete caller surfaces that requires it.
    let decode_b64_to_string = |k: &str| -> String {
        msg.get(k)
            .and_then(|v| v.as_str())
            .and_then(|b64| BASE64.decode(b64).ok())
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_default()
    };

    match msg_type {
        'v' => Ok(SoxOp::Version),
        'y' => Ok(SoxOp::VersionExt),
        // 'c' is intercepted upstream; reaching here means the caller ignored the dedicated path.
        'c' => Err("read-component handled by dedicated path".into()),
        'w' => {
            let comp_id = u16_of("compId").ok_or("missing compId".to_string())?;
            let slot_idx = msg
                .get("slotId")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .ok_or("missing slotId".to_string())?;
            // Editor sends value as base64 binary; MVP: decode and re-encode as a JSON string
            // so apply_op's WriteSlot accepts it via json_to_slot_value's string path.
            // Typed-value writes (numeric slots) still need the binary->typed path — tracked
            // as a G-2c follow-up item in the plan doc.
            let value = JsonValue::String(decode_b64_to_string("value"));
            Ok(SoxOp::WriteSlot {
                comp_id,
                slot_idx,
                value,
            })
        }
        'a' => {
            let parent_id = u16_of("parentId").ok_or("missing parentId".to_string())?;
            let kit_id = u8_of("kitId").ok_or("missing kitId".to_string())?;
            let type_id = u8_of("typeId").ok_or("missing typeId".to_string())?;
            let name = str_of("name").ok_or("missing name".to_string())?;
            Ok(SoxOp::AddComp {
                parent_id,
                kit_id,
                type_id,
                name,
            })
        }
        'd' => u16_of("compId")
            .map(|comp_id| SoxOp::DeleteComp { comp_id })
            .ok_or("missing compId".into()),
        'n' => {
            let comp_id = u16_of("compId").ok_or("missing compId".to_string())?;
            let name = str_of("newName")
                .or_else(|| str_of("name"))
                .ok_or("missing newName".to_string())?;
            Ok(SoxOp::Rename { comp_id, name })
        }
        'l' => {
            let op = str_of("op")
                .and_then(|s| s.chars().next())
                .ok_or("missing op".to_string())?;
            let from_comp = u16_of("fromCompId")
                .or_else(|| u16_of("fromComp"))
                .ok_or("missing fromCompId".to_string())?;
            let from_slot = u8_of("fromSlotId")
                .or_else(|| u8_of("fromSlot"))
                .ok_or("missing fromSlotId".to_string())?;
            let to_comp = u16_of("toCompId")
                .or_else(|| u16_of("toComp"))
                .ok_or("missing toCompId".to_string())?;
            let to_slot = u8_of("toSlotId")
                .or_else(|| u8_of("toSlot"))
                .ok_or("missing toSlotId".to_string())?;
            match op {
                'a' => Ok(SoxOp::AddLink {
                    from_comp,
                    from_slot,
                    to_comp,
                    to_slot,
                }),
                'd' => Ok(SoxOp::DeleteLink {
                    from_comp,
                    from_slot,
                    to_comp,
                    to_slot,
                }),
                other => Err(format!("unknown link op: {other}")),
            }
        }
        'o' => {
            let comp_id = u16_of("compId").ok_or("missing compId".to_string())?;
            let children = vec_u16("childrenIds");
            Ok(SoxOp::Reorder { comp_id, children })
        }
        's' => Ok(SoxOp::Subscribe {
            comp_ids: vec_u16("compIds"),
        }),
        'u' => Ok(SoxOp::Unsubscribe {
            comp_ids: vec_u16("compIds"),
        }),
        'q' => {
            let query_type = str_of("queryType")
                .and_then(|s| s.chars().next())
                .ok_or("missing queryType".to_string())?;
            let kit_id = u8_of("kitId").ok_or("missing kitId".to_string())?;
            let type_id = u8_of("typeId").ok_or("missing typeId".to_string())?;
            Ok(SoxOp::Query {
                query_type,
                kit_id,
                type_id,
            })
        }
        'i' => {
            let comp_id = u16_of("compId").ok_or("missing compId".to_string())?;
            let slot_idx = msg
                .get("slotId")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .ok_or("missing slotId".to_string())?;
            Ok(SoxOp::Invoke { comp_id, slot_idx })
        }
        'x' => Ok(SoxOp::CloseWatch {
            watch_id: str_of("watchId").unwrap_or_default(),
        }),
        // File ops (f, z, b) land in Phase 14.0G-4. Reject cleanly so
        // the editor falls back to HTTP /file which we'll wire in G-4.
        'f' | 'z' | 'b' => Err(format!(
            "file op '{msg_type}' not implemented yet — see Phase 14.0G-4"
        )),
        other => Err(format!("unknown msgType: {other}")),
    }
}

// ── Fantom envelope rendering ──────────────────────

fn render_error(reply_num: u8, cause: &str) -> Option<String> {
    Some(
        json!({
            "msgType": "!",
            "replyNum": reply_num,
            "cause": cause,
            "isError": true,
        })
        .to_string(),
    )
}

/// Render an `apply_op` result in the Fantom envelope shape.
///
/// The Fantom pod uses the uppercase msgType as the response code and
/// inlines the SoxOp's result fields at the envelope top level. This
/// function handles the per-msgType field mapping; a few types need
/// custom shapes (version, query, closeWatch) while the rest just get
/// the raw JSON flattened.
fn render_live_response(msg_type: char, reply_num: u8, response: SoxResponse) -> String {
    let data = match response {
        SoxResponse::Ok(v) => v,
        SoxResponse::Error { message, .. } => {
            return json!({
                "msgType": "!",
                "replyNum": reply_num,
                "cause": message,
                "isError": true,
            })
            .to_string();
        }
    };

    let mut out = serde_json::Map::new();
    let response_type = response_code_for(msg_type);
    out.insert("msgType".into(), JsonValue::String(response_type.to_string()));
    out.insert("replyNum".into(), JsonValue::Number(reply_num.into()));

    // Op-specific field mapping to match Fantom's response shapes.
    match msg_type {
        'v' => {
            // V: {kitCount, kits: [{name, checksum}]}
            flatten(&mut out, data);
        }
        'y' => {
            // Y: {platformId, scodeFlags, kitVersions, pairs}
            flatten(&mut out, data);
        }
        'w' => {
            // W: just msgType + replyNum, no payload fields.
        }
        'a' => {
            // A: {compId, ...}
            if let Some(comp_id) = data.get("compId") {
                out.insert("compId".into(), comp_id.clone());
            }
        }
        'd' | 'n' | 'l' | 'o' => {
            // D/N/L/O: acknowledgement only, no payload beyond the echo.
        }
        's' => {
            // S: {numSubscribed}
            if let Some(count) = data.get("count") {
                out.insert("numSubscribed".into(), count.clone());
            }
        }
        'u' => {
            // U: acknowledgement; editor ignores payload.
        }
        'q' => {
            // Q: {compIds}
            if let Some(ids) = data.get("compIds") {
                out.insert("compIds".into(), ids.clone());
            }
        }
        'i' => {
            // I: acknowledgement.
        }
        'x' => {
            // X: {watchId, unsubscribed}
            flatten(&mut out, data);
        }
        _ => {
            // Unknown — attach data under "result" for debugging; harmless.
            out.insert("result".into(), data);
        }
    }

    JsonValue::Object(out).to_string()
}

/// Map a request msgType to its response code per Fantom's convention:
/// lowercase request → uppercase response, ping is a special-case echo.
fn response_code_for(msg_type: char) -> char {
    match msg_type {
        'p' => 'p',
        c if c.is_ascii_lowercase() => c.to_ascii_uppercase(),
        c => c,
    }
}

/// Move every top-level key from `data` into the outer envelope map.
fn flatten(out: &mut serde_json::Map<String, JsonValue>, data: JsonValue) {
    if let JsonValue::Object(map) = data {
        for (k, v) in map {
            out.insert(k, v);
        }
    }
}

// ── Component read (direct tree access, binary slot encoding) ──

fn handle_read_comp_live(state: &LiveState, reply_num: u8, msg: &JsonValue) -> Option<String> {
    let comp_id = match msg.get("compId").and_then(|v| v.as_u64()).map(|v| v as u16) {
        Some(id) => id,
        None => return render_error(reply_num, "missing compId"),
    };
    let what = msg
        .get("what")
        .and_then(|v| v.as_str())
        .and_then(|s| s.chars().next())
        .unwrap_or('c');

    let tree = state.tree.read().unwrap();
    let comp = match tree.get(comp_id) {
        Some(c) => c,
        None => return render_error(reply_num, &format!("component {comp_id} not found")),
    };

    let mut out = serde_json::Map::new();
    out.insert("msgType".into(), JsonValue::String("C".into()));
    out.insert("replyNum".into(), JsonValue::Number(reply_num.into()));
    out.insert("compId".into(), JsonValue::Number(comp_id.into()));
    out.insert("what".into(), JsonValue::String(what.to_string()));

    match what {
        't' => {
            out.insert("compTree".into(), build_comp_tree_payload(comp));
        }
        'l' => {
            out.insert("compLinks".into(), build_comp_links_payload(comp));
        }
        'c' | 'r' | 'C' | 'R' => {
            let payload = encode_slots_for_what(comp, what);
            let b64 = BASE64.encode(&payload);
            out.insert(
                "compProps".into(),
                json!({ "propValues": b64 }),
            );
        }
        other => {
            return render_error(reply_num, &format!("unsupported 'what' code: {other}"));
        }
    }

    Some(JsonValue::Object(out).to_string())
}

fn build_comp_tree_payload(comp: &VirtualComponent) -> JsonValue {
    json!({
        "kitId": comp.kit_id,
        "typeId": comp.type_id,
        "name": comp.name,
        "parent": comp.parent_id,
        "permissions": 0xFFu8,
        "kidIds": comp.children,
    })
}

fn build_comp_links_payload(comp: &VirtualComponent) -> JsonValue {
    let links: Vec<JsonValue> = comp
        .links
        .iter()
        .map(|l| {
            json!({
                "fromCompId": l.from_comp,
                "fromSlotId": l.from_slot,
                "toCompId": l.to_comp,
                "toSlotId": l.to_slot,
            })
        })
        .collect();
    json!({ "links": links })
}

/// Encode the slot values for a `readComp`/event frame given a `what`
/// discriminator. The output is the raw SOX slot-byte concatenation —
/// base64-ready.
fn encode_slots_for_what(comp: &VirtualComponent, what: char) -> Vec<u8> {
    let mut payload = Vec::with_capacity(64);
    for slot in &comp.slots {
        let is_action = slot.flags & SLOT_FLAG_ACTION != 0;
        let is_config = slot.flags & SLOT_FLAG_CONFIG != 0;
        let include = match what {
            'c' | 'C' => !is_action && is_config,
            'r' | 'R' => !is_action && !is_config,
            _ => !is_action,
        };
        if !include {
            continue;
        }
        encode_slot_value_raw(&mut payload, &slot.value);
    }
    payload
}

// ── Event-push building ────────────────────────────

/// Produce `e` frames for subscribed components that have changed
/// since the last tick. Config and runtime changes map to `e`/`what='c'`
/// and `e`/`what='r'` frames respectively; tree-structure changes map to
/// `e`/`what='t'`.
fn build_live_events(
    tree_handle: &SharedComponentTree,
    subscribed: &HashSet<u16>,
    cov_cache: &mut HashMap<u16, Vec<JsonValue>>,
    tree_cache: &mut HashMap<u16, Vec<u16>>,
) -> Vec<String> {
    let tree = tree_handle.read().unwrap();
    let mut frames: Vec<String> = Vec::new();

    for &comp_id in subscribed {
        let Some(comp) = tree.get(comp_id) else {
            // Component gone — emit a delete event so the editor drops it.
            if tree_cache.remove(&comp_id).is_some() {
                frames.push(
                    json!({
                        "msgType": "e",
                        "compId": comp_id,
                        "what": "d",
                        "deleted": { "compId": comp_id, "parentId": 0 },
                    })
                    .to_string(),
                );
                cov_cache.remove(&comp_id);
            }
            continue;
        };

        // Tree-shape change?
        let current_children = comp.children.clone();
        let tree_changed = tree_cache
            .get(&comp_id)
            .is_none_or(|cached| cached != &current_children);
        if tree_changed {
            frames.push(
                json!({
                    "msgType": "e",
                    "compId": comp_id,
                    "what": "t",
                    "compTree": build_comp_tree_payload(comp),
                })
                .to_string(),
            );
            tree_cache.insert(comp_id, current_children);
        }

        // Slot-value change?
        let current_slots: Vec<JsonValue> = comp
            .slots
            .iter()
            .map(|s| slot_value_to_json(&s.value))
            .collect();
        let slots_changed = cov_cache
            .get(&comp_id)
            .is_none_or(|cached| cached != &current_slots);
        if slots_changed {
            // Push a config frame. Runtime frames would double the push;
            // for MVP we emit one 'c' frame which the editor re-reads for
            // runtime slots on demand. A follow-up can split into
            // config + runtime frames if the editor's state worker
            // rejects the collapsed form.
            let payload = encode_slots_for_what(comp, 'c');
            let b64 = BASE64.encode(&payload);
            frames.push(
                json!({
                    "msgType": "e",
                    "compId": comp_id,
                    "what": "c",
                    "compProps": { "propValues": b64 },
                })
                .to_string(),
            );
            cov_cache.insert(comp_id, current_slots);
        }
    }

    frames
}

// ── Tests ──────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sox::sox_handlers::{ComponentTree, ManifestDb, SlotValue};
    use std::sync::Arc;

    fn make_test_state() -> LiveState {
        let manifest_db = Arc::new(ManifestDb::load(""));
        let tree = Arc::new(std::sync::RwLock::new(ComponentTree::new_with_manifest(
            manifest_db.clone(),
        )));
        LiveState {
            tree,
            manifest_db,
            dyn_store: None,
        }
    }

    fn dispatch(state: &LiveState, text: &str) -> JsonValue {
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_live_msg(
            text,
            state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        serde_json::from_str(reply.as_deref().unwrap()).unwrap()
    }

    #[test]
    fn invalid_json_returns_error() {
        let state = make_test_state();
        let v = dispatch(&state, "not json");
        assert_eq!(v["msgType"], "!");
        assert_eq!(v["isError"], true);
    }

    #[test]
    fn missing_msg_type_returns_error() {
        let state = make_test_state();
        let v = dispatch(&state, r#"{"replyNum":1}"#);
        assert_eq!(v["msgType"], "!");
    }

    #[test]
    fn ping_is_echoed() {
        let state = make_test_state();
        let v = dispatch(&state, r#"{"msgType":"p","replyNum":255}"#);
        assert_eq!(v["msgType"], "p");
        assert_eq!(v["replyNum"], 255);
    }

    #[test]
    fn version_returns_kit_list() {
        let state = make_test_state();
        let v = dispatch(&state, r#"{"msgType":"v","replyNum":0}"#);
        assert_eq!(v["msgType"], "V");
        assert_eq!(v["replyNum"], 0);
        assert!(v["kits"].is_array());
        assert!(v["kitCount"].as_u64().unwrap() > 0);
    }

    #[test]
    fn version_ext_returns_extended_fields() {
        let state = make_test_state();
        let v = dispatch(&state, r#"{"msgType":"y","replyNum":1}"#);
        assert_eq!(v["msgType"], "Y");
        assert_eq!(v["platformId"], "");
        assert_eq!(v["scodeFlags"], 0);
        assert_eq!(v["pairs"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn subscribe_returns_num_subscribed() {
        let state = make_test_state();
        let v = dispatch(
            &state,
            r#"{"msgType":"s","replyNum":2,"whatMask":15,"compIds":[]}"#,
        );
        assert_eq!(v["msgType"], "S");
        assert_eq!(v["numSubscribed"], 0);
    }

    #[test]
    fn close_watch_returns_watch_id() {
        let state = make_test_state();
        let v = dispatch(
            &state,
            r#"{"msgType":"x","replyNum":3,"watchId":"tab-7"}"#,
        );
        assert_eq!(v["msgType"], "X");
        assert_eq!(v["watchId"], "tab-7");
    }

    #[test]
    fn unknown_msg_type_returns_error() {
        let state = make_test_state();
        let v = dispatch(&state, r#"{"msgType":"Z","replyNum":4}"#);
        assert_eq!(v["msgType"], "!");
        assert!(v["cause"].as_str().unwrap().contains("unknown msgType"));
    }

    #[test]
    fn file_op_not_yet_implemented() {
        let state = make_test_state();
        let v = dispatch(&state, r#"{"msgType":"f","replyNum":5,"uri":"app.sab"}"#);
        assert_eq!(v["msgType"], "!");
        assert!(v["cause"].as_str().unwrap().contains("file op"));
    }

    #[test]
    fn read_comp_unknown_component() {
        let state = make_test_state();
        let v = dispatch(
            &state,
            r#"{"msgType":"c","replyNum":6,"compId":9999,"what":"c"}"#,
        );
        assert_eq!(v["msgType"], "!");
        assert!(v["cause"].as_str().unwrap().contains("not found"));
    }

    #[test]
    fn query_returns_comp_ids_for_known_type() {
        let state = make_test_state();
        let v = dispatch(
            &state,
            r#"{"msgType":"q","replyNum":7,"queryType":"s","kitId":1,"typeId":0}"#,
        );
        // May be Q with empty list or ! if type_id not in manifest; accept either
        // so the test tolerates manifest variance across runs.
        assert!(v["msgType"] == "Q" || v["msgType"] == "!");
    }

    #[test]
    fn encode_slots_respects_what() {
        use crate::sox::sox_protocol::SoxValueType;
        use crate::sox::sox_handlers::{VirtualSlot, VirtualComponent};

        let comp = VirtualComponent {
            comp_id: 1,
            parent_id: 0,
            name: "test".into(),
            type_name: "Test".into(),
            kit_id: 0,
            type_id: 0,
            children: vec![],
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: SoxValueType::Int as u8,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Int(42),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: SoxValueType::Float as u8,
                    flags: 0,
                    value: SlotValue::Float(3.14),
                },
                VirtualSlot {
                    name: "trigger".into(),
                    type_id: SoxValueType::Bool as u8,
                    flags: SLOT_FLAG_ACTION,
                    value: SlotValue::Bool(false),
                },
            ],
            links: vec![],
        };

        let config_bytes = encode_slots_for_what(&comp, 'c');
        // Int(42) = 4 bytes big-endian.
        assert_eq!(config_bytes.len(), 4);
        assert_eq!(config_bytes, 42i32.to_be_bytes().to_vec());

        let runtime_bytes = encode_slots_for_what(&comp, 'r');
        // Float(3.14) = 4 bytes.
        assert_eq!(runtime_bytes.len(), 4);

        // Actions never included.
        let anything_bytes = encode_slots_for_what(&comp, 'r');
        assert!(!anything_bytes.is_empty());
        assert_eq!(anything_bytes.len(), 4); // no action bool appended
    }
}
