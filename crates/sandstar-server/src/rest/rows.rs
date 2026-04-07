//! RoWS (ROX over WebSocket): real-time bidirectional component tree
//! operations and COV (Change of Value) push events.
//!
//! Clients connect to `GET /api/rows`, send JSON commands to read/mutate the
//! virtual component tree, and receive server-pushed COV updates for
//! subscribed components. This is the WebSocket equivalent of the SOX REST
//! API (`/api/sox/*`) combined with live COV streaming.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value as JsonValue;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info};

use crate::sox::dyn_slots::DynValue;
use crate::sox::sox_handlers::{
    ComponentTree, ManifestDb, SlotValue, VirtualComponent, VirtualSlot,
    DEFAULT_KITS, SLOT_FLAG_ACTION, SLOT_FLAG_CONFIG,
};
use crate::sox::sox_protocol::SoxValueType;
use crate::sox::{DynSlotStoreHandle, SharedComponentTree};

use super::sox_api::{
    category_for_type, decode_position, encode_position, is_system_comp,
    json_to_slot_value, slot_direction, slot_value_to_json, sox_type_name,
};

// ── Constants ──────────────────────────────────────

const MAX_ROWS_CONNECTIONS: i64 = 16;
const COV_INTERVAL_MS: u64 = 1000;
const CLIENT_TIMEOUT_SECS: u64 = 120;
// ── State ──────────────────────────────────────────

#[derive(Clone)]
pub struct RowsState {
    pub tree: SharedComponentTree,
    pub manifest_db: Arc<ManifestDb>,
    pub dyn_store: Option<DynSlotStoreHandle>,
}

// ── Upgrade handler ────────────────────────────────

pub async fn rows_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<RowsState>,
) -> Response {
    let current = crate::metrics::metrics()
        .rows_active
        .load(Ordering::Relaxed);
    if current >= MAX_ROWS_CONNECTIONS {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many RoWS connections",
        )
            .into_response();
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
    build_cov_messages_with_tags(tree_handle, subscribed, cov_cache, tree_cache, compact_cov, None, None)
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
        let current_slots: Vec<JsonValue> = comp.slots.iter()
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
                let is_new = !cached
                    .and_then(|c| c.get(idx))
                    .is_some_and(|cv| cv == val);
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
                    .unwrap_or(!current_tags.as_object().map(|o| o.is_empty()).unwrap_or(true));
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
    let op = match msg.get("op").and_then(|v| v.as_str()) {
        Some(o) => o,
        None => {
            return make_error(id.as_deref(), "INVALID_MESSAGE", "missing 'op' field");
        }
    };

    match op {
        "ping" => make_pong(id.as_deref()),
        "readTree" => handle_read_tree(state, id.as_deref()),
        "readComp" => {
            let comp_id = msg.get("compId").and_then(|v| v.as_u64()).map(|v| v as u16);
            match comp_id {
                Some(cid) => handle_read_comp(state, cid, id.as_deref()),
                None => make_error(id.as_deref(), "BAD_REQUEST", "missing or invalid 'compId'"),
            }
        }
        "writeSlot" => {
            let comp_id = msg.get("compId").and_then(|v| v.as_u64()).map(|v| v as u16);
            let slot_idx = msg.get("slotIdx").and_then(|v| v.as_u64()).map(|v| v as usize);
            let value = msg.get("value");
            match (comp_id, slot_idx, value) {
                (Some(cid), Some(idx), Some(val)) => handle_write_slot(state, cid, idx, val, id.as_deref()),
                _ => make_error(id.as_deref(), "BAD_REQUEST", "missing compId, slotIdx, or value"),
            }
        }
        "addComp" => {
            let parent_id = msg.get("parentId").and_then(|v| v.as_u64()).map(|v| v as u16);
            let kit_id = msg.get("kitId").and_then(|v| v.as_u64()).map(|v| v as u8);
            let type_id = msg.get("typeId").and_then(|v| v.as_u64()).map(|v| v as u8);
            let name = msg.get("name").and_then(|v| v.as_str());
            match (parent_id, kit_id, type_id, name) {
                (Some(pid), Some(kid), Some(tid), Some(n)) => {
                    handle_add_comp(state, pid, kid, tid, n, id.as_deref())
                }
                _ => make_error(id.as_deref(), "BAD_REQUEST", "missing parentId, kitId, typeId, or name"),
            }
        }
        "deleteComp" => {
            let comp_id = msg.get("compId").and_then(|v| v.as_u64()).map(|v| v as u16);
            match comp_id {
                Some(cid) => handle_delete_comp(state, cid, id.as_deref()),
                None => make_error(id.as_deref(), "BAD_REQUEST", "missing or invalid 'compId'"),
            }
        }
        "rename" => {
            let comp_id = msg.get("compId").and_then(|v| v.as_u64()).map(|v| v as u16);
            let name = msg.get("name").and_then(|v| v.as_str());
            match (comp_id, name) {
                (Some(cid), Some(n)) => handle_rename(state, cid, n, id.as_deref()),
                _ => make_error(id.as_deref(), "BAD_REQUEST", "missing compId or name"),
            }
        }
        "addLink" => {
            let fc = msg.get("fromComp").and_then(|v| v.as_u64()).map(|v| v as u16);
            let fs = msg.get("fromSlot").and_then(|v| v.as_u64()).map(|v| v as u8);
            let tc = msg.get("toComp").and_then(|v| v.as_u64()).map(|v| v as u16);
            let ts = msg.get("toSlot").and_then(|v| v.as_u64()).map(|v| v as u8);
            match (fc, fs, tc, ts) {
                (Some(fc), Some(fs), Some(tc), Some(ts)) => {
                    handle_add_link(state, fc, fs, tc, ts, id.as_deref())
                }
                _ => make_error(id.as_deref(), "BAD_REQUEST", "missing fromComp/fromSlot/toComp/toSlot"),
            }
        }
        "deleteLink" => {
            let fc = msg.get("fromComp").and_then(|v| v.as_u64()).map(|v| v as u16);
            let fs = msg.get("fromSlot").and_then(|v| v.as_u64()).map(|v| v as u8);
            let tc = msg.get("toComp").and_then(|v| v.as_u64()).map(|v| v as u16);
            let ts = msg.get("toSlot").and_then(|v| v.as_u64()).map(|v| v as u8);
            match (fc, fs, tc, ts) {
                (Some(fc), Some(fs), Some(tc), Some(ts)) => {
                    handle_delete_link(state, fc, fs, tc, ts, id.as_deref())
                }
                _ => make_error(id.as_deref(), "BAD_REQUEST", "missing fromComp/fromSlot/toComp/toSlot"),
            }
        }
        "updatePos" => {
            let comp_id = msg.get("compId").and_then(|v| v.as_u64()).map(|v| v as u16);
            let x = msg.get("x").and_then(|v| v.as_u64()).map(|v| v as u8);
            let y = msg.get("y").and_then(|v| v.as_u64()).map(|v| v as u8);
            match (comp_id, x, y) {
                (Some(cid), Some(x), Some(y)) => handle_update_pos(state, cid, x, y, id.as_deref()),
                _ => make_error(id.as_deref(), "BAD_REQUEST", "missing compId, x, or y"),
            }
        }
        "invoke" => {
            let comp_id = msg.get("compId").and_then(|v| v.as_u64()).map(|v| v as u16);
            let slot_idx = msg.get("slotIdx").and_then(|v| v.as_u64()).map(|v| v as usize);
            match (comp_id, slot_idx) {
                (Some(cid), Some(idx)) => handle_invoke(state, cid, idx, id.as_deref()),
                _ => make_error(id.as_deref(), "BAD_REQUEST", "missing compId or slotIdx"),
            }
        }
        "nameTable" => handle_name_table(state, id.as_deref(), compact_cov),
        "palette" => handle_palette(state, id.as_deref()),
        "subscribe" => {
            let comp_ids = msg.get("compIds")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u16))
                        .collect::<Vec<u16>>()
                })
                .unwrap_or_default();
            for cid in &comp_ids {
                subscribed.insert(*cid);
            }
            // Prime the cache for newly subscribed components
            {
                let tree = state.tree.read().unwrap();
                for &cid in &comp_ids {
                    if let Some(comp) = tree.get(cid) {
                        let vals: Vec<JsonValue> = comp.slots.iter()
                            .map(|s| slot_value_to_json(&s.value))
                            .collect();
                        cov_cache.insert(cid, vals);
                        tree_cache.insert(cid, comp.children.clone());
                    }
                }
            }
            make_result(id.as_deref(), serde_json::json!({
                "subscribed": comp_ids,
                "count": subscribed.len(),
            }))
        }
        "unsubscribe" => {
            let comp_ids = msg.get("compIds")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u16))
                        .collect::<Vec<u16>>()
                })
                .unwrap_or_default();
            for cid in &comp_ids {
                subscribed.remove(cid);
                cov_cache.remove(cid);
                tree_cache.remove(cid);
            }
            make_result(id.as_deref(), serde_json::json!({
                "unsubscribed": comp_ids,
                "count": subscribed.len(),
            }))
        }
        "readTags" => {
            let comp_id = msg.get("compId").and_then(|v| v.as_u64()).map(|v| v as u16);
            match comp_id {
                Some(cid) => handle_read_tags(state, cid, id.as_deref()),
                None => make_error(id.as_deref(), "BAD_REQUEST", "missing or invalid 'compId'"),
            }
        }
        "setTags" => {
            let comp_id = msg.get("compId").and_then(|v| v.as_u64()).map(|v| v as u16);
            let tags = msg.get("tags");
            match (comp_id, tags) {
                (Some(cid), Some(t)) => handle_set_tags(state, cid, t, id.as_deref()),
                _ => make_error(id.as_deref(), "BAD_REQUEST", "missing compId or tags"),
            }
        }
        "deleteTag" => {
            let comp_id = msg.get("compId").and_then(|v| v.as_u64()).map(|v| v as u16);
            let key = msg.get("key").and_then(|v| v.as_str());
            match (comp_id, key) {
                (Some(cid), Some(k)) => handle_delete_tag(state, cid, k, id.as_deref()),
                _ => make_error(id.as_deref(), "BAD_REQUEST", "missing compId or key"),
            }
        }
        "setFormat" => {
            // setFormat allows the client to request Trio text encoding for responses.
            // Currently accepted for forward-compatibility but JSON remains the default.
            let format = msg.get("format").and_then(|v| v.as_str()).unwrap_or("json");
            match format {
                "json" | "trio" => {
                    make_result(id.as_deref(), serde_json::json!({
                        "format": format,
                        "status": "ok",
                    }))
                }
                _ => make_error(id.as_deref(), "BAD_REQUEST", &format!("unsupported format: {format}")),
            }
        }
        _ => make_error(id.as_deref(), "UNKNOWN_OP", &format!("unknown op: {op}")),
    }
}

// ── Command handlers ───────────────────────────────

fn handle_read_tree(state: &RowsState, id: Option<&str>) -> Option<String> {
    let tree = state.tree.read().unwrap();
    let mut comps: Vec<JsonValue> = tree
        .comp_ids()
        .into_iter()
        .filter_map(|cid| tree.get(cid).map(|c| comp_to_tree_json(c, &tree)))
        .collect();
    comps.sort_by(|a, b| {
        let a_id = a.get("compId").and_then(|v| v.as_u64()).unwrap_or(0);
        let b_id = b.get("compId").and_then(|v| v.as_u64()).unwrap_or(0);
        a_id.cmp(&b_id)
    });
    make_result(id, serde_json::json!({ "components": comps }))
}

fn handle_read_comp(state: &RowsState, comp_id: u16, id: Option<&str>) -> Option<String> {
    let tree = state.tree.read().unwrap();
    match tree.get(comp_id) {
        Some(comp) => {
            let mut detail = comp_to_detail_json(comp, &tree);
            // Append dynamic tags if any exist for this component.
            if let Some(ref dyn_store) = state.dyn_store {
                if let Ok(store) = dyn_store.read() {
                    if let Some(tags) = store.get_all(comp_id) {
                        if !tags.is_empty() {
                            detail["tags"] = serde_json::to_value(tags).unwrap_or_default();
                        }
                    }
                }
            }
            make_result(id, detail)
        }
        None => make_error(id, "NOT_FOUND", &format!("component {comp_id} not found")),
    }
}

fn handle_write_slot(
    state: &RowsState,
    comp_id: u16,
    slot_idx: usize,
    value: &JsonValue,
    id: Option<&str>,
) -> Option<String> {
    let mut tree = state.tree.write().unwrap();
    let slot_type_id = {
        let comp = match tree.get(comp_id) {
            Some(c) => c,
            None => return make_error(id, "NOT_FOUND", &format!("component {comp_id} not found")),
        };
        match comp.slots.get(slot_idx) {
            Some(slot) => slot.type_id,
            None => return make_error(id, "BAD_REQUEST", &format!("slot index {slot_idx} out of range")),
        }
    };
    let new_value = match json_to_slot_value(value, slot_type_id) {
        Some(v) => v,
        None => return make_error(id, "BAD_REQUEST", &format!("cannot coerce value to type_id {slot_type_id}")),
    };
    if let Some(comp) = tree.get_mut(comp_id) {
        if let Some(slot) = comp.slots.get_mut(slot_idx) {
            slot.value = new_value;
        }
    }
    tree.mark_dirty();
    make_result(id, serde_json::json!({ "ok": true }))
}

fn handle_add_comp(
    state: &RowsState,
    parent_id: u16,
    kit_id: u8,
    type_id: u8,
    name: &str,
    id: Option<&str>,
) -> Option<String> {
    // Validate name
    if let Some(err) = validate_name(name) {
        return make_error(id, "BAD_REQUEST", err);
    }

    let mut tree = state.tree.write().unwrap();

    if tree.get(parent_id).is_none() {
        return make_error(id, "NOT_FOUND", &format!("parent {parent_id} not found"));
    }

    let slots = match state.manifest_db.get_slots(kit_id, type_id) {
        Some(manifest_slots) => ManifestDb::slots_to_virtual(manifest_slots),
        None => {
            vec![VirtualSlot {
                name: "meta".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(1),
            }]
        }
    };

    let type_name = state
        .manifest_db
        .type_name(kit_id, type_id)
        .unwrap_or_else(|| format!("Type_{}_{}", kit_id, type_id));

    let comp_id = tree.next_comp_id();
    let comp = VirtualComponent {
        comp_id,
        parent_id,
        name: name.to_string(),
        type_name,
        kit_id,
        type_id,
        children: Vec::new(),
        slots,
        links: Vec::new(),
    };

    tree.add(comp);
    tree.mark_user_added(comp_id);
    tree.mark_dirty();

    make_result(id, serde_json::json!({ "compId": comp_id }))
}

fn handle_delete_comp(state: &RowsState, comp_id: u16, id: Option<&str>) -> Option<String> {
    if comp_id < 7 {
        return make_error(id, "FORBIDDEN", "cannot delete system components (id < 7)");
    }

    let mut tree = state.tree.write().unwrap();

    // Remove all links involving this component
    let link_peers: Vec<(u16, u8, u16, u8)> = tree
        .get(comp_id)
        .map(|c| {
            c.links
                .iter()
                .map(|l| (l.from_comp, l.from_slot, l.to_comp, l.to_slot))
                .collect()
        })
        .unwrap_or_default();
    for (fc, fs, tc, ts) in &link_peers {
        tree.remove_link(*fc, *fs, *tc, *ts);
    }

    match tree.remove(comp_id) {
        Some(_) => {
            tree.mark_dirty();
            make_result(id, serde_json::json!({ "ok": true }))
        }
        None => make_error(id, "NOT_FOUND", &format!("component {comp_id} not found")),
    }
}

fn handle_rename(state: &RowsState, comp_id: u16, name: &str, id: Option<&str>) -> Option<String> {
    if let Some(err) = validate_name(name) {
        return make_error(id, "BAD_REQUEST", err);
    }

    let mut tree = state.tree.write().unwrap();
    if tree.rename(comp_id, name.to_string()) {
        tree.mark_dirty();
        make_result(id, serde_json::json!({ "ok": true }))
    } else {
        make_error(id, "NOT_FOUND", &format!("component {comp_id} not found"))
    }
}

fn handle_add_link(
    state: &RowsState,
    from_comp: u16,
    from_slot: u8,
    to_comp: u16,
    to_slot: u8,
    id: Option<&str>,
) -> Option<String> {
    let mut tree = state.tree.write().unwrap();
    if tree.add_link(from_comp, from_slot, to_comp, to_slot) {
        tree.mark_dirty();
        make_result(id, serde_json::json!({ "ok": true }))
    } else {
        make_error(id, "CONFLICT", "link already exists or would create a cycle")
    }
}

fn handle_delete_link(
    state: &RowsState,
    from_comp: u16,
    from_slot: u8,
    to_comp: u16,
    to_slot: u8,
    id: Option<&str>,
) -> Option<String> {
    let mut tree = state.tree.write().unwrap();
    if tree.remove_link(from_comp, from_slot, to_comp, to_slot) {
        tree.mark_dirty();
        make_result(id, serde_json::json!({ "ok": true }))
    } else {
        make_error(id, "NOT_FOUND", "link not found")
    }
}

fn handle_update_pos(
    state: &RowsState,
    comp_id: u16,
    x: u8,
    y: u8,
    id: Option<&str>,
) -> Option<String> {
    let mut tree = state.tree.write().unwrap();
    let comp = match tree.get_mut(comp_id) {
        Some(c) => c,
        None => return make_error(id, "NOT_FOUND", &format!("component {comp_id} not found")),
    };
    if comp.slots.is_empty() {
        return make_error(id, "BAD_REQUEST", "component has no meta slot");
    }
    comp.slots[0].value = encode_position(x, y);
    tree.mark_dirty();
    make_result(id, serde_json::json!({ "ok": true }))
}

fn handle_invoke(
    state: &RowsState,
    comp_id: u16,
    slot_idx: usize,
    id: Option<&str>,
) -> Option<String> {
    let mut tree = state.tree.write().unwrap();
    let comp = match tree.get_mut(comp_id) {
        Some(c) => c,
        None => return make_error(id, "NOT_FOUND", &format!("component {comp_id} not found")),
    };
    let slot = match comp.slots.get(slot_idx) {
        Some(s) => s,
        None => return make_error(id, "BAD_REQUEST", &format!("slot index {slot_idx} out of range")),
    };
    if slot.flags & SLOT_FLAG_ACTION == 0 {
        return make_error(id, "BAD_REQUEST", "slot is not an action slot");
    }
    // Execute the action based on its name
    let slot_name = slot.name.clone();
    match slot_name.as_str() {
        "setTrue" => {
            for slot in &mut comp.slots {
                if slot.type_id == SoxValueType::Bool as u8 && slot.flags & SLOT_FLAG_ACTION == 0 {
                    slot.value = SlotValue::Bool(true);
                    break;
                }
            }
        }
        "setFalse" => {
            for slot in &mut comp.slots {
                if slot.type_id == SoxValueType::Bool as u8 && slot.flags & SLOT_FLAG_ACTION == 0 {
                    slot.value = SlotValue::Bool(false);
                    break;
                }
            }
        }
        _ => {
            // Generic invoke — no-op for now (side effects happen server-side)
        }
    }
    tree.mark_dirty();
    make_result(id, serde_json::json!({ "ok": true }))
}

fn handle_name_table(
    state: &RowsState,
    id: Option<&str>,
    compact_cov: &mut bool,
) -> Option<String> {
    let tree = state.tree.read().unwrap();
    let names: Vec<JsonValue> = tree.name_table
        .all_names()
        .into_iter()
        .map(|(name_id, name)| {
            serde_json::json!({
                "id": name_id.0,
                "name": name,
            })
        })
        .collect();
    // Enable compact COV mode for this session — the client now has the name table.
    *compact_cov = true;
    make_result(id, serde_json::json!({ "names": names }))
}

fn handle_palette(state: &RowsState, id: Option<&str>) -> Option<String> {
    let mut entries: Vec<JsonValue> = state
        .manifest_db
        .all_types()
        .map(|(&(kit_id, type_id), slots)| {
            let kit_name = DEFAULT_KITS
                .get(kit_id as usize)
                .map(|k| k.name.to_string())
                .unwrap_or_else(|| format!("kit{}", kit_id));
            let type_name = state
                .manifest_db
                .type_name(kit_id, type_id)
                .unwrap_or_else(|| format!("Type{}", type_id));
            let category = category_for_type(kit_id, type_id);
            let slot_list: Vec<JsonValue> = slots
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "typeId": s.type_id,
                        "typeName": sox_type_name(s.type_id),
                        "flags": s.flags,
                        "direction": slot_direction(s.flags, &s.name),
                    })
                })
                .collect();
            serde_json::json!({
                "kitId": kit_id,
                "typeId": type_id,
                "kitName": kit_name,
                "typeName": type_name,
                "category": category,
                "slots": slot_list,
            })
        })
        .collect();
    entries.sort_by(|a, b| {
        let ak = a.get("kitId").and_then(|v| v.as_u64()).unwrap_or(0);
        let at = a.get("typeId").and_then(|v| v.as_u64()).unwrap_or(0);
        let bk = b.get("kitId").and_then(|v| v.as_u64()).unwrap_or(0);
        let bt = b.get("typeId").and_then(|v| v.as_u64()).unwrap_or(0);
        (ak, at).cmp(&(bk, bt))
    });
    make_result(id, serde_json::json!(entries))
}

// ── Dynamic tag handlers ──────────────────────────

fn handle_read_tags(state: &RowsState, comp_id: u16, id: Option<&str>) -> Option<String> {
    let dyn_store = match &state.dyn_store {
        Some(ds) => ds,
        None => return make_error(id, "NOT_SUPPORTED", "dynamic tags not available"),
    };
    let store = dyn_store.read().expect("dyn_slots lock poisoned");
    let tags = store
        .get_all(comp_id)
        .map(|t| serde_json::to_value(t).unwrap_or_default())
        .unwrap_or_else(|| serde_json::json!({}));
    make_result(id, serde_json::json!({ "tags": tags }))
}

fn handle_set_tags(
    state: &RowsState,
    comp_id: u16,
    tags_val: &JsonValue,
    id: Option<&str>,
) -> Option<String> {
    let dyn_store = match &state.dyn_store {
        Some(ds) => ds,
        None => return make_error(id, "NOT_SUPPORTED", "dynamic tags not available"),
    };
    let obj = match tags_val.as_object() {
        Some(o) => o,
        None => return make_error(id, "BAD_REQUEST", "'tags' must be a JSON object"),
    };

    let mut store = dyn_store.write().expect("dyn_slots lock poisoned");
    for (key, val) in obj {
        let dv = serde_json::from_value::<DynValue>(val.clone())
            .unwrap_or_else(|_| super::tags::json_to_dynvalue(val));
        if let Err(e) = store.set(comp_id, key.clone(), dv) {
            return make_error(id, "LIMIT_EXCEEDED", &e);
        }
    }
    make_result(id, serde_json::json!({ "ok": true }))
}

fn handle_delete_tag(
    state: &RowsState,
    comp_id: u16,
    key: &str,
    id: Option<&str>,
) -> Option<String> {
    let dyn_store = match &state.dyn_store {
        Some(ds) => ds,
        None => return make_error(id, "NOT_SUPPORTED", "dynamic tags not available"),
    };
    let mut store = dyn_store.write().expect("dyn_slots lock poisoned");
    let removed = store.remove(comp_id, key).is_some();
    make_result(id, serde_json::json!({ "ok": true, "removed": removed }))
}

// ── JSON builders ──────────────────────────────────

fn comp_to_tree_json(comp: &VirtualComponent, tree: &ComponentTree) -> JsonValue {
    let position = if let Some(meta_slot) = comp.slots.first() {
        decode_position(&meta_slot.value)
    } else {
        (0, 0)
    };
    serde_json::json!({
        "compId": comp.comp_id,
        "parentId": comp.parent_id,
        "name": comp.name,
        "typeName": comp.type_name,
        "kitId": comp.kit_id,
        "typeId": comp.type_id,
        "children": comp.children,
        "position": [position.0, position.1],
        "isChannel": tree.is_channel_comp(comp.comp_id),
        "isSystem": is_system_comp(comp.comp_id),
    })
}

fn comp_to_detail_json(comp: &VirtualComponent, tree: &ComponentTree) -> JsonValue {
    let position = if let Some(meta_slot) = comp.slots.first() {
        decode_position(&meta_slot.value)
    } else {
        (0, 0)
    };
    let slots: Vec<JsonValue> = comp
        .slots
        .iter()
        .enumerate()
        .map(|(idx, slot)| {
            serde_json::json!({
                "index": idx,
                "name": slot.name,
                "typeId": slot.type_id,
                "typeName": sox_type_name(slot.type_id),
                "flags": slot.flags,
                "direction": slot_direction(slot.flags, &slot.name),
                "value": slot_value_to_json(&slot.value),
            })
        })
        .collect();
    let links: Vec<JsonValue> = comp
        .links
        .iter()
        .map(|l| {
            serde_json::json!({
                "fromComp": l.from_comp,
                "fromSlot": l.from_slot,
                "toComp": l.to_comp,
                "toSlot": l.to_slot,
            })
        })
        .collect();
    serde_json::json!({
        "compId": comp.comp_id,
        "parentId": comp.parent_id,
        "name": comp.name,
        "typeName": comp.type_name,
        "kitId": comp.kit_id,
        "typeId": comp.type_id,
        "children": comp.children,
        "position": [position.0, position.1],
        "isChannel": tree.is_channel_comp(comp.comp_id),
        "isSystem": is_system_comp(comp.comp_id),
        "slots": slots,
        "links": links,
    })
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
fn validate_name(name: &str) -> Option<&'static str> {
    crate::sox::name_intern::NameInternTable::validate_name(name)
}

// ── Tests ──────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(validate_name("1comp"), Some("name must start with a letter"));
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
        assert_eq!(validate_name("_comp"), Some("name must start with a letter"));
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
        use crate::sox::sox_handlers::{ComponentTree, ManifestDb, VirtualComponent, VirtualSlot, SlotValue};
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
        let msgs = build_cov_messages(&tree_handle, &subscribed, &mut cov_cache, &mut tree_cache_map, false);
        assert_eq!(msgs.len(), 1);
        let v: JsonValue = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(v["op"], "cov");
        assert_eq!(v["compId"], 100);

        // Second call: no change, no COV
        let msgs = build_cov_messages(&tree_handle, &subscribed, &mut cov_cache, &mut tree_cache_map, false);
        assert!(msgs.is_empty());

        // Mutate value
        {
            let mut t = tree_handle.write().unwrap();
            if let Some(comp) = t.get_mut(100) {
                comp.slots[0].value = SlotValue::Float(2.0);
            }
        }

        // Third call: should detect change
        let msgs = build_cov_messages(&tree_handle, &subscribed, &mut cov_cache, &mut tree_cache_map, false);
        assert_eq!(msgs.len(), 1);
        let v: JsonValue = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(v["op"], "cov");
        assert_eq!(v["slots"][0]["index"], 0);
    }

    #[test]
    fn cov_detects_tree_change() {
        use crate::sox::sox_handlers::{ComponentTree, ManifestDb, VirtualComponent, VirtualSlot, SlotValue};
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
        let _ = build_cov_messages(&tree_handle, &subscribed, &mut cov_cache, &mut tree_cache_map, false);

        // Add a child
        {
            let mut t = tree_handle.write().unwrap();
            if let Some(comp) = t.get_mut(50) {
                comp.children.push(100);
            }
        }

        let msgs = build_cov_messages(&tree_handle, &subscribed, &mut cov_cache, &mut tree_cache_map, false);
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

        let tree_handle: SharedComponentTree =
            Arc::new(std::sync::RwLock::new(tree_inner));
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
        assert!(compact_cov, "compact_cov should be true after nameTable request");
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

        let tree_handle: SharedComponentTree =
            Arc::new(std::sync::RwLock::new(tree_inner));
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
        assert!(slot.get("name").is_none(), "compact COV should not have 'name'");
        assert!(slot.get("index").is_none(), "compact COV should not have 'index'");
    }

    // ── Dynamic tag tests ──────────────────────

    fn make_test_state_with_dyn_store() -> (RowsState, DynSlotStoreHandle) {
        use crate::sox::sox_handlers::{ComponentTree, ManifestDb};
        let manifest_db = Arc::new(ManifestDb::load(""));
        let tree = Arc::new(std::sync::RwLock::new(
            ComponentTree::new_with_manifest(manifest_db.clone()),
        ));
        let dyn_store: DynSlotStoreHandle =
            Arc::new(std::sync::RwLock::new(crate::sox::dyn_slots::DynSlotStore::with_defaults()));
        let state = RowsState {
            tree,
            manifest_db,
            dyn_store: Some(dyn_store.clone()),
        };
        (state, dyn_store)
    }

    #[test]
    fn rows_read_comp_includes_tags_when_present() {
        use crate::sox::sox_handlers::{
            ComponentTree, ManifestDb, SlotValue, VirtualComponent, VirtualSlot,
        };
        use crate::sox::sox_protocol::SoxValueType;
        use crate::sox::dyn_slots::DynValue;

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
        let dyn_store: DynSlotStoreHandle =
            Arc::new(std::sync::RwLock::new(crate::sox::dyn_slots::DynSlotStore::with_defaults()));

        // Set a dynamic tag
        {
            let mut store = dyn_store.write().unwrap();
            store.set(100, "modbusAddr".into(), DynValue::Int(40001)).unwrap();
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
        assert!(v["data"]["tags"].is_object(), "readComp should include tags");
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
        let dyn_store: DynSlotStoreHandle =
            Arc::new(std::sync::RwLock::new(crate::sox::dyn_slots::DynSlotStore::with_defaults()));
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
        assert!(v["data"]["tags"].is_null(), "readComp should not include tags when empty");
    }

    #[test]
    fn rows_read_tags_command() {
        let (state, dyn_store) = make_test_state_with_dyn_store();
        {
            let mut store = dyn_store.write().unwrap();
            store.set(0, "testTag".into(), crate::sox::dyn_slots::DynValue::Str("hello".into())).unwrap();
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
            store.set(50, "foo".into(), crate::sox::dyn_slots::DynValue::Marker).unwrap();
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
        use crate::sox::sox_handlers::{
            ComponentTree, ManifestDb, SlotValue, VirtualComponent, VirtualSlot,
        };
        use crate::sox::sox_protocol::SoxValueType;
        use crate::sox::dyn_slots::DynValue;

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
        let dyn_store: DynSlotStoreHandle =
            Arc::new(std::sync::RwLock::new(crate::sox::dyn_slots::DynSlotStore::with_defaults()));

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
            store.set(200, "devEUI".into(), DynValue::Str("A81758".into())).unwrap();
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

    // ── Test helpers ───────────────────────────

    fn make_test_state() -> RowsState {
        use crate::sox::sox_handlers::{ComponentTree, ManifestDb};
        let manifest_db = Arc::new(ManifestDb::load(""));
        let tree = Arc::new(std::sync::RwLock::new(
            ComponentTree::new_with_manifest(manifest_db.clone()),
        ));
        RowsState { tree, manifest_db, dyn_store: None }
    }
}
