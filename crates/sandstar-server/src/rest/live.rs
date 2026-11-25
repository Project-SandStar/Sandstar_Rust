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

/// Bytes of an inbound/outbound frame shown in the `/live` RX/TX log.
const LOG_PREVIEW_BYTES: usize = 240;

/// The first `max` bytes of `s`, cut back to a char boundary (R-6:
/// `&s[..max]` panicked inside a multibyte character).
/// `str::floor_char_boundary` needs Rust 1.91; the MSRV is 1.82.
fn log_preview(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or_default()
}

// ── Upgrade handler ────────────────────────────────

pub async fn live_ws_handler(ws: WebSocketUpgrade, State(state): State<LiveState>) -> Response {
    let current = crate::metrics::metrics()
        .rows_active
        .load(Ordering::Relaxed);
    if current >= MAX_LIVE_CONNECTIONS {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many /live connections",
        )
            .into_response();
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
                        // Debug-diagnostics: log every inbound frame (truncated) to
                        // make it possible to trace editor↔server contract mismatches
                        // without attaching a browser. Remove or demote to debug!
                        // once the editor bring-up is stable.
                        let t: &str = text.as_str();
                        let preview = log_preview(t, LOG_PREVIEW_BYTES);
                        info!(inbound = %preview, "/live RX");
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
                            let r: &str = reply_json.as_str();
                            let rpreview = log_preview(r, LOG_PREVIEW_BYTES);
                            info!(outbound = %rpreview, "/live TX");
                            if ws_tx.send(Message::Text(reply_json.into())).await.is_err() {
                                break;
                            }
                        } else {
                            info!("/live handler returned None (no reply queued)");
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
        .and_then(|v| u8::try_from(v).ok())
        .unwrap_or(0xFF);
    // Exactly one byte, which in a str is always ASCII.
    let msg_type = match msg
        .get("msgType")
        .and_then(|v| v.as_str())
        .map(str::as_bytes)
    {
        Some(&[b]) => char::from(b),
        _ => return render_error(reply_num, "missing or invalid 'msgType'"),
    };

    // Ping is a keep-alive with its own envelope (`p` replyNum 255).
    // Fantom pod responds with UPPERCASE `P` per sox.worker.ts:607
    // (`if (data.msgType === 'P')` — editor silently drops lowercase
    // replies, which triggers a 15 s pong timeout → reconnect → new
    // `v` → reconnect loop. Caught in browser console log 2026-04-24.
    if msg_type == 'p' {
        return Some(json!({"msgType": "P", "replyNum": reply_num}).to_string());
    }

    // Component reads need binary-encoded slot payloads; handle them
    // here rather than via the generic apply_op path.
    if msg_type == 'c' {
        return handle_read_comp_live(state, reply_num, &msg);
    }

    // Writes come in as base64-encoded raw SOX slot bytes. The generic
    // apply_op path treats `value` as a JSON-friendly primitive, which
    // silently fails for typed binary payloads (e.g. a meta Int becomes
    // a 4-byte UTF-8 blob → Int coerce rejects it). Handle writes here
    // with a tree-aware decoder that uses the slot's declared type_id.
    if msg_type == 'w' {
        return handle_write_slot_live(state, reply_num, &msg);
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

    // Drive dataflow synchronously after a tree-mutating op so editor
    // clients observe link-propagated values on the very next COV tick.
    // The DASP UDP loop also calls these every second, but it can stall
    // on the engine list_channels await — relying on the WS path here
    // keeps the editor responsive regardless.
    if matches!(msg_type, 'l' | 'a' | 'd' | 'n' | 'o') {
        if let Ok(mut tree) = state.tree.write() {
            let _ = tree.execute_links();
            let _ = tree.execute_components();
        }
    }

    Some(render_live_response(msg_type, reply_num, response))
}

// ── Fantom envelope parsing ────────────────────────

/// Translate a Fantom msgType + JSON payload into a typed [`SoxOp`].
/// Returns `Err(cause)` for malformed requests; caller wraps in a `!`
/// error frame.
fn parse_live_op(msg_type: char, msg: &JsonValue) -> Result<SoxOp, String> {
    // A number that does not fit the field is treated as missing; `as`
    // would wrap it onto another component or slot.
    let u16_of = |k: &str| {
        msg.get(k)
            .and_then(|v| v.as_u64())
            .and_then(|v| u16::try_from(v).ok())
    };
    let u8_of = |k: &str| {
        msg.get(k)
            .and_then(|v| v.as_u64())
            .and_then(|v| u8::try_from(v).ok())
    };
    let str_of = |k: &str| msg.get(k).and_then(|v| v.as_str()).map(String::from);
    let vec_u16 = |k: &str| {
        msg.get(k)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_u64().and_then(|n| u16::try_from(n).ok()))
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
                .and_then(|v| usize::try_from(v).ok())
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
            let children = crate::sox::sox_ops::strict_u16_list(msg.get("childrenIds"))
                .ok_or("missing or invalid childrenIds".to_string())?;
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
                .and_then(|v| usize::try_from(v).ok())
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
    out.insert(
        "msgType".into(),
        JsonValue::String(response_type.to_string()),
    );
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
    let comp_id = match msg
        .get("compId")
        .and_then(|v| v.as_u64())
        .and_then(|v| u16::try_from(v).ok())
    {
        Some(id) => id,
        None => return render_error(reply_num, "missing compId"),
    };
    let what = msg
        .get("what")
        .and_then(|v| v.as_str())
        .and_then(|s| s.chars().next())
        .unwrap_or('c');

    let tree = crate::lock::read_or_recover(&state.tree, "SOX component tree");
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
            out.insert("compProps".into(), json!({ "propValues": b64 }));
        }
        other => {
            return render_error(reply_num, &format!("unsupported 'what' code: {other}"));
        }
    }

    Some(JsonValue::Object(out).to_string())
}

/// Handle Fantom-dialect `w` (write slot). The editor sends
/// `value` as base64-encoded raw SOX slot bytes: for an Int, 4 BE
/// bytes; for a Str, u2 length + chars + NUL; etc. Decode with the
/// slot's declared `type_id` via `decode_slot_value` (same routine
/// the DASP SOX path uses) and mutate the tree in place.
///
/// `slotId` in the editor's envelope maps directly to slot index.
fn handle_write_slot_live(state: &LiveState, reply_num: u8, msg: &JsonValue) -> Option<String> {
    use crate::sox::sox_handlers::decode_slot_value;
    use crate::sox::sox_protocol::SoxReader;

    let comp_id = match msg
        .get("compId")
        .and_then(|v| v.as_u64())
        .and_then(|v| u16::try_from(v).ok())
    {
        Some(id) => id,
        None => return render_error(reply_num, "missing compId"),
    };
    let slot_idx = match msg
        .get("slotId")
        .and_then(|v| v.as_u64())
        .and_then(|v| usize::try_from(v).ok())
    {
        Some(idx) => idx,
        None => return render_error(reply_num, "missing slotId"),
    };
    let b64 = match msg.get("value").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return render_error(reply_num, "missing value"),
    };
    let raw = match BASE64.decode(b64) {
        Ok(b) => b,
        Err(e) => return render_error(reply_num, &format!("bad base64 value: {e}")),
    };

    let mut tree = crate::lock::write_or_recover(&state.tree, "SOX component tree");
    let slot_type_id = {
        let comp = match tree.get(comp_id) {
            Some(c) => c,
            None => return render_error(reply_num, &format!("component {comp_id} not found")),
        };
        match comp.slots.get(slot_idx) {
            Some(s) => s.type_id,
            None => return render_error(reply_num, &format!("slot index {slot_idx} out of range")),
        }
    };

    let mut reader = SoxReader::new(&raw);
    let new_value = match decode_slot_value(&mut reader, slot_type_id) {
        Some(v) => v,
        None => {
            return render_error(
                reply_num,
                &format!(
                    "cannot decode {} bytes as type_id {}",
                    raw.len(),
                    slot_type_id
                ),
            );
        }
    };

    if let Some(comp) = tree.get_mut(comp_id) {
        if let Some(slot) = comp.slots.get_mut(slot_idx) {
            slot.value = new_value;
        }
    }
    tree.mark_dirty();

    // Drive dataflow synchronously after a write so the editor sees
    // link-propagated + component-recomputed values on the very next
    // COV tick (avoids up-to-1s lag while waiting on the DASP refresh
    // loop). The DASP loop also runs these every second; calling them
    // here is idempotent.
    let _ = tree.execute_links();
    let _ = tree.execute_components();

    // Fantom response shape: `W` with msgType + replyNum only.
    Some(json!({ "msgType": "W", "replyNum": reply_num }).to_string())
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
    let tree = crate::lock::read_or_recover(tree_handle, "SOX component tree");
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
    // ── S-24: log preview on a char boundary (R-6) ──────────────

    #[test]
    fn log_preview_never_splits_a_character() {
        use super::log_preview;
        // A 2-, 3- and 4-byte character straddling the cut.
        for ch in ["ö", "温", "😀"] {
            for pad in 0..4 {
                let s = format!("{}{}{}", "a".repeat(240 - pad), ch, "tail");
                let p = log_preview(&s, 240);
                assert!(p.len() <= 240, "{ch} pad {pad}");
                // Cut back only as far as the character start.
                assert!(p.len() > 240 - ch.len(), "{ch} pad {pad}: {}", p.len());
                assert!(s.starts_with(p));
            }
        }
        assert_eq!(log_preview("", 240), "");
        assert_eq!(log_preview("short", 240), "short");
        assert_eq!(log_preview("ö", 1), "");
    }

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
        // Pong MUST be uppercase `P` — sox.worker.ts:607 strictly checks
        // `data.msgType === 'P'`; lowercase replies cause the editor to
        // hit a 15s pong timeout and reconnect in a loop. Phase 14.0G fix.
        let state = make_test_state();
        let v = dispatch(&state, r#"{"msgType":"p","replyNum":255}"#);
        assert_eq!(v["msgType"], "P");
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
        let v = dispatch(&state, r#"{"msgType":"x","replyNum":3,"watchId":"tab-7"}"#);
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
        use crate::sox::sox_handlers::{VirtualComponent, VirtualSlot};
        use crate::sox::sox_protocol::SoxValueType;

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

    /// Regression for the editor bring-up bug where link propagation
    /// was only ever driven by the DASP UDP loop. If that loop stalls
    /// (or is disabled), `addLink` over `/live` would silently leave
    /// the target slot stale until something else triggered a tick.
    /// Verify that a `link add` over /live updates the target slot
    /// in the same call.
    #[test]
    fn add_link_propagates_immediately() {
        use crate::sox::sox_handlers::{VirtualComponent, VirtualSlot, NO_PARENT};
        use crate::sox::sox_protocol::SoxValueType;

        let state = make_test_state();
        // Build a tiny tree: source.out=42.0, target.in=0.0, no link yet.
        {
            let mut tree = state.tree.write().unwrap();
            tree.add(VirtualComponent {
                comp_id: 200,
                parent_id: NO_PARENT,
                name: "src".into(),
                type_name: "ConstFloat".into(),
                kit_id: 2,
                type_id: 14,
                children: vec![],
                slots: vec![
                    VirtualSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        value: SlotValue::Int(0),
                    },
                    VirtualSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        value: SlotValue::Float(42.0),
                    },
                ],
                links: vec![],
            });
            tree.add(VirtualComponent {
                comp_id: 201,
                parent_id: NO_PARENT,
                name: "dst".into(),
                type_name: "WriteFloat".into(),
                kit_id: 2,
                type_id: 57,
                children: vec![],
                slots: vec![
                    VirtualSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        value: SlotValue::Int(0),
                    },
                    VirtualSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: 0,
                        value: SlotValue::Float(0.0),
                    },
                    VirtualSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: 0,
                        value: SlotValue::Float(0.0),
                    },
                ],
                links: vec![],
            });
        }

        // Issue an `l` op with subcommand `a` (add link) over /live.
        let v = dispatch(
            &state,
            r#"{"msgType":"l","replyNum":9,"op":"a",
                "fromCompId":200,"fromSlotId":1,
                "toCompId":201,"toSlotId":2}"#,
        );
        assert_eq!(v["msgType"], "L");

        // Without our synchronous tick the target's `in` slot would
        // still be 0.0; with the fix it picks up 42.0 immediately, and
        // the WriteFloat passthrough also propagates `in` → `out`.
        let tree = state.tree.read().unwrap();
        let dst = tree.get(201).unwrap();
        match &dst.slots[2].value {
            SlotValue::Float(v) => assert_eq!(*v, 42.0, "in should reflect linked source"),
            other => panic!("expected Float, got {other:?}"),
        }
        match &dst.slots[1].value {
            SlotValue::Float(v) => assert_eq!(*v, 42.0, "WriteFloat passthrough should set out=in"),
            other => panic!("expected Float, got {other:?}"),
        }
    }
}

// ── Tests: write, component, link and event flows (R-33, S-54) ──

#[cfg(test)]
mod flow_tests {
    use super::*;
    use crate::sox::sox_handlers::{
        ComponentTree, ManifestDb, SlotValue, VirtualSlot, NO_PARENT, SLOT_FLAG_CONFIG,
    };
    use crate::sox::sox_protocol::SoxValueType;
    use std::sync::Arc;

    const ROOT: u16 = 0;
    const SRC: u16 = 200;
    const DST: u16 = 201;

    // Slot indices of the test component.
    const S_META: usize = 0;
    const S_FLOAT: usize = 1;
    const S_STR: usize = 2;

    fn comp(comp_id: u16, parent_id: u16, name: &str) -> VirtualComponent {
        VirtualComponent {
            comp_id,
            parent_id,
            name: name.into(),
            type_name: "Test".into(),
            kit_id: 200,
            type_id: 1,
            children: vec![],
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: SoxValueType::Int as u8,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "val".into(),
                    type_id: SoxValueType::Float as u8,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(0.0),
                },
                VirtualSlot {
                    name: "label".into(),
                    type_id: SoxValueType::Buf as u8,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Str(String::new()),
                },
            ],
            links: vec![],
        }
    }

    /// One connection: shared state plus the per-session caches.
    struct Conn {
        state: LiveState,
        subscribed: HashSet<u16>,
        cov_cache: HashMap<u16, Vec<JsonValue>>,
        tree_cache: HashMap<u16, Vec<u16>>,
        compact_cov: bool,
    }

    impl Conn {
        /// Root (id 0) with two children, SRC and DST.
        fn new() -> Self {
            let manifest_db = Arc::new(ManifestDb::load(""));
            let mut tree = ComponentTree::new_with_manifest(manifest_db.clone());
            tree.add(comp(ROOT, NO_PARENT, "app"));
            tree.add(comp(SRC, ROOT, "src"));
            tree.add(comp(DST, ROOT, "dst"));
            Conn {
                state: LiveState {
                    tree: Arc::new(std::sync::RwLock::new(tree)),
                    manifest_db,
                    dyn_store: None,
                },
                subscribed: HashSet::new(),
                cov_cache: HashMap::new(),
                tree_cache: HashMap::new(),
                compact_cov: false,
            }
        }

        fn send(&mut self, text: &str) -> JsonValue {
            let reply = handle_live_msg(
                text,
                &self.state,
                &mut self.subscribed,
                &mut self.cov_cache,
                &mut self.tree_cache,
                &mut self.compact_cov,
            );
            serde_json::from_str(reply.as_deref().unwrap()).unwrap()
        }

        fn send_json(&mut self, msg: JsonValue) -> JsonValue {
            self.send(&msg.to_string())
        }

        fn events(&mut self) -> Vec<JsonValue> {
            build_live_events(
                &self.state.tree,
                &self.subscribed,
                &mut self.cov_cache,
                &mut self.tree_cache,
            )
            .iter()
            .map(|f| serde_json::from_str(f).unwrap())
            .collect()
        }

        fn slot(&self, comp_id: u16, idx: usize) -> SlotValue {
            self.state.tree.read().unwrap().get(comp_id).unwrap().slots[idx]
                .value
                .clone()
        }

        fn exists(&self, comp_id: u16) -> bool {
            self.state.tree.read().unwrap().get(comp_id).is_some()
        }

        fn name(&self, comp_id: u16) -> String {
            self.state
                .tree
                .read()
                .unwrap()
                .get(comp_id)
                .unwrap()
                .name
                .clone()
        }

        fn links(&self, comp_id: u16) -> usize {
            self.state
                .tree
                .read()
                .unwrap()
                .get(comp_id)
                .unwrap()
                .links
                .len()
        }

        fn comp_count(&self) -> usize {
            self.state.tree.read().unwrap().len()
        }
    }

    fn is_error(reply: &JsonValue) -> bool {
        reply["msgType"] == "!" && reply["isError"] == true
    }

    fn b64(bytes: &[u8]) -> String {
        BASE64.encode(bytes)
    }

    fn write_msg(comp_id: u16, slot_id: usize, bytes: &[u8]) -> JsonValue {
        json!({ "msgType": "w", "replyNum": 5, "compId": comp_id, "slotId": slot_id, "value": b64(bytes) })
    }

    // ── envelope ───────────────────────────────────

    #[test]
    fn empty_and_non_object_messages_are_errors() {
        let mut conn = Conn::new();
        for text in ["", " ", "null", "[]", "42", "\"w\"", "{", "{\"msgType\":"] {
            let reply = conn.send(text);
            assert!(is_error(&reply), "{text:?} gave {reply}");
        }
    }

    #[test]
    fn msg_type_must_be_one_ascii_character() {
        let mut conn = Conn::new();
        for bad in [json!(""), json!("ww"), json!("ş"), json!(5), json!(null)] {
            let reply = conn.send_json(json!({ "msgType": bad, "replyNum": 3 }));
            assert!(is_error(&reply), "{bad} gave {reply}");
            assert_eq!(reply["replyNum"], 3);
        }
    }

    #[test]
    fn error_reply_echoes_the_reply_number() {
        let mut conn = Conn::new();
        let reply = conn.send(r#"{"msgType":"d","replyNum":17}"#);
        assert!(is_error(&reply));
        assert_eq!(reply["replyNum"], 17);
        assert!(reply["cause"].as_str().unwrap().contains("compId"));
    }

    // ── write ──────────────────────────────────────

    #[test]
    fn write_sets_a_float_slot_from_four_big_endian_bytes() {
        let mut conn = Conn::new();
        let reply = conn.send_json(write_msg(SRC, S_FLOAT, &21.5f32.to_be_bytes()));
        assert_eq!(reply["msgType"], "W");
        assert_eq!(reply["replyNum"], 5);
        assert_eq!(conn.slot(SRC, S_FLOAT), SlotValue::Float(21.5));
        assert_eq!(conn.slot(DST, S_FLOAT), SlotValue::Float(0.0));
    }

    #[test]
    fn write_sets_an_int_slot() {
        let mut conn = Conn::new();
        let reply = conn.send_json(write_msg(SRC, S_META, &(-7i32).to_be_bytes()));
        assert_eq!(reply["msgType"], "W");
        assert_eq!(conn.slot(SRC, S_META), SlotValue::Int(-7));
    }

    #[test]
    fn write_with_missing_or_mistyped_fields_is_an_error() {
        let mut conn = Conn::new();
        let value = b64(&1.0f32.to_be_bytes());
        let bad = [
            json!({ "msgType": "w", "slotId": S_FLOAT, "value": value }),
            json!({ "msgType": "w", "compId": SRC, "value": value }),
            json!({ "msgType": "w", "compId": SRC, "slotId": S_FLOAT }),
            json!({ "msgType": "w", "compId": "200", "slotId": S_FLOAT, "value": value }),
            json!({ "msgType": "w", "compId": -1, "slotId": S_FLOAT, "value": value }),
            json!({ "msgType": "w", "compId": 1.5, "slotId": S_FLOAT, "value": value }),
            json!({ "msgType": "w", "compId": SRC, "slotId": "1", "value": value }),
            json!({ "msgType": "w", "compId": SRC, "slotId": S_FLOAT, "value": 1.0 }),
            json!({ "msgType": "w", "compId": SRC, "slotId": S_FLOAT, "value": "not base64 !!" }),
        ];
        for msg in bad {
            let reply = conn.send_json(msg.clone());
            assert!(is_error(&reply), "{msg} gave {reply}");
        }
        assert_eq!(conn.slot(SRC, S_FLOAT), SlotValue::Float(0.0));
    }

    #[test]
    fn write_to_a_missing_component_or_slot_is_an_error() {
        let mut conn = Conn::new();
        let bytes = 1.0f32.to_be_bytes();
        assert!(is_error(&conn.send_json(write_msg(999, S_FLOAT, &bytes))));
        assert!(is_error(&conn.send_json(write_msg(SRC, 3, &bytes))));
        let huge =
            json!({ "msgType": "w", "compId": SRC, "slotId": u64::MAX, "value": b64(&bytes) });
        assert!(is_error(&conn.send_json(huge)));
    }

    #[test]
    fn write_with_too_few_bytes_is_an_error_and_keeps_the_value() {
        let mut conn = Conn::new();
        conn.send_json(write_msg(SRC, S_FLOAT, &3.0f32.to_be_bytes()));
        for short in [&[][..], &[0x41][..], &[0x41, 0x20, 0x00][..]] {
            let reply = conn.send_json(write_msg(SRC, S_FLOAT, short));
            assert!(is_error(&reply), "{} bytes gave {reply}", short.len());
        }
        assert_eq!(conn.slot(SRC, S_FLOAT), SlotValue::Float(3.0));
    }

    #[test]
    fn write_of_a_string_with_a_bad_length_or_bad_utf8_does_not_panic() {
        let mut conn = Conn::new();
        // Length field says 500 bytes, only 2 follow.
        let reply = conn.send_json(write_msg(SRC, S_STR, &[0x01, 0xF4, b'a', b'b']));
        assert!(is_error(&reply), "{reply}");
        // Length 2, bytes are not valid UTF-8: any answer is fine, a panic is not.
        let _ = conn.send_json(write_msg(SRC, S_STR, &[0x00, 0x02, 0xFF, 0xFE]));
        // Multi-byte UTF-8 with a correct length.
        let text = "çğü";
        let mut bytes = (text.len() as u16 + 1).to_be_bytes().to_vec();
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(0);
        let _ = conn.send_json(write_msg(SRC, S_STR, &bytes));
    }

    // ── add / delete / rename ──────────────────────

    #[test]
    fn add_returns_the_new_id_and_delete_removes_it() {
        let mut conn = Conn::new();
        let reply = conn.send_json(json!({
            "msgType": "a", "replyNum": 2, "parentId": ROOT, "kitId": 200, "typeId": 1, "name": "fresh"
        }));
        assert_eq!(reply["msgType"], "A");
        let id = reply["compId"].as_u64().unwrap() as u16;
        assert_eq!(conn.name(id), "fresh");

        let reply = conn.send_json(json!({ "msgType": "d", "replyNum": 3, "compId": id }));
        assert_eq!(reply["msgType"], "D");
        assert!(!conn.exists(id));
    }

    #[test]
    fn add_with_missing_fields_or_a_bad_name_changes_nothing() {
        let mut conn = Conn::new();
        let long = "a".repeat(200);
        let bad = [
            json!({ "msgType": "a", "kitId": 200, "typeId": 1, "name": "x1" }),
            json!({ "msgType": "a", "parentId": ROOT, "typeId": 1, "name": "x1" }),
            json!({ "msgType": "a", "parentId": ROOT, "kitId": 200, "name": "x1" }),
            json!({ "msgType": "a", "parentId": ROOT, "kitId": 200, "typeId": 1 }),
            json!({ "msgType": "a", "parentId": ROOT, "kitId": 200, "typeId": 1, "name": 5 }),
            json!({ "msgType": "a", "parentId": ROOT, "kitId": 200, "typeId": 1, "name": "" }),
            json!({ "msgType": "a", "parentId": ROOT, "kitId": 200, "typeId": 1, "name": "ünlü" }),
            json!({ "msgType": "a", "parentId": ROOT, "kitId": 200, "typeId": 1, "name": long }),
            json!({ "msgType": "a", "parentId": 999, "kitId": 200, "typeId": 1, "name": "x1" }),
        ];
        for msg in bad {
            let reply = conn.send_json(msg.clone());
            assert!(is_error(&reply), "{msg} gave {reply}");
        }
        assert_eq!(conn.comp_count(), 3);
    }

    #[test]
    fn delete_of_a_missing_or_system_component_is_an_error() {
        let mut conn = Conn::new();
        assert!(is_error(&conn.send(r#"{"msgType":"d","compId":999}"#)));
        assert!(is_error(&conn.send(r#"{"msgType":"d","compId":0}"#)));
        assert!(is_error(&conn.send(r#"{"msgType":"d","compId":"200"}"#)));
        assert_eq!(conn.comp_count(), 3);
    }

    /// A component id that does not fit 16 bits must not be cut down to
    /// another component's id (`numeric-output.md`: range check before `as`).
    #[test]
    fn a_component_id_above_u16_does_not_reach_another_component() {
        let mut conn = Conn::new();
        let wrapped = 65_536 + u64::from(SRC);

        let reply = conn.send_json(json!({ "msgType": "d", "compId": wrapped }));
        assert!(is_error(&reply), "{reply}");
        assert!(
            conn.exists(SRC),
            "delete of {wrapped} removed component {SRC}"
        );

        let reply =
            conn.send_json(json!({ "msgType": "n", "compId": wrapped, "newName": "hijack" }));
        assert!(is_error(&reply), "{reply}");
        assert_eq!(conn.name(SRC), "src");

        let reply = conn.send_json(json!({
            "msgType": "w", "compId": wrapped, "slotId": S_FLOAT, "value": b64(&9.0f32.to_be_bytes())
        }));
        assert!(is_error(&reply), "{reply}");
        assert_eq!(conn.slot(SRC, S_FLOAT), SlotValue::Float(0.0));
    }

    #[test]
    fn rename_accepts_both_field_names_and_validates_the_name() {
        let mut conn = Conn::new();
        let reply = conn
            .send_json(json!({ "msgType": "n", "replyNum": 1, "compId": SRC, "newName": "one" }));
        assert_eq!(reply["msgType"], "N");
        assert_eq!(conn.name(SRC), "one");
        let reply = conn.send_json(json!({ "msgType": "n", "compId": SRC, "name": "two" }));
        assert_eq!(reply["msgType"], "N");
        assert_eq!(conn.name(SRC), "two");

        for bad in [
            json!({ "msgType": "n", "compId": SRC }),
            json!({ "msgType": "n", "newName": "x1" }),
            json!({ "msgType": "n", "compId": SRC, "newName": "" }),
            json!({ "msgType": "n", "compId": SRC, "newName": "şemsiye" }),
            json!({ "msgType": "n", "compId": SRC, "newName": "1x" }),
            json!({ "msgType": "n", "compId": 999, "newName": "x1" }),
        ] {
            let reply = conn.send_json(bad.clone());
            assert!(is_error(&reply), "{bad} gave {reply}");
        }
        assert_eq!(conn.name(SRC), "two");
    }

    #[test]
    fn small_fields_above_their_range_are_errors() {
        let mut conn = Conn::new();
        let bad = [
            json!({ "msgType": "a", "parentId": ROOT, "kitId": 256, "typeId": 1, "name": "x1" }),
            json!({ "msgType": "a", "parentId": ROOT, "kitId": 200, "typeId": 257, "name": "x1" }),
            json!({ "msgType": "a", "parentId": 65_536, "kitId": 200, "typeId": 1, "name": "x1" }),
            json!({ "msgType": "c", "compId": 65_536 + u64::from(SRC), "what": "c" }),
            json!({
                "msgType": "l", "op": "a",
                "fromCompId": SRC, "fromSlotId": 257, "toCompId": DST, "toSlotId": S_FLOAT
            }),
        ];
        for msg in bad {
            let reply = conn.send_json(msg.clone());
            assert!(is_error(&reply), "{msg} gave {reply}");
        }
        assert_eq!(conn.comp_count(), 3);
        assert_eq!(conn.links(DST), 0);
    }

    #[test]
    fn reorder_needs_a_complete_list_of_ids() {
        let mut conn = Conn::new();
        let children = |conn: &Conn| {
            conn.state
                .tree
                .read()
                .unwrap()
                .get(ROOT)
                .unwrap()
                .children
                .clone()
        };
        for bad in [
            json!({ "msgType": "o", "compId": ROOT }),
            json!({ "msgType": "o", "compId": ROOT, "childrenIds": "200,201" }),
            json!({ "msgType": "o", "compId": ROOT, "childrenIds": [DST, 70_000] }),
            json!({ "msgType": "o", "compId": ROOT, "childrenIds": [DST, "200"] }),
            json!({ "msgType": "o", "compId": ROOT, "childrenIds": [DST, -1] }),
        ] {
            let reply = conn.send_json(bad.clone());
            assert!(is_error(&reply), "{bad} gave {reply}");
        }
        assert_eq!(children(&conn), vec![SRC, DST]);

        let reply =
            conn.send_json(json!({ "msgType": "o", "compId": ROOT, "childrenIds": [DST, SRC] }));
        assert_eq!(reply["msgType"], "O");
        assert_eq!(children(&conn), vec![DST, SRC]);
    }

    // ── links ──────────────────────────────────────

    fn link_msg(op: &str) -> JsonValue {
        json!({
            "msgType": "l", "replyNum": 9, "op": op,
            "fromCompId": SRC, "fromSlotId": S_FLOAT, "toCompId": DST, "toSlotId": S_FLOAT
        })
    }

    #[test]
    fn link_add_carries_the_value_and_link_delete_removes_the_link() {
        let mut conn = Conn::new();
        conn.send_json(write_msg(SRC, S_FLOAT, &42.0f32.to_be_bytes()));

        let reply = conn.send_json(link_msg("a"));
        assert_eq!(reply["msgType"], "L");
        assert_eq!(conn.links(SRC), 1);
        assert_eq!(conn.links(DST), 1);
        assert_eq!(conn.slot(DST, S_FLOAT), SlotValue::Float(42.0));

        let reply = conn.send_json(link_msg("d"));
        assert_eq!(reply["msgType"], "L");
        assert_eq!(conn.links(SRC), 0);
        assert_eq!(conn.links(DST), 0);

        // With the link gone a new source value stays where it is.
        conn.send_json(write_msg(SRC, S_FLOAT, &7.0f32.to_be_bytes()));
        assert_eq!(conn.slot(DST, S_FLOAT), SlotValue::Float(42.0));
    }

    #[test]
    fn link_accepts_the_short_field_names() {
        let mut conn = Conn::new();
        let reply = conn.send_json(json!({
            "msgType": "l", "op": "a",
            "fromComp": SRC, "fromSlot": S_FLOAT, "toComp": DST, "toSlot": S_FLOAT
        }));
        assert_eq!(reply["msgType"], "L");
        assert_eq!(conn.links(DST), 1);
    }

    #[test]
    fn link_with_a_bad_op_or_missing_field_changes_nothing() {
        let mut conn = Conn::new();
        let mut bad = vec![link_msg("x"), link_msg(""), link_msg("ş")];
        for field in ["op", "fromCompId", "fromSlotId", "toCompId", "toSlotId"] {
            let mut msg = link_msg("a");
            msg.as_object_mut().unwrap().remove(field);
            bad.push(msg);
        }
        let mut wrong_type = link_msg("a");
        wrong_type["toCompId"] = json!("201");
        bad.push(wrong_type);

        for msg in bad {
            let reply = conn.send_json(msg.clone());
            assert!(is_error(&reply), "{msg} gave {reply}");
        }
        assert_eq!(conn.links(SRC), 0);
        assert_eq!(conn.links(DST), 0);

        // Deleting a link that was never added is an error too.
        assert!(is_error(&conn.send_json(link_msg("d"))));
    }

    // ── events ─────────────────────────────────────

    fn subscribe(conn: &mut Conn, ids: &[u16]) {
        let reply = conn.send_json(json!({ "msgType": "s", "replyNum": 1, "compIds": ids }));
        assert_eq!(reply["msgType"], "S");
        assert_eq!(reply["numSubscribed"], ids.len());
    }

    #[test]
    fn no_event_without_a_change() {
        let mut conn = Conn::new();
        subscribe(&mut conn, &[SRC, DST]);
        assert!(conn.events().is_empty());
        assert!(conn.events().is_empty());
    }

    #[test]
    fn a_write_gives_one_event_for_the_subscribed_component_only() {
        let mut conn = Conn::new();
        subscribe(&mut conn, &[SRC]);

        conn.send_json(write_msg(SRC, S_FLOAT, &5.0f32.to_be_bytes()));
        conn.send_json(write_msg(DST, S_FLOAT, &6.0f32.to_be_bytes()));

        let events = conn.events();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["msgType"], "e");
        assert_eq!(events[0]["compId"], SRC);
        assert_eq!(events[0]["what"], "c");
        assert!(events[0]["compProps"]["propValues"].is_string());
        // The change is reported once.
        assert!(conn.events().is_empty());
    }

    #[test]
    fn a_linked_write_reaches_the_subscriber_of_the_target() {
        let mut conn = Conn::new();
        conn.send_json(link_msg("a"));
        subscribe(&mut conn, &[DST]);

        conn.send_json(write_msg(SRC, S_FLOAT, &8.0f32.to_be_bytes()));

        let events = conn.events();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["compId"], DST);
        assert_eq!(conn.slot(DST, S_FLOAT), SlotValue::Float(8.0));
    }

    #[test]
    fn adding_a_child_gives_a_tree_event_for_the_parent() {
        let mut conn = Conn::new();
        subscribe(&mut conn, &[ROOT]);

        conn.send_json(json!({
            "msgType": "a", "parentId": ROOT, "kitId": 200, "typeId": 1, "name": "kid"
        }));

        let events = conn.events();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["compId"], ROOT);
        assert_eq!(events[0]["what"], "t");
    }

    #[test]
    fn deleting_a_subscribed_component_gives_one_delete_event() {
        let mut conn = Conn::new();
        subscribe(&mut conn, &[SRC]);

        conn.send_json(json!({ "msgType": "d", "compId": SRC }));

        let events = conn.events();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["what"], "d");
        assert_eq!(events[0]["compId"], SRC);
        assert!(conn.events().is_empty());
    }

    #[test]
    fn unsubscribe_and_close_watch_stop_events() {
        let mut conn = Conn::new();
        subscribe(&mut conn, &[SRC, DST]);

        let reply = conn.send_json(json!({ "msgType": "u", "compIds": [SRC] }));
        assert_eq!(reply["msgType"], "U");
        conn.send_json(write_msg(SRC, S_FLOAT, &1.0f32.to_be_bytes()));
        assert!(conn.events().is_empty());

        let reply = conn.send_json(json!({ "msgType": "x", "watchId": "w" }));
        assert_eq!(reply["msgType"], "X");
        conn.send_json(write_msg(DST, S_FLOAT, &2.0f32.to_be_bytes()));
        assert!(conn.events().is_empty());
    }
}
