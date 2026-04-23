//! RoWS (ROX over WebSocket): real-time bidirectional component tree
//! operations and COV (Change of Value) push events.
//!
//! Clients connect to `GET /api/rows`, send JSON commands to read/mutate the
//! virtual component tree, and receive server-pushed COV updates for
//! subscribed components. The command dispatcher lives in
//! [`crate::sox::sox_ops`]; this file owns only the RoWS-specific JSON
//! envelope and the COV push loop.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value as JsonValue;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info};

use crate::sox::sox_ops::{apply_op, SessionState, SoxOp, SoxResponse};
use crate::sox::{DynSlotStoreHandle, SharedComponentTree};

use super::sox_api::slot_value_to_json;

// ── State ──────────────────────────────────────────

/// Alias preserved for existing call sites (router in `main.rs`, tests).
/// The canonical definition lives in [`crate::sox::sox_ops::SoxState`].
pub use crate::sox::sox_ops::SoxState as RowsState;

// ── Constants ──────────────────────────────────────

const MAX_ROWS_CONNECTIONS: i64 = 16;
const COV_INTERVAL_MS: u64 = 1000;
const CLIENT_TIMEOUT_SECS: u64 = 120;

// ── Upgrade handler ────────────────────────────────

pub async fn rows_ws_handler(ws: WebSocketUpgrade, State(state): State<RowsState>) -> Response {
    let current = crate::metrics::metrics()
        .rows_active
        .load(Ordering::Relaxed);
    if current >= MAX_ROWS_CONNECTIONS {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many RoWS connections").into_response();
    }
    ws.on_upgrade(|socket| handle_rows_session(socket, state))
}

// ── Session handler ────────────────────────────────

async fn handle_rows_session(ws: WebSocket, state: RowsState) {
    let metrics = crate::metrics::metrics();
    metrics.rows_active.fetch_add(1, Ordering::Relaxed);
    metrics.rows_total.fetch_add(1, Ordering::Relaxed);
    info!("RoWS connected");

    let (mut ws_tx, mut ws_rx) = ws.split();

    // Per-session COV subscription state
    let mut subscribed: HashSet<u16> = HashSet::new();
    // Cache of last-seen slot values per component (comp_id -> vec of json values)
    let mut cov_cache: HashMap<u16, Vec<JsonValue>> = HashMap::new();
    // Cache of last-seen children per component for tree change detection
    let mut tree_cache: HashMap<u16, Vec<u16>> = HashMap::new();
    // Cache of last-seen dynamic tags per component for tag change detection
    let mut tags_cache: HashMap<u16, JsonValue> = HashMap::new();
    // When true, COV messages use compact format (slot index + value only, no name).
    let mut compact_cov: bool = false;

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
                        let reply = handle_client_msg(
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
                // Check client timeout
                if last_client_msg.elapsed() > Duration::from_secs(CLIENT_TIMEOUT_SECS) {
                    debug!("RoWS client timeout after {}s inactivity", CLIENT_TIMEOUT_SECS);
                    break;
                }

                if subscribed.is_empty() {
                    continue;
                }

                // Build COV, tree-change, and tag-change messages
                let messages = build_cov_messages_with_tags(
                    &state.tree,
                    &subscribed,
                    &mut cov_cache,
                    &mut tree_cache,
                    compact_cov,
                    state.dyn_store.as_ref(),
                    Some(&mut tags_cache),
                );

                let mut disconnected = false;
                for msg_json in messages {
                    metrics.rows_messages_out.fetch_add(1, Ordering::Relaxed);
                    if ws_tx.send(Message::Text(msg_json.into())).await.is_err() {
                        disconnected = true;
                        break;
                    }
                }
                if disconnected { break; }
            }
        }
    }

    metrics.rows_active.fetch_sub(1, Ordering::Relaxed);
    info!("RoWS disconnected (subscriptions: {})", subscribed.len());
}

// ── COV message builder ────────────────────────────

/// Convenience wrapper for tests (no dynamic tag tracking).
#[cfg(test)]
fn build_cov_messages(
    tree_handle: &SharedComponentTree,
    subscribed: &HashSet<u16>,
    cov_cache: &mut HashMap<u16, Vec<JsonValue>>,
    tree_cache: &mut HashMap<u16, Vec<u16>>,
    compact_cov: bool,
) -> Vec<String> {
    build_cov_messages_with_tags(
        tree_handle,
        subscribed,
        cov_cache,
        tree_cache,
        compact_cov,
        None,
        None,
    )
}

fn build_cov_messages_with_tags(
    tree_handle: &SharedComponentTree,
    subscribed: &HashSet<u16>,
    cov_cache: &mut HashMap<u16, Vec<JsonValue>>,
    tree_cache: &mut HashMap<u16, Vec<u16>>,
    compact_cov: bool,
    dyn_store: Option<&DynSlotStoreHandle>,
    tags_cache: Option<&mut HashMap<u16, JsonValue>>,
) -> Vec<String> {
    let tree = tree_handle.read().unwrap();
    let mut messages = Vec::new();

    for &comp_id in subscribed {
        let Some(comp) = tree.get(comp_id) else {
            // Component was deleted — send treeChanged event
            if tree_cache.remove(&comp_id).is_some() {
                if let Ok(json) = serde_json::to_string(&serde_json::json!({
                    "op": "treeChanged",
                    "action": "deleted",
                    "compId": comp_id,
                })) {
                    messages.push(json);
                }
                cov_cache.remove(&comp_id);
            }
            continue;
        };

        // Check for tree structure changes (children added/removed)
        let current_children = &comp.children;
        if let Some(cached_children) = tree_cache.get(&comp_id) {
            if cached_children != current_children {
                if let Ok(json) = serde_json::to_string(&serde_json::json!({
                    "op": "treeChanged",
                    "action": "childrenChanged",
                    "compId": comp_id,
                    "children": current_children,
                })) {
                    messages.push(json);
                }
                tree_cache.insert(comp_id, current_children.clone());
            }
        } else {
            tree_cache.insert(comp_id, current_children.clone());
        }

        // Check for slot value changes
        let current_slots: Vec<JsonValue> = comp
            .slots
            .iter()
            .map(|s| slot_value_to_json(&s.value))
            .collect();

        let changed = if let Some(cached) = cov_cache.get(&comp_id) {
            if cached.len() != current_slots.len() {
                true
            } else {
                cached.iter().zip(current_slots.iter()).any(|(a, b)| a != b)
            }
        } else {
            true // First time seeing this component
        };

        if changed {
            // Build list of changed slots only
            let cached = cov_cache.get(&comp_id);
            let mut changed_slots = Vec::new();
            for (idx, (slot, val)) in comp.slots.iter().zip(current_slots.iter()).enumerate() {
                #[allow(clippy::option_map_or_none)]
                let is_new = !cached.and_then(|c| c.get(idx)).is_some_and(|cv| cv == val);
                if is_new {
                    if compact_cov {
                        // Compact mode: slot index + value only (no name).
                        // Client already has slot names from readComp / nameTable.
                        changed_slots.push(serde_json::json!({
                            "i": idx,
                            "v": val,
                        }));
                    } else {
                        changed_slots.push(serde_json::json!({
                            "index": idx,
                            "name": slot.name,
                            "value": val,
                        }));
                    }
                }
            }

            if !changed_slots.is_empty() {
                if let Ok(json) = serde_json::to_string(&serde_json::json!({
                    "op": "cov",
                    "compId": comp_id,
                    "slots": changed_slots,
                })) {
                    messages.push(json);
                }
            }

            cov_cache.insert(comp_id, current_slots);
        }
    }

    // Check for dynamic tag changes on subscribed components.
    if let (Some(ds), Some(tc)) = (dyn_store, tags_cache) {
        if let Ok(store) = ds.read() {
            for &comp_id in subscribed {
                let current_tags = store
                    .get_all(comp_id)
                    .map(|t| serde_json::to_value(t).unwrap_or_default())
                    .unwrap_or(serde_json::json!({}));
                let changed = tc
                    .get(&comp_id)
                    .map(|cached| cached != &current_tags)
                    .unwrap_or(
                        !current_tags
                            .as_object()
                            .map(|o| o.is_empty())
                            .unwrap_or(true),
                    );
                if changed {
                    if let Ok(json) = serde_json::to_string(&serde_json::json!({
                        "op": "tagsChanged",
                        "compId": comp_id,
                        "tags": current_tags,
                    })) {
                        messages.push(json);
                    }
                    tc.insert(comp_id, current_tags);
                }
            }
        }
    }

    messages
}

// ── Client message handler ─────────────────────────

fn handle_client_msg(
    text: &str,
    state: &RowsState,
    subscribed: &mut HashSet<u16>,
    cov_cache: &mut HashMap<u16, Vec<JsonValue>>,
    tree_cache: &mut HashMap<u16, Vec<u16>>,
    compact_cov: &mut bool,
) -> Option<String> {
    let msg: JsonValue = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(_) => {
            return make_error(None, "INVALID_MESSAGE", "invalid JSON");
        }
    };

    let id = msg.get("id").and_then(|v| v.as_str()).map(String::from);
    let op_name = match msg.get("op").and_then(|v| v.as_str()) {
        Some(o) => o,
        None => {
            return make_error(id.as_deref(), "INVALID_MESSAGE", "missing 'op' field");
        }
    };

    // Ping has its own envelope ({"op":"pong",...}) distinct from a normal
    // result — bypass apply_op to preserve that wire shape.
    if op_name == "ping" {
        return make_pong(id.as_deref());
    }

    let op = match parse_rows_op(op_name, &msg) {
        Ok(o) => o,
        Err((code, message)) => return make_error(id.as_deref(), code, &message),
    };

    let mut session = SessionState {
        subscribed,
        cov_cache,
        tree_cache,
        compact_cov,
    };
    match apply_op(state, op, &mut session) {
        SoxResponse::Ok(data) => make_result(id.as_deref(), data),
        SoxResponse::Error { code, message } => make_error(id.as_deref(), code, &message),
    }
}

/// Translate a RoWS JSON command into a typed [`SoxOp`].
///
/// Returns `Err((code, message))` for malformed requests; the caller wraps
/// these in RoWS's `{"op":"error",...}` envelope.
fn parse_rows_op(op: &str, msg: &JsonValue) -> Result<SoxOp, (&'static str, String)> {
    let u16_of = |k: &str| msg.get(k).and_then(|v| v.as_u64()).map(|v| v as u16);
    let u8_of = |k: &str| msg.get(k).and_then(|v| v.as_u64()).map(|v| v as u8);
    let usize_of = |k: &str| msg.get(k).and_then(|v| v.as_u64()).map(|v| v as usize);
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

    match op {
        "readTree" => Ok(SoxOp::ReadTree),
        "readComp" => u16_of("compId")
            .map(|comp_id| SoxOp::ReadComp { comp_id })
            .ok_or(("BAD_REQUEST", "missing or invalid 'compId'".into())),
        "writeSlot" => {
            let comp_id = u16_of("compId")
                .ok_or(("BAD_REQUEST", "missing compId".into()))?;
            let slot_idx = usize_of("slotIdx")
                .ok_or(("BAD_REQUEST", "missing slotIdx".into()))?;
            let value = msg
                .get("value")
                .cloned()
                .ok_or(("BAD_REQUEST", "missing value".into()))?;
            Ok(SoxOp::WriteSlot {
                comp_id,
                slot_idx,
                value,
            })
        }
        "addComp" => {
            let parent_id = u16_of("parentId")
                .ok_or(("BAD_REQUEST", "missing parentId".into()))?;
            let kit_id = u8_of("kitId")
                .ok_or(("BAD_REQUEST", "missing kitId".into()))?;
            let type_id = u8_of("typeId")
                .ok_or(("BAD_REQUEST", "missing typeId".into()))?;
            let name = str_of("name")
                .ok_or(("BAD_REQUEST", "missing name".into()))?;
            Ok(SoxOp::AddComp {
                parent_id,
                kit_id,
                type_id,
                name,
            })
        }
        "deleteComp" => u16_of("compId")
            .map(|comp_id| SoxOp::DeleteComp { comp_id })
            .ok_or(("BAD_REQUEST", "missing or invalid 'compId'".into())),
        "rename" => {
            let comp_id = u16_of("compId")
                .ok_or(("BAD_REQUEST", "missing compId".into()))?;
            let name = str_of("name")
                .ok_or(("BAD_REQUEST", "missing name".into()))?;
            Ok(SoxOp::Rename { comp_id, name })
        }
        "addLink" => {
            let from_comp = u16_of("fromComp")
                .ok_or(("BAD_REQUEST", "missing fromComp".into()))?;
            let from_slot = u8_of("fromSlot")
                .ok_or(("BAD_REQUEST", "missing fromSlot".into()))?;
            let to_comp = u16_of("toComp")
                .ok_or(("BAD_REQUEST", "missing toComp".into()))?;
            let to_slot = u8_of("toSlot")
                .ok_or(("BAD_REQUEST", "missing toSlot".into()))?;
            Ok(SoxOp::AddLink {
                from_comp,
                from_slot,
                to_comp,
                to_slot,
            })
        }
        "deleteLink" => {
            let from_comp = u16_of("fromComp")
                .ok_or(("BAD_REQUEST", "missing fromComp".into()))?;
            let from_slot = u8_of("fromSlot")
                .ok_or(("BAD_REQUEST", "missing fromSlot".into()))?;
            let to_comp = u16_of("toComp")
                .ok_or(("BAD_REQUEST", "missing toComp".into()))?;
            let to_slot = u8_of("toSlot")
                .ok_or(("BAD_REQUEST", "missing toSlot".into()))?;
            Ok(SoxOp::DeleteLink {
                from_comp,
                from_slot,
                to_comp,
                to_slot,
            })
        }
        "updatePos" => {
            let comp_id = u16_of("compId")
                .ok_or(("BAD_REQUEST", "missing compId".into()))?;
            let x = u8_of("x").ok_or(("BAD_REQUEST", "missing x".into()))?;
            let y = u8_of("y").ok_or(("BAD_REQUEST", "missing y".into()))?;
            Ok(SoxOp::UpdatePos { comp_id, x, y })
        }
        "invoke" => {
            let comp_id = u16_of("compId")
                .ok_or(("BAD_REQUEST", "missing compId".into()))?;
            let slot_idx = usize_of("slotIdx")
                .ok_or(("BAD_REQUEST", "missing slotIdx".into()))?;
            Ok(SoxOp::Invoke { comp_id, slot_idx })
        }
        "nameTable" => Ok(SoxOp::NameTable),
        "palette" => Ok(SoxOp::Palette),
        "subscribe" => Ok(SoxOp::Subscribe {
            comp_ids: vec_u16("compIds"),
        }),
        "unsubscribe" => Ok(SoxOp::Unsubscribe {
            comp_ids: vec_u16("compIds"),
        }),
        "readTags" => u16_of("compId")
            .map(|comp_id| SoxOp::ReadTags { comp_id })
            .ok_or(("BAD_REQUEST", "missing or invalid 'compId'".into())),
        "setTags" => {
            let comp_id = u16_of("compId")
                .ok_or(("BAD_REQUEST", "missing compId".into()))?;
            let tags = msg
                .get("tags")
                .cloned()
                .ok_or(("BAD_REQUEST", "missing tags".into()))?;
            Ok(SoxOp::SetTags { comp_id, tags })
        }
        "deleteTag" => {
            let comp_id = u16_of("compId")
                .ok_or(("BAD_REQUEST", "missing compId".into()))?;
            let key = str_of("key")
                .ok_or(("BAD_REQUEST", "missing key".into()))?;
            Ok(SoxOp::DeleteTag { comp_id, key })
        }
        "setFormat" => Ok(SoxOp::SetFormat {
            format: str_of("format").unwrap_or_else(|| "json".into()),
        }),
        // G-2b editor-parity ops
        "version" => Ok(SoxOp::Version),
        "versionExt" => Ok(SoxOp::VersionExt),
        "reorder" => {
            let comp_id = u16_of("compId")
                .ok_or(("BAD_REQUEST", "missing compId".into()))?;
            let children = vec_u16("children");
            Ok(SoxOp::Reorder { comp_id, children })
        }
        "query" => {
            let query_type = str_of("queryType")
                .and_then(|s| s.chars().next())
                .ok_or(("BAD_REQUEST", "missing queryType".into()))?;
            let kit_id = u8_of("kitId")
                .ok_or(("BAD_REQUEST", "missing kitId".into()))?;
            let type_id = u8_of("typeId")
                .ok_or(("BAD_REQUEST", "missing typeId".into()))?;
            Ok(SoxOp::Query {
                query_type,
                kit_id,
                type_id,
            })
        }
        "closeWatch" => Ok(SoxOp::CloseWatch {
            watch_id: str_of("watchId").unwrap_or_default(),
        }),
        unknown => Err(("UNKNOWN_OP", format!("unknown op: {unknown}"))),
    }
}
// ── Response helpers ───────────────────────────────

fn make_result(id: Option<&str>, data: JsonValue) -> Option<String> {
    let mut msg = serde_json::json!({
        "op": "result",
        "ok": true,
        "data": data,
    });
    if let Some(id) = id {
        msg["id"] = JsonValue::String(id.to_string());
    }
    serde_json::to_string(&msg).ok()
}

fn make_error(id: Option<&str>, code: &str, message: &str) -> Option<String> {
    let mut msg = serde_json::json!({
        "op": "error",
        "code": code,
        "message": message,
    });
    if let Some(id) = id {
        msg["id"] = JsonValue::String(id.to_string());
    }
    serde_json::to_string(&msg).ok()
}

fn make_pong(id: Option<&str>) -> Option<String> {
    let mut msg = serde_json::json!({ "op": "pong" });
    if let Some(id) = id {
        msg["id"] = JsonValue::String(id.to_string());
    }
    serde_json::to_string(&msg).ok()
}

// ── Name validation ────────────────────────────────

/// Validate a component name using the centralised Sedona-compatible rules.
/// Only used by tests — the live dispatcher validates via `sox_ops`.
#[cfg(test)]
fn validate_name(name: &str) -> Option<&'static str> {
    crate::sox::name_intern::NameInternTable::validate_name(name)
}

// ── Tests ──────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sox::sox_handlers::ManifestDb;
    use std::sync::Arc;

    #[test]
    fn validate_name_valid() {
        assert!(validate_name("myComp").is_none());
        assert!(validate_name("a").is_none());
        assert!(validate_name("comp_1").is_none());
        assert!(validate_name("Add2").is_none());
    }

    #[test]
    fn validate_name_empty() {
        assert_eq!(validate_name(""), Some("name cannot be empty"));
    }

    #[test]
    fn validate_name_too_long() {
        let long = "a".repeat(32);
        assert_eq!(validate_name(&long), Some("name too long (max 31 chars)"));
    }

    #[test]
    fn validate_name_starts_with_digit() {
        assert_eq!(
            validate_name("1comp"),
            Some("name must start with a letter")
        );
    }

    #[test]
    fn validate_name_special_chars() {
        assert_eq!(
            validate_name("my-comp"),
            Some("name can only contain letters, numbers, and underscores")
        );
    }

    #[test]
    fn validate_name_starts_underscore() {
        assert_eq!(
            validate_name("_comp"),
            Some("name must start with a letter")
        );
    }

    #[test]
    fn make_result_with_id() {
        let json = make_result(Some("req-1"), serde_json::json!({"ok": true})).unwrap();
        let v: JsonValue = serde_json::from_str(&json).unwrap();
        assert_eq!(v["op"], "result");
        assert_eq!(v["ok"], true);
        assert_eq!(v["id"], "req-1");
    }

    #[test]
    fn make_result_without_id() {
        let json = make_result(None, serde_json::json!({"ok": true})).unwrap();
        let v: JsonValue = serde_json::from_str(&json).unwrap();
        assert_eq!(v["op"], "result");
        assert!(v.get("id").is_none());
    }

    #[test]
    fn make_error_with_id() {
        let json = make_error(Some("req-2"), "NOT_FOUND", "comp 99 not found").unwrap();
        let v: JsonValue = serde_json::from_str(&json).unwrap();
        assert_eq!(v["op"], "error");
        assert_eq!(v["code"], "NOT_FOUND");
        assert_eq!(v["message"], "comp 99 not found");
        assert_eq!(v["id"], "req-2");
    }

    #[test]
    fn make_error_without_id() {
        let json = make_error(None, "BAD_REQUEST", "oops").unwrap();
        let v: JsonValue = serde_json::from_str(&json).unwrap();
        assert_eq!(v["op"], "error");
        assert!(v.get("id").is_none());
    }

    #[test]
    fn make_pong_with_id() {
        let json = make_pong(Some("p-1")).unwrap();
        let v: JsonValue = serde_json::from_str(&json).unwrap();
        assert_eq!(v["op"], "pong");
        assert_eq!(v["id"], "p-1");
    }

    #[test]
    fn make_pong_without_id() {
        let json = make_pong(None).unwrap();
        let v: JsonValue = serde_json::from_str(&json).unwrap();
        assert_eq!(v["op"], "pong");
        assert!(v.get("id").is_none());
    }

    #[test]
    fn handle_invalid_json() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            "not json at all",
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "error");
        assert_eq!(v["code"], "INVALID_MESSAGE");
    }

    #[test]
    fn handle_missing_op() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"id":"1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "error");
        assert_eq!(v["code"], "INVALID_MESSAGE");
    }

    #[test]
    fn handle_unknown_op() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"foobar","id":"1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "error");
        assert_eq!(v["code"], "UNKNOWN_OP");
        assert_eq!(v["id"], "1");
    }

    #[test]
    fn handle_ping() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"ping","id":"p1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "pong");
        assert_eq!(v["id"], "p1");
    }

    #[test]
    fn handle_read_tree_returns_components() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"readTree","id":"t1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "result");
        assert_eq!(v["ok"], true);
        assert!(v["data"]["components"].is_array());
    }

    #[test]
    fn handle_read_comp_not_found() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"readComp","compId":9999,"id":"c1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "error");
        assert_eq!(v["code"], "NOT_FOUND");
    }

    #[test]
    fn handle_subscribe_unsubscribe() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;

        // Subscribe
        let reply = handle_client_msg(
            r#"{"op":"subscribe","compIds":[1,2,3],"id":"s1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "result");
        assert_eq!(v["data"]["count"], 3);
        assert_eq!(subscribed.len(), 3);

        // Unsubscribe one
        let reply = handle_client_msg(
            r#"{"op":"unsubscribe","compIds":[2],"id":"u1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "result");
        assert_eq!(v["data"]["count"], 2);
        assert_eq!(subscribed.len(), 2);
        assert!(!subscribed.contains(&2));
    }

    #[test]
    fn handle_delete_system_comp_forbidden() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"deleteComp","compId":3,"id":"d1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "error");
        assert_eq!(v["code"], "FORBIDDEN");
    }

    #[test]
    fn handle_rename_bad_name() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"rename","compId":10,"name":"1bad","id":"r1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "error");
        assert_eq!(v["code"], "BAD_REQUEST");
    }

    #[test]
    fn handle_palette_returns_array() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"palette","id":"p1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "result");
        assert!(v["data"].is_array());
    }

    #[test]
    fn cov_detects_slot_change() {
        use crate::sox::sox_handlers::{
            ComponentTree, ManifestDb, SlotValue, VirtualComponent, VirtualSlot,
        };
        use crate::sox::sox_protocol::SoxValueType;

        let manifest_db = Arc::new(ManifestDb::load(""));
        let mut tree_inner = ComponentTree::new_with_manifest(manifest_db.clone());
        tree_inner.add(VirtualComponent {
            comp_id: 100,
            parent_id: 0,
            name: "test".into(),
            type_name: "Test".into(),
            kit_id: 2,
            type_id: 14,
            children: vec![],
            slots: vec![VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Float as u8,
                flags: 0x04,
                value: SlotValue::Float(1.0),
            }],
            links: vec![],
        });

        let tree_handle: SharedComponentTree = Arc::new(std::sync::RwLock::new(tree_inner));
        let mut subscribed = HashSet::new();
        subscribed.insert(100);
        let mut cov_cache = HashMap::new();
        let mut tree_cache_map = HashMap::new();

        // First call: should produce a COV (no cache)
        let msgs = build_cov_messages(
            &tree_handle,
            &subscribed,
            &mut cov_cache,
            &mut tree_cache_map,
            false,
        );
        assert_eq!(msgs.len(), 1);
        let v: JsonValue = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(v["op"], "cov");
        assert_eq!(v["compId"], 100);

        // Second call: no change, no COV
        let msgs = build_cov_messages(
            &tree_handle,
            &subscribed,
            &mut cov_cache,
            &mut tree_cache_map,
            false,
        );
        assert!(msgs.is_empty());

        // Mutate value
        {
            let mut t = tree_handle.write().unwrap();
            if let Some(comp) = t.get_mut(100) {
                comp.slots[0].value = SlotValue::Float(2.0);
            }
        }

        // Third call: should detect change
        let msgs = build_cov_messages(
            &tree_handle,
            &subscribed,
            &mut cov_cache,
            &mut tree_cache_map,
            false,
        );
        assert_eq!(msgs.len(), 1);
        let v: JsonValue = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(v["op"], "cov");
        assert_eq!(v["slots"][0]["index"], 0);
    }

    #[test]
    fn cov_detects_tree_change() {
        use crate::sox::sox_handlers::{
            ComponentTree, ManifestDb, SlotValue, VirtualComponent, VirtualSlot,
        };
        use crate::sox::sox_protocol::SoxValueType;

        let manifest_db = Arc::new(ManifestDb::load(""));
        let mut tree_inner = ComponentTree::new_with_manifest(manifest_db.clone());
        tree_inner.add(VirtualComponent {
            comp_id: 50,
            parent_id: 0,
            name: "parent".into(),
            type_name: "Folder".into(),
            kit_id: 0,
            type_id: 11,
            children: vec![],
            slots: vec![VirtualSlot {
                name: "meta".into(),
                type_id: SoxValueType::Int as u8,
                flags: 0x01,
                value: SlotValue::Int(1),
            }],
            links: vec![],
        });

        let tree_handle: SharedComponentTree = Arc::new(std::sync::RwLock::new(tree_inner));
        let mut subscribed = HashSet::new();
        subscribed.insert(50);
        let mut cov_cache = HashMap::new();
        let mut tree_cache_map = HashMap::new();

        // Prime cache
        let _ = build_cov_messages(
            &tree_handle,
            &subscribed,
            &mut cov_cache,
            &mut tree_cache_map,
            false,
        );

        // Add a child
        {
            let mut t = tree_handle.write().unwrap();
            if let Some(comp) = t.get_mut(50) {
                comp.children.push(100);
            }
        }

        let msgs = build_cov_messages(
            &tree_handle,
            &subscribed,
            &mut cov_cache,
            &mut tree_cache_map,
            false,
        );
        // Should have a treeChanged message
        let tree_msgs: Vec<&String> = msgs.iter().filter(|m| m.contains("treeChanged")).collect();
        assert_eq!(tree_msgs.len(), 1);
        let v: JsonValue = serde_json::from_str(tree_msgs[0]).unwrap();
        assert_eq!(v["action"], "childrenChanged");
        assert_eq!(v["compId"], 50);
    }

    #[test]
    fn handle_name_table_returns_names_and_enables_compact() {
        use crate::sox::sox_handlers::{
            ComponentTree, ManifestDb, SlotValue, VirtualComponent, VirtualSlot,
        };
        use crate::sox::sox_protocol::SoxValueType;

        let manifest_db = Arc::new(ManifestDb::load(""));
        let mut tree_inner = ComponentTree::new_with_manifest(manifest_db.clone());
        // Add a component so its name gets interned
        tree_inner.add(VirtualComponent {
            comp_id: 100,
            parent_id: 0,
            name: "myComp".into(),
            type_name: "Test".into(),
            kit_id: 2,
            type_id: 14,
            children: vec![],
            slots: vec![VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Float as u8,
                flags: 0x04,
                value: SlotValue::Float(1.0),
            }],
            links: vec![],
        });

        let tree_handle: SharedComponentTree = Arc::new(std::sync::RwLock::new(tree_inner));
        let state = RowsState {
            tree: tree_handle,
            manifest_db,
            dyn_store: None,
        };
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;

        let reply = handle_client_msg(
            r#"{"op":"nameTable","id":"n1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "result");
        assert_eq!(v["ok"], true);
        assert_eq!(v["id"], "n1");

        // The name table should contain "myComp"
        let names = v["data"]["names"].as_array().unwrap();
        assert!(!names.is_empty());
        let has_my_comp = names.iter().any(|n| n["name"] == "myComp");
        assert!(has_my_comp, "name table should contain 'myComp'");

        // compact_cov should now be enabled
        assert!(
            compact_cov,
            "compact_cov should be true after nameTable request"
        );
    }

    #[test]
    fn compact_cov_uses_short_keys() {
        use crate::sox::sox_handlers::{
            ComponentTree, ManifestDb, SlotValue, VirtualComponent, VirtualSlot,
        };
        use crate::sox::sox_protocol::SoxValueType;

        let manifest_db = Arc::new(ManifestDb::load(""));
        let mut tree_inner = ComponentTree::new_with_manifest(manifest_db.clone());
        tree_inner.add(VirtualComponent {
            comp_id: 200,
            parent_id: 0,
            name: "test".into(),
            type_name: "Test".into(),
            kit_id: 2,
            type_id: 14,
            children: vec![],
            slots: vec![VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Float as u8,
                flags: 0x04,
                value: SlotValue::Float(42.0),
            }],
            links: vec![],
        });

        let tree_handle: SharedComponentTree = Arc::new(std::sync::RwLock::new(tree_inner));
        let mut subscribed = HashSet::new();
        subscribed.insert(200);
        let mut cov_cache = HashMap::new();
        let mut tree_cache_map = HashMap::new();

        // First call with compact=false to prime cache
        let _ = build_cov_messages(
            &tree_handle,
            &subscribed,
            &mut cov_cache,
            &mut tree_cache_map,
            false,
        );

        // Mutate value
        {
            let mut t = tree_handle.write().unwrap();
            if let Some(comp) = t.get_mut(200) {
                comp.slots[0].value = SlotValue::Float(99.0);
            }
        }

        // Compact COV should use "i" and "v" keys
        let msgs = build_cov_messages(
            &tree_handle,
            &subscribed,
            &mut cov_cache,
            &mut tree_cache_map,
            true,
        );
        assert_eq!(msgs.len(), 1);
        let v: JsonValue = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(v["op"], "cov");
        let slot = &v["slots"][0];
        // Compact format uses "i" and "v" instead of "index", "name", "value"
        assert!(slot.get("i").is_some(), "compact COV should have 'i' key");
        assert!(slot.get("v").is_some(), "compact COV should have 'v' key");
        assert!(
            slot.get("name").is_none(),
            "compact COV should not have 'name'"
        );
        assert!(
            slot.get("index").is_none(),
            "compact COV should not have 'index'"
        );
    }

    // ── Dynamic tag tests ──────────────────────

    fn make_test_state_with_dyn_store() -> (RowsState, DynSlotStoreHandle) {
        use crate::sox::sox_handlers::{ComponentTree, ManifestDb};
        let manifest_db = Arc::new(ManifestDb::load(""));
        let tree = Arc::new(std::sync::RwLock::new(ComponentTree::new_with_manifest(
            manifest_db.clone(),
        )));
        let dyn_store: DynSlotStoreHandle = Arc::new(std::sync::RwLock::new(
            crate::sox::dyn_slots::DynSlotStore::with_defaults(),
        ));
        let state = RowsState {
            tree,
            manifest_db,
            dyn_store: Some(dyn_store.clone()),
        };
        (state, dyn_store)
    }

    #[test]
    fn rows_read_comp_includes_tags_when_present() {
        use crate::sox::dyn_slots::DynValue;
        use crate::sox::sox_handlers::{
            ComponentTree, ManifestDb, SlotValue, VirtualComponent, VirtualSlot,
        };
        use crate::sox::sox_protocol::SoxValueType;

        let manifest_db = Arc::new(ManifestDb::load(""));
        let mut tree_inner = ComponentTree::new_with_manifest(manifest_db.clone());
        tree_inner.add(VirtualComponent {
            comp_id: 100,
            parent_id: 0,
            name: "test".into(),
            type_name: "Test".into(),
            kit_id: 2,
            type_id: 14,
            children: vec![],
            slots: vec![VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Float as u8,
                flags: 0x04,
                value: SlotValue::Float(1.0),
            }],
            links: vec![],
        });

        let tree = Arc::new(std::sync::RwLock::new(tree_inner));
        let dyn_store: DynSlotStoreHandle = Arc::new(std::sync::RwLock::new(
            crate::sox::dyn_slots::DynSlotStore::with_defaults(),
        ));

        // Set a dynamic tag
        {
            let mut store = dyn_store.write().unwrap();
            store
                .set(100, "modbusAddr".into(), DynValue::Int(40001))
                .unwrap();
        }

        let state = RowsState {
            tree,
            manifest_db,
            dyn_store: Some(dyn_store),
        };

        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"readComp","compId":100,"id":"rc1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "result");
        assert_eq!(v["ok"], true);
        // Tags should be present in the response
        assert!(
            v["data"]["tags"].is_object(),
            "readComp should include tags"
        );
        assert_eq!(v["data"]["tags"]["modbusAddr"]["val"], 40001);
    }

    #[test]
    fn rows_read_comp_no_tags_field_when_empty() {
        use crate::sox::sox_handlers::{
            ComponentTree, ManifestDb, SlotValue, VirtualComponent, VirtualSlot,
        };
        use crate::sox::sox_protocol::SoxValueType;

        let manifest_db = Arc::new(ManifestDb::load(""));
        let mut tree_inner = ComponentTree::new_with_manifest(manifest_db.clone());
        tree_inner.add(VirtualComponent {
            comp_id: 50,
            parent_id: 0,
            name: "noTags".into(),
            type_name: "Test".into(),
            kit_id: 2,
            type_id: 14,
            children: vec![],
            slots: vec![VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Float as u8,
                flags: 0x04,
                value: SlotValue::Float(1.0),
            }],
            links: vec![],
        });
        let tree = Arc::new(std::sync::RwLock::new(tree_inner));
        let dyn_store: DynSlotStoreHandle = Arc::new(std::sync::RwLock::new(
            crate::sox::dyn_slots::DynSlotStore::with_defaults(),
        ));
        let state = RowsState {
            tree,
            manifest_db,
            dyn_store: Some(dyn_store),
        };
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        // Comp 50 exists but has no dynamic tags
        let reply = handle_client_msg(
            r#"{"op":"readComp","compId":50,"id":"rc2"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "result");
        // Tags field should NOT be present when there are no dynamic tags
        assert!(
            v["data"]["tags"].is_null(),
            "readComp should not include tags when empty"
        );
    }

    #[test]
    fn rows_read_tags_command() {
        let (state, dyn_store) = make_test_state_with_dyn_store();
        {
            let mut store = dyn_store.write().unwrap();
            store
                .set(
                    0,
                    "testTag".into(),
                    crate::sox::dyn_slots::DynValue::Str("hello".into()),
                )
                .unwrap();
        }
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"readTags","compId":0,"id":"rt1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "result");
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["tags"]["testTag"]["val"], "hello");
    }

    #[test]
    fn rows_read_tags_empty() {
        let (state, _) = make_test_state_with_dyn_store();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"readTags","compId":999,"id":"rt2"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "result");
        assert_eq!(v["data"]["tags"].as_object().unwrap().len(), 0);
    }

    #[test]
    fn rows_set_tags_command() {
        let (state, dyn_store) = make_test_state_with_dyn_store();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"setTags","compId":50,"tags":{"addr":40001,"dis":"Zone Temp"},"id":"st1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "result");
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["ok"], true);

        // Verify tags were stored
        let store = dyn_store.read().unwrap();
        assert_eq!(store.tag_count(50), 2);
    }

    #[test]
    fn rows_delete_tag_command() {
        let (state, dyn_store) = make_test_state_with_dyn_store();
        {
            let mut store = dyn_store.write().unwrap();
            store
                .set(50, "foo".into(), crate::sox::dyn_slots::DynValue::Marker)
                .unwrap();
        }
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"deleteTag","compId":50,"key":"foo","id":"dt1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "result");
        assert_eq!(v["data"]["removed"], true);

        let store = dyn_store.read().unwrap();
        assert_eq!(store.tag_count(50), 0);
    }

    #[test]
    fn rows_delete_tag_nonexistent() {
        let (state, _) = make_test_state_with_dyn_store();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"deleteTag","compId":50,"key":"nope","id":"dt2"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "result");
        assert_eq!(v["data"]["removed"], false);
    }

    #[test]
    fn rows_tags_changed_cov_push() {
        use crate::sox::dyn_slots::DynValue;
        use crate::sox::sox_handlers::{
            ComponentTree, ManifestDb, SlotValue, VirtualComponent, VirtualSlot,
        };
        use crate::sox::sox_protocol::SoxValueType;

        let manifest_db = Arc::new(ManifestDb::load(""));
        let mut tree_inner = ComponentTree::new_with_manifest(manifest_db.clone());
        tree_inner.add(VirtualComponent {
            comp_id: 200,
            parent_id: 0,
            name: "test".into(),
            type_name: "Test".into(),
            kit_id: 2,
            type_id: 14,
            children: vec![],
            slots: vec![VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Float as u8,
                flags: 0x04,
                value: SlotValue::Float(1.0),
            }],
            links: vec![],
        });

        let tree: SharedComponentTree = Arc::new(std::sync::RwLock::new(tree_inner));
        let dyn_store: DynSlotStoreHandle = Arc::new(std::sync::RwLock::new(
            crate::sox::dyn_slots::DynSlotStore::with_defaults(),
        ));

        let mut subscribed = HashSet::new();
        subscribed.insert(200);
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut tags_cache = HashMap::new();

        // First tick: prime caches (no tags yet)
        let msgs = build_cov_messages_with_tags(
            &tree,
            &subscribed,
            &mut cov_cache,
            &mut tree_cache,
            false,
            Some(&dyn_store),
            Some(&mut tags_cache),
        );
        // Should get an initial COV for the slots (first time)
        assert!(!msgs.is_empty());
        // But no tagsChanged since there are no tags
        assert!(
            !msgs.iter().any(|m| m.contains("tagsChanged")),
            "no tagsChanged expected when there are no tags"
        );

        // Now add a dynamic tag
        {
            let mut store = dyn_store.write().unwrap();
            store
                .set(200, "devEUI".into(), DynValue::Str("A81758".into()))
                .unwrap();
        }

        // Second tick: should detect the tag change
        let msgs = build_cov_messages_with_tags(
            &tree,
            &subscribed,
            &mut cov_cache,
            &mut tree_cache,
            false,
            Some(&dyn_store),
            Some(&mut tags_cache),
        );
        let has_tags_changed = msgs.iter().any(|m| {
            let v: JsonValue = serde_json::from_str(m).unwrap();
            v["op"] == "tagsChanged" && v["compId"] == 200
        });
        assert!(has_tags_changed, "should emit tagsChanged event");

        // Third tick: no change, no event
        let msgs = build_cov_messages_with_tags(
            &tree,
            &subscribed,
            &mut cov_cache,
            &mut tree_cache,
            false,
            Some(&dyn_store),
            Some(&mut tags_cache),
        );
        assert!(
            !msgs.iter().any(|m| m.contains("tagsChanged")),
            "no tagsChanged when tags haven't changed"
        );
    }

    #[test]
    fn rows_set_tags_without_dyn_store_returns_error() {
        let state = make_test_state(); // no dyn_store
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"setTags","compId":50,"tags":{"a":1},"id":"e1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "error");
        assert_eq!(v["code"], "NOT_SUPPORTED");
    }

    // ── G-2b editor-parity op tests ────────────

    #[test]
    fn handle_version_returns_kit_list() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"version","id":"v1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "result");
        assert_eq!(v["id"], "v1");
        assert!(v["data"]["kits"].is_array());
        assert!(v["data"]["kitCount"].as_u64().unwrap() > 0);
        // Each kit entry must carry name + checksum.
        let first_kit = &v["data"]["kits"][0];
        assert!(first_kit["name"].is_string());
        assert!(first_kit["checksum"].is_number());
    }

    #[test]
    fn handle_version_ext_returns_extended_fields() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"versionExt","id":"y1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "result");
        // Matches Fantom pod placeholders: empty platformId, zero scodeFlags, empty pairs.
        assert_eq!(v["data"]["platformId"], "");
        assert_eq!(v["data"]["scodeFlags"], 0);
        assert!(v["data"]["kitVersions"].is_array());
        assert_eq!(v["data"]["pairs"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn handle_reorder_missing_comp_id_errors() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"reorder","children":[1,2]}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "error");
        assert_eq!(v["code"], "BAD_REQUEST");
    }

    #[test]
    fn handle_reorder_unknown_comp_not_found() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"reorder","compId":9999,"children":[]}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "error");
        assert_eq!(v["code"], "NOT_FOUND");
    }

    #[test]
    fn handle_query_rejects_unknown_type() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"query","queryType":"z","kitId":0,"typeId":0}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "error");
        assert_eq!(v["code"], "BAD_REQUEST");
        assert!(v["message"].as_str().unwrap().contains("unknown query type"));
    }

    #[test]
    fn handle_query_type_not_found() {
        let state = make_test_state();
        let mut subscribed = HashSet::new();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"query","queryType":"s","kitId":255,"typeId":255}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "error");
        assert_eq!(v["code"], "NOT_FOUND");
    }

    #[test]
    fn handle_close_watch_clears_subscriptions() {
        let state = make_test_state();
        let mut subscribed: HashSet<u16> = [7u16, 8, 9].into_iter().collect();
        let mut cov_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut compact_cov = false;
        let reply = handle_client_msg(
            r#"{"op":"closeWatch","watchId":"w1","id":"x1"}"#,
            &state,
            &mut subscribed,
            &mut cov_cache,
            &mut tree_cache,
            &mut compact_cov,
        );
        let v: JsonValue = serde_json::from_str(reply.as_deref().unwrap()).unwrap();
        assert_eq!(v["op"], "result");
        assert_eq!(v["data"]["watchId"], "w1");
        assert_eq!(v["data"]["unsubscribed"], 3);
        assert!(subscribed.is_empty());
    }

    // ── Test helpers ───────────────────────────

    fn make_test_state() -> RowsState {
        use crate::sox::sox_handlers::{ComponentTree, ManifestDb};
        let manifest_db = Arc::new(ManifestDb::load(""));
        let tree = Arc::new(std::sync::RwLock::new(ComponentTree::new_with_manifest(
            manifest_db.clone(),
        )));
        RowsState {
            tree,
            manifest_db,
            dyn_store: None,
        }
    }
}
