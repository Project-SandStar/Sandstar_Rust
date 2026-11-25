//! Shared SOX operation dispatcher (envelope-agnostic).
//!
//! Two transport layers are built on top of this module:
//! - `rest::rows` — the RoWS dialect (`{"op":"...","id":"...",...}` envelopes)
//! - `rest::live` — the Fantom dialect (`{"msgType":"...","replyNum":N,...}`),
//!   added in Phase 14.0G-2c.
//!
//! Each envelope is free to parse its incoming JSON into a [`SoxOp`], hand it
//! to [`apply_op`] together with the per-connection [`SessionState`], then
//! render the returned [`SoxResponse`] back in its own shape.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value as JsonValue;

use crate::sox::dyn_slots::DynValue;
use crate::sox::sox_handlers::{
    ComponentTree, ManifestDb, SlotValue, VirtualComponent, VirtualSlot, DEFAULT_KITS,
    SLOT_FLAG_ACTION, SLOT_FLAG_CONFIG,
};
use crate::sox::sox_protocol::SoxValueType;
use crate::sox::{DynSlotStoreHandle, SharedComponentTree};

use crate::rest::sox_api::{
    category_for_type, decode_position, encode_position, is_system_comp, json_to_slot_value,
    slot_direction, slot_value_to_json, sox_type_name,
};

// ── Shared connection state ────────────────────────

/// Per-connection shared state. Cloned into each WebSocket session.
/// Formerly known as `rest::rows::RowsState` before the G-2a refactor.
#[derive(Clone)]
pub struct SoxState {
    pub tree: SharedComponentTree,
    pub manifest_db: Arc<ManifestDb>,
    pub dyn_store: Option<DynSlotStoreHandle>,
}

/// Mutable per-session state passed through every op. Subscriptions, caches
/// and the compact-COV toggle live here; apply_op mutates in place.
pub struct SessionState<'a> {
    pub subscribed: &'a mut HashSet<u16>,
    pub cov_cache: &'a mut HashMap<u16, Vec<JsonValue>>,
    pub tree_cache: &'a mut HashMap<u16, Vec<u16>>,
    pub compact_cov: &'a mut bool,
}

// ── Op + response enums ────────────────────────────

/// Every SOX operation the dispatcher can execute.
///
/// Covers the 19 operations RoWS has supported since Phase 14.0A plus 5
/// added in Phase 14.0G-2b for editor parity (`Version`, `VersionExt`,
/// `Reorder`, `Query`, `CloseWatch`). New ops go here; both envelopes
/// pick them up automatically once their parser learns to recognise the
/// wire form.
pub enum SoxOp {
    // RoWS v1 ops (Phase 14.0A)
    Ping,
    ReadTree,
    ReadComp {
        comp_id: u16,
    },
    WriteSlot {
        comp_id: u16,
        slot_idx: usize,
        value: JsonValue,
    },
    AddComp {
        parent_id: u16,
        kit_id: u8,
        type_id: u8,
        name: String,
    },
    DeleteComp {
        comp_id: u16,
    },
    Rename {
        comp_id: u16,
        name: String,
    },
    AddLink {
        from_comp: u16,
        from_slot: u8,
        to_comp: u16,
        to_slot: u8,
    },
    DeleteLink {
        from_comp: u16,
        from_slot: u8,
        to_comp: u16,
        to_slot: u8,
    },
    UpdatePos {
        comp_id: u16,
        x: u8,
        y: u8,
    },
    Invoke {
        comp_id: u16,
        slot_idx: usize,
    },
    NameTable,
    Palette,
    Subscribe {
        comp_ids: Vec<u16>,
    },
    Unsubscribe {
        comp_ids: Vec<u16>,
    },
    ReadTags {
        comp_id: u16,
    },
    SetTags {
        comp_id: u16,
        tags: JsonValue,
    },
    DeleteTag {
        comp_id: u16,
        key: String,
    },
    SetFormat {
        format: String,
    },
    // G-2b additions — editor-parity ops. Wire contract frozen in
    // docs/Progress/NOTES_FANTOM_MSGTYPE_SEMANTICS.md (2026-04-23).
    Version,
    VersionExt,
    Reorder {
        comp_id: u16,
        children: Vec<u16>,
    },
    Query {
        query_type: char,
        kit_id: u8,
        type_id: u8,
    },
    CloseWatch {
        watch_id: String,
    },
}

/// Outcome of an op. Envelopes render these into their own shape.
pub enum SoxResponse {
    /// Success with a JSON payload. RoWS wraps it as `{"op":"result","data":…}`;
    /// the `/live` handler in G-2c encodes it per msgType.
    Ok(JsonValue),
    /// Error with a machine-readable code and a human-readable message.
    Error { code: &'static str, message: String },
}

impl SoxResponse {
    #[inline]
    pub fn ok(value: JsonValue) -> Self {
        SoxResponse::Ok(value)
    }

    #[inline]
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        SoxResponse::Error {
            code,
            message: message.into(),
        }
    }
}

// ── Dispatcher ─────────────────────────────────────

/// Execute a parsed op against shared state + mutable session state.
/// Envelope layers call this and then render the returned response.
pub fn apply_op(state: &SoxState, op: SoxOp, session: &mut SessionState<'_>) -> SoxResponse {
    match op {
        SoxOp::Ping => SoxResponse::ok(serde_json::json!({ "pong": true })),
        SoxOp::ReadTree => handle_read_tree(state),
        SoxOp::ReadComp { comp_id } => handle_read_comp(state, comp_id),
        SoxOp::WriteSlot {
            comp_id,
            slot_idx,
            value,
        } => handle_write_slot(state, comp_id, slot_idx, &value),
        SoxOp::AddComp {
            parent_id,
            kit_id,
            type_id,
            name,
        } => handle_add_comp(state, parent_id, kit_id, type_id, &name),
        SoxOp::DeleteComp { comp_id } => handle_delete_comp(state, comp_id),
        SoxOp::Rename { comp_id, name } => handle_rename(state, comp_id, &name),
        SoxOp::AddLink {
            from_comp,
            from_slot,
            to_comp,
            to_slot,
        } => handle_add_link(state, from_comp, from_slot, to_comp, to_slot),
        SoxOp::DeleteLink {
            from_comp,
            from_slot,
            to_comp,
            to_slot,
        } => handle_delete_link(state, from_comp, from_slot, to_comp, to_slot),
        SoxOp::UpdatePos { comp_id, x, y } => handle_update_pos(state, comp_id, x, y),
        SoxOp::Invoke { comp_id, slot_idx } => handle_invoke(state, comp_id, slot_idx),
        SoxOp::NameTable => handle_name_table(state, session),
        SoxOp::Palette => handle_palette(state),
        SoxOp::Subscribe { comp_ids } => handle_subscribe(state, comp_ids, session),
        SoxOp::Unsubscribe { comp_ids } => handle_unsubscribe(comp_ids, session),
        SoxOp::ReadTags { comp_id } => handle_read_tags(state, comp_id),
        SoxOp::SetTags { comp_id, tags } => handle_set_tags(state, comp_id, &tags),
        SoxOp::DeleteTag { comp_id, key } => handle_delete_tag(state, comp_id, &key),
        SoxOp::SetFormat { format } => handle_set_format(&format),
        SoxOp::Version => handle_version(),
        SoxOp::VersionExt => handle_version_ext(),
        SoxOp::Reorder { comp_id, children } => handle_reorder(state, comp_id, children),
        SoxOp::Query {
            query_type,
            kit_id,
            type_id,
        } => handle_query(state, query_type, kit_id, type_id),
        SoxOp::CloseWatch { watch_id } => handle_close_watch(&watch_id, session),
    }
}

// ── Op handlers ────────────────────────────────────

fn handle_read_tree(state: &SoxState) -> SoxResponse {
    let tree = crate::lock::read_or_recover(&state.tree, "SOX component tree");
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
    SoxResponse::ok(serde_json::json!({ "components": comps }))
}

fn handle_read_comp(state: &SoxState, comp_id: u16) -> SoxResponse {
    let tree = crate::lock::read_or_recover(&state.tree, "SOX component tree");
    match tree.get(comp_id) {
        Some(comp) => {
            let mut detail = comp_to_detail_json(comp, &tree);
            if let Some(ref dyn_store) = state.dyn_store {
                if let Ok(store) = dyn_store.read() {
                    if let Some(tags) = store.get_all(comp_id) {
                        if !tags.is_empty() {
                            detail["tags"] = serde_json::to_value(tags).unwrap_or_default();
                        }
                    }
                }
            }
            SoxResponse::ok(detail)
        }
        None => SoxResponse::error("NOT_FOUND", format!("component {comp_id} not found")),
    }
}

fn handle_write_slot(
    state: &SoxState,
    comp_id: u16,
    slot_idx: usize,
    value: &JsonValue,
) -> SoxResponse {
    let mut tree = crate::lock::write_or_recover(&state.tree, "SOX component tree");
    let slot_type_id = {
        let comp = match tree.get(comp_id) {
            Some(c) => c,
            None => {
                return SoxResponse::error("NOT_FOUND", format!("component {comp_id} not found"))
            }
        };
        match comp.slots.get(slot_idx) {
            Some(slot) => slot.type_id,
            None => {
                return SoxResponse::error(
                    "BAD_REQUEST",
                    format!("slot index {slot_idx} out of range"),
                );
            }
        }
    };
    let new_value = match json_to_slot_value(value, slot_type_id) {
        Some(v) => v,
        None => {
            return SoxResponse::error(
                "BAD_REQUEST",
                format!("cannot coerce value to type_id {slot_type_id}"),
            );
        }
    };
    if let Some(comp) = tree.get_mut(comp_id) {
        if let Some(slot) = comp.slots.get_mut(slot_idx) {
            slot.value = new_value;
        }
    }
    tree.mark_dirty();
    SoxResponse::ok(serde_json::json!({ "ok": true }))
}

fn handle_add_comp(
    state: &SoxState,
    parent_id: u16,
    kit_id: u8,
    type_id: u8,
    name: &str,
) -> SoxResponse {
    if let Some(err) = validate_name(name) {
        return SoxResponse::error("BAD_REQUEST", err);
    }

    let mut tree = crate::lock::write_or_recover(&state.tree, "SOX component tree");

    if tree.get(parent_id).is_none() {
        return SoxResponse::error("NOT_FOUND", format!("parent {parent_id} not found"));
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

    SoxResponse::ok(serde_json::json!({ "compId": comp_id }))
}

fn handle_delete_comp(state: &SoxState, comp_id: u16) -> SoxResponse {
    if comp_id < 7 {
        return SoxResponse::error("FORBIDDEN", "cannot delete system components (id < 7)");
    }

    let mut tree = crate::lock::write_or_recover(&state.tree, "SOX component tree");

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
            SoxResponse::ok(serde_json::json!({ "ok": true }))
        }
        None => SoxResponse::error("NOT_FOUND", format!("component {comp_id} not found")),
    }
}

fn handle_rename(state: &SoxState, comp_id: u16, name: &str) -> SoxResponse {
    if let Some(err) = validate_name(name) {
        return SoxResponse::error("BAD_REQUEST", err);
    }

    let mut tree = crate::lock::write_or_recover(&state.tree, "SOX component tree");
    if tree.rename(comp_id, name.to_string()) {
        tree.mark_dirty();
        SoxResponse::ok(serde_json::json!({ "ok": true }))
    } else {
        SoxResponse::error("NOT_FOUND", format!("component {comp_id} not found"))
    }
}

fn handle_add_link(
    state: &SoxState,
    from_comp: u16,
    from_slot: u8,
    to_comp: u16,
    to_slot: u8,
) -> SoxResponse {
    let mut tree = crate::lock::write_or_recover(&state.tree, "SOX component tree");
    if tree.add_link(from_comp, from_slot, to_comp, to_slot) {
        tree.mark_dirty();
        SoxResponse::ok(serde_json::json!({ "ok": true }))
    } else {
        SoxResponse::error("CONFLICT", "link already exists or would create a cycle")
    }
}

fn handle_delete_link(
    state: &SoxState,
    from_comp: u16,
    from_slot: u8,
    to_comp: u16,
    to_slot: u8,
) -> SoxResponse {
    let mut tree = crate::lock::write_or_recover(&state.tree, "SOX component tree");
    if tree.remove_link(from_comp, from_slot, to_comp, to_slot) {
        tree.mark_dirty();
        SoxResponse::ok(serde_json::json!({ "ok": true }))
    } else {
        SoxResponse::error("NOT_FOUND", "link not found")
    }
}

fn handle_update_pos(state: &SoxState, comp_id: u16, x: u8, y: u8) -> SoxResponse {
    let mut tree = crate::lock::write_or_recover(&state.tree, "SOX component tree");
    let comp = match tree.get_mut(comp_id) {
        Some(c) => c,
        None => return SoxResponse::error("NOT_FOUND", format!("component {comp_id} not found")),
    };
    if comp.slots.is_empty() {
        return SoxResponse::error("BAD_REQUEST", "component has no meta slot");
    }
    comp.slots[0].value = encode_position(x, y);
    tree.mark_dirty();
    SoxResponse::ok(serde_json::json!({ "ok": true }))
}

fn handle_invoke(state: &SoxState, comp_id: u16, slot_idx: usize) -> SoxResponse {
    let mut tree = crate::lock::write_or_recover(&state.tree, "SOX component tree");
    let comp = match tree.get_mut(comp_id) {
        Some(c) => c,
        None => return SoxResponse::error("NOT_FOUND", format!("component {comp_id} not found")),
    };
    let slot = match comp.slots.get(slot_idx) {
        Some(s) => s,
        None => {
            return SoxResponse::error(
                "BAD_REQUEST",
                format!("slot index {slot_idx} out of range"),
            );
        }
    };
    if slot.flags & SLOT_FLAG_ACTION == 0 {
        return SoxResponse::error("BAD_REQUEST", "slot is not an action slot");
    }
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
    SoxResponse::ok(serde_json::json!({ "ok": true }))
}

fn handle_name_table(state: &SoxState, session: &mut SessionState<'_>) -> SoxResponse {
    let tree = crate::lock::read_or_recover(&state.tree, "SOX component tree");
    let names: Vec<JsonValue> = tree
        .name_table
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
    *session.compact_cov = true;
    SoxResponse::ok(serde_json::json!({ "names": names }))
}

fn handle_palette(state: &SoxState) -> SoxResponse {
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
    SoxResponse::ok(serde_json::json!(entries))
}

fn handle_subscribe(
    state: &SoxState,
    comp_ids: Vec<u16>,
    session: &mut SessionState<'_>,
) -> SoxResponse {
    for cid in &comp_ids {
        session.subscribed.insert(*cid);
    }
    // Prime the cache for newly subscribed components.
    {
        let tree = crate::lock::read_or_recover(&state.tree, "SOX component tree");
        for &cid in &comp_ids {
            if let Some(comp) = tree.get(cid) {
                let vals: Vec<JsonValue> = comp
                    .slots
                    .iter()
                    .map(|s| slot_value_to_json(&s.value))
                    .collect();
                session.cov_cache.insert(cid, vals);
                session.tree_cache.insert(cid, comp.children.clone());
            }
        }
    }
    SoxResponse::ok(serde_json::json!({
        "subscribed": comp_ids,
        "count": session.subscribed.len(),
    }))
}

fn handle_unsubscribe(comp_ids: Vec<u16>, session: &mut SessionState<'_>) -> SoxResponse {
    for cid in &comp_ids {
        session.subscribed.remove(cid);
        session.cov_cache.remove(cid);
        session.tree_cache.remove(cid);
    }
    SoxResponse::ok(serde_json::json!({
        "unsubscribed": comp_ids,
        "count": session.subscribed.len(),
    }))
}

fn handle_read_tags(state: &SoxState, comp_id: u16) -> SoxResponse {
    let dyn_store = match &state.dyn_store {
        Some(ds) => ds,
        None => return SoxResponse::error("NOT_SUPPORTED", "dynamic tags not available"),
    };
    let store = crate::lock::read_or_recover(dyn_store, "dyn slot store");
    let tags = store
        .get_all(comp_id)
        .map(|t| serde_json::to_value(t).unwrap_or_default())
        .unwrap_or_else(|| serde_json::json!({}));
    SoxResponse::ok(serde_json::json!({ "tags": tags }))
}

fn handle_set_tags(state: &SoxState, comp_id: u16, tags_val: &JsonValue) -> SoxResponse {
    let dyn_store = match &state.dyn_store {
        Some(ds) => ds,
        None => return SoxResponse::error("NOT_SUPPORTED", "dynamic tags not available"),
    };
    let obj = match tags_val.as_object() {
        Some(o) => o,
        None => return SoxResponse::error("BAD_REQUEST", "'tags' must be a JSON object"),
    };

    let tags = obj
        .iter()
        .map(|(key, val)| {
            let dv = serde_json::from_value::<DynValue>(val.clone())
                .unwrap_or_else(|_| crate::rest::tags::json_to_dynvalue(val));
            (key.clone(), dv)
        })
        .collect();
    let mut store = crate::lock::write_or_recover(dyn_store, "dyn slot store");
    // All tags or none; a bad name or value is the request's fault (R-48).
    match store.set_many(comp_id, tags) {
        Ok(()) => SoxResponse::ok(serde_json::json!({ "ok": true })),
        Err(e) if e.is_limit() => SoxResponse::error("LIMIT_EXCEEDED", e.to_string()),
        Err(e) => SoxResponse::error("BAD_REQUEST", e.to_string()),
    }
}

fn handle_delete_tag(state: &SoxState, comp_id: u16, key: &str) -> SoxResponse {
    let dyn_store = match &state.dyn_store {
        Some(ds) => ds,
        None => return SoxResponse::error("NOT_SUPPORTED", "dynamic tags not available"),
    };
    let mut store = crate::lock::write_or_recover(dyn_store, "dyn slot store");
    let removed = store.remove(comp_id, key).is_some();
    SoxResponse::ok(serde_json::json!({ "ok": true, "removed": removed }))
}

fn handle_set_format(format: &str) -> SoxResponse {
    // setFormat allows the client to request Trio text encoding for responses.
    // Currently accepted for forward-compatibility but JSON remains the default.
    match format {
        "json" | "trio" => SoxResponse::ok(serde_json::json!({
            "format": format,
            "status": "ok",
        })),
        _ => SoxResponse::error("BAD_REQUEST", format!("unsupported format: {format}")),
    }
}

// ── G-2b: editor-parity handlers ───────────────────

fn handle_version() -> SoxResponse {
    let kits: Vec<JsonValue> = DEFAULT_KITS
        .iter()
        .map(|k| {
            serde_json::json!({
                "name": k.name,
                "checksum": k.checksum,
            })
        })
        .collect();
    SoxResponse::ok(serde_json::json!({
        "kitCount": kits.len(),
        "kits": kits,
    }))
}

fn handle_version_ext() -> SoxResponse {
    let kit_versions: Vec<JsonValue> = DEFAULT_KITS
        .iter()
        .map(|k| {
            serde_json::json!({
                "name": k.name,
                "checksum": k.checksum,
            })
        })
        .collect();
    // platformId, scodeFlags, pairs are Fantom-side legacy fields — the pod
    // returns empty/0 placeholders, so we match.
    SoxResponse::ok(serde_json::json!({
        "platformId": "",
        "scodeFlags": 0,
        "kitVersions": kit_versions,
        "pairs": [],
    }))
}

fn handle_reorder(state: &SoxState, comp_id: u16, children: Vec<u16>) -> SoxResponse {
    let mut tree = crate::lock::write_or_recover(&state.tree, "SOX component tree");
    let comp = match tree.get_mut(comp_id) {
        Some(c) => c,
        None => return SoxResponse::error("NOT_FOUND", format!("component {comp_id} not found")),
    };
    // Match Fantom semantics: accept the client's ordering verbatim (no
    // membership validation). Callers are trusted to pass valid child IDs.
    comp.children = children;
    tree.mark_dirty();
    SoxResponse::ok(serde_json::json!({ "ok": true }))
}

fn handle_query(state: &SoxState, query_type: char, kit_id: u8, type_id: u8) -> SoxResponse {
    if query_type != 's' {
        return SoxResponse::error("BAD_REQUEST", format!("unknown query type: {query_type}"));
    }
    // v1 semantic: "type matches exactly". Fantom's pod checks declared
    // service interfaces via its Java client; we start narrower and widen
    // if a real caller needs the interface-walk. Spec: see
    // docs/Progress/NOTES_FANTOM_MSGTYPE_SEMANTICS.md.
    if state.manifest_db.type_name(kit_id, type_id).is_none() {
        return SoxResponse::error(
            "NOT_FOUND",
            format!("type not found: kitId={kit_id} typeId={type_id}"),
        );
    }
    let tree = crate::lock::read_or_recover(&state.tree, "SOX component tree");
    let matches: Vec<u16> = tree
        .comp_ids()
        .into_iter()
        .filter(|cid| {
            tree.get(*cid)
                .map(|c| c.kit_id == kit_id && c.type_id == type_id)
                .unwrap_or(false)
        })
        .collect();
    SoxResponse::ok(serde_json::json!({ "compIds": matches }))
}

fn handle_close_watch(watch_id: &str, session: &mut SessionState<'_>) -> SoxResponse {
    // v1 semantic: sessions carry one implicit watch, so closeWatch clears
    // everything regardless of the watch_id string. Multi-watch indirection
    // is deferred to a future minor release if a real caller surfaces.
    let unsubscribed = session.subscribed.len() as u64;
    session.subscribed.clear();
    session.cov_cache.clear();
    session.tree_cache.clear();
    SoxResponse::ok(serde_json::json!({
        "watchId": watch_id,
        "unsubscribed": unsubscribed,
    }))
}

// ── JSON builders (shared across handlers) ─────────

pub(crate) fn comp_to_tree_json(comp: &VirtualComponent, tree: &ComponentTree) -> JsonValue {
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

pub(crate) fn comp_to_detail_json(comp: &VirtualComponent, tree: &ComponentTree) -> JsonValue {
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

// ── Request field helpers (shared by the envelopes) ─

/// A list of component ids for an op that replaces state with it (reorder).
/// `None` if the field is missing, not an array, or holds anything that is
/// not a 16-bit id: dropping one element would drop a child from the parent.
pub(crate) fn strict_u16_list(value: Option<&JsonValue>) -> Option<Vec<u16>> {
    value?
        .as_array()?
        .iter()
        .map(|v| v.as_u64().and_then(|n| u16::try_from(n).ok()))
        .collect()
}

// ── Name validation ────────────────────────────────

fn validate_name(name: &str) -> Option<&'static str> {
    crate::sox::name_intern::NameInternTable::validate_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sox::dyn_slots::DynSlotStore;
    use crate::sox::sox_handlers::NO_PARENT;
    use serde_json::json;
    use std::sync::RwLock;

    const ROOT: u16 = 0;
    const A: u16 = 10;
    const B: u16 = 11;
    const MISSING: u16 = 999;

    // Slot indices of the test component.
    const S_META: usize = 0;
    const S_FLOAT: usize = 1;
    const S_INT: usize = 2;
    const S_BOOL: usize = 3;
    const S_ACTION: usize = 4;
    const S_STR: usize = 5;

    fn slot(name: &str, ty: SoxValueType, flags: u8, value: SlotValue) -> VirtualSlot {
        VirtualSlot {
            name: name.into(),
            type_id: ty as u8,
            flags,
            value,
        }
    }

    fn comp(comp_id: u16, parent_id: u16, name: &str) -> VirtualComponent {
        VirtualComponent {
            comp_id,
            parent_id,
            name: name.into(),
            type_name: "Test".into(),
            kit_id: 200,
            type_id: 1,
            children: Vec::new(),
            slots: vec![
                slot(
                    "meta",
                    SoxValueType::Int,
                    SLOT_FLAG_CONFIG,
                    SlotValue::Int(1),
                ),
                slot("out", SoxValueType::Float, 0, SlotValue::Float(0.0)),
                slot("count", SoxValueType::Int, 0, SlotValue::Int(0)),
                slot("flag", SoxValueType::Bool, 0, SlotValue::Bool(false)),
                slot(
                    "setTrue",
                    SoxValueType::Void,
                    SLOT_FLAG_ACTION,
                    SlotValue::Null,
                ),
                slot("label", SoxValueType::Buf, 0, SlotValue::Str(String::new())),
            ],
            links: Vec::new(),
        }
    }

    /// Root (id 0) with two children A and B; tags enabled.
    fn make_state() -> SoxState {
        let manifest_db = Arc::new(ManifestDb::load(""));
        let mut tree = ComponentTree::new_with_manifest(manifest_db.clone());
        tree.add(comp(ROOT, NO_PARENT, "app"));
        tree.add(comp(A, ROOT, "compA"));
        tree.add(comp(B, ROOT, "compB"));
        SoxState {
            tree: Arc::new(RwLock::new(tree)),
            manifest_db,
            dyn_store: Some(Arc::new(RwLock::new(DynSlotStore::new(4, 100)))),
        }
    }

    #[derive(Default)]
    struct Session {
        subscribed: HashSet<u16>,
        cov_cache: HashMap<u16, Vec<JsonValue>>,
        tree_cache: HashMap<u16, Vec<u16>>,
        compact_cov: bool,
    }

    impl Session {
        fn apply(&mut self, state: &SoxState, op: SoxOp) -> SoxResponse {
            let mut session = SessionState {
                subscribed: &mut self.subscribed,
                cov_cache: &mut self.cov_cache,
                tree_cache: &mut self.tree_cache,
                compact_cov: &mut self.compact_cov,
            };
            apply_op(state, op, &mut session)
        }
    }

    fn apply(state: &SoxState, op: SoxOp) -> SoxResponse {
        Session::default().apply(state, op)
    }

    fn ok(resp: SoxResponse) -> JsonValue {
        match resp {
            SoxResponse::Ok(v) => v,
            SoxResponse::Error { code, message } => panic!("expected Ok, got {code}: {message}"),
        }
    }

    fn err_code(resp: SoxResponse) -> &'static str {
        match resp {
            SoxResponse::Error { code, .. } => code,
            SoxResponse::Ok(v) => panic!("expected Error, got Ok({v})"),
        }
    }

    fn slot_value(state: &SoxState, comp_id: u16, idx: usize) -> SlotValue {
        state.tree.read().unwrap().get(comp_id).unwrap().slots[idx]
            .value
            .clone()
    }

    fn link_count(state: &SoxState, comp_id: u16) -> usize {
        state.tree.read().unwrap().get(comp_id).unwrap().links.len()
    }

    fn write(comp_id: u16, slot_idx: usize, value: JsonValue) -> SoxOp {
        SoxOp::WriteSlot {
            comp_id,
            slot_idx,
            value,
        }
    }

    fn link(from_comp: u16, from_slot: u8, to_comp: u16, to_slot: u8) -> SoxOp {
        SoxOp::AddLink {
            from_comp,
            from_slot,
            to_comp,
            to_slot,
        }
    }

    // ── read ───────────────────────────────────────

    #[test]
    fn ping_answers_pong() {
        let state = make_state();
        assert_eq!(ok(apply(&state, SoxOp::Ping)), json!({ "pong": true }));
    }

    #[test]
    fn read_comp_returns_slots_and_missing_is_not_found() {
        let state = make_state();
        let detail = ok(apply(&state, SoxOp::ReadComp { comp_id: A }));
        assert_eq!(detail["compId"], A);
        assert_eq!(detail["parentId"], ROOT);
        assert_eq!(detail["name"], "compA");
        assert_eq!(detail["slots"].as_array().unwrap().len(), 6);

        assert_eq!(
            err_code(apply(&state, SoxOp::ReadComp { comp_id: MISSING })),
            "NOT_FOUND"
        );
    }

    // ── write slot ─────────────────────────────────

    #[test]
    fn write_slot_stores_each_supported_type() {
        let state = make_state();
        ok(apply(&state, write(A, S_FLOAT, json!(21.5))));
        ok(apply(&state, write(A, S_INT, json!(-7))));
        ok(apply(&state, write(A, S_BOOL, json!(true))));
        ok(apply(&state, write(A, S_STR, json!("çğü"))));

        assert_eq!(slot_value(&state, A, S_FLOAT), SlotValue::Float(21.5));
        assert_eq!(slot_value(&state, A, S_INT), SlotValue::Int(-7));
        assert_eq!(slot_value(&state, A, S_BOOL), SlotValue::Bool(true));
        assert_eq!(slot_value(&state, A, S_STR), SlotValue::Str("çğü".into()));
        // The other component is untouched.
        assert_eq!(slot_value(&state, B, S_FLOAT), SlotValue::Float(0.0));
    }

    #[test]
    fn write_slot_rejects_missing_component_and_slot() {
        let state = make_state();
        assert_eq!(
            err_code(apply(&state, write(MISSING, S_FLOAT, json!(1.0)))),
            "NOT_FOUND"
        );
        assert_eq!(
            err_code(apply(&state, write(A, 6, json!(1.0)))),
            "BAD_REQUEST"
        );
        assert_eq!(
            err_code(apply(&state, write(A, usize::MAX, json!(1.0)))),
            "BAD_REQUEST"
        );
    }

    #[test]
    fn write_slot_rejects_a_value_of_the_wrong_type() {
        let state = make_state();
        for (idx, bad) in [
            (S_FLOAT, json!("12")),
            (S_FLOAT, json!(null)),
            (S_INT, json!(1.5)),
            (S_INT, json!("1")),
            (S_BOOL, json!(1)),
            (S_STR, json!(5)),
            (S_FLOAT, json!([1.0])),
            (S_FLOAT, json!({ "v": 1.0 })),
        ] {
            assert_eq!(
                err_code(apply(&state, write(A, idx, bad.clone()))),
                "BAD_REQUEST",
                "slot {idx} accepted {bad}"
            );
        }
        assert_eq!(slot_value(&state, A, S_FLOAT), SlotValue::Float(0.0));
        assert_eq!(slot_value(&state, A, S_INT), SlotValue::Int(0));
    }

    /// A number too large for `f32` must not become infinity in the slot
    /// (`numeric-output.md`: non-finite values are rejected, not stored).
    #[test]
    fn write_slot_rejects_a_float_that_does_not_fit_f32() {
        let state = make_state();
        for big in [1e300, -1e300, 3.5e38] {
            assert_eq!(
                err_code(apply(&state, write(A, S_FLOAT, json!(big)))),
                "BAD_REQUEST",
                "{big} accepted"
            );
        }
        assert_eq!(slot_value(&state, A, S_FLOAT), SlotValue::Float(0.0));
    }

    /// An integer outside `i32` must not be cut down silently
    /// (`numeric-output.md`: range check before `as`).
    #[test]
    fn write_slot_rejects_an_int_outside_i32() {
        let state = make_state();
        for big in [i64::from(i32::MAX) + 1, i64::from(i32::MIN) - 1, i64::MAX] {
            assert_eq!(
                err_code(apply(&state, write(A, S_INT, json!(big)))),
                "BAD_REQUEST",
                "{big} accepted"
            );
        }
        assert_eq!(slot_value(&state, A, S_INT), SlotValue::Int(0));
    }

    // ── add / delete / rename ──────────────────────

    fn add(parent_id: u16, name: &str) -> SoxOp {
        SoxOp::AddComp {
            parent_id,
            kit_id: 200,
            type_id: 1,
            name: name.into(),
        }
    }

    #[test]
    fn add_comp_creates_a_child_with_a_fresh_id() {
        let state = make_state();
        let reply = ok(apply(&state, add(A, "child1")));
        let id = reply["compId"].as_u64().unwrap() as u16;
        assert!(id > B);

        let tree = state.tree.read().unwrap();
        let child = tree.get(id).unwrap();
        assert_eq!(child.parent_id, A);
        assert_eq!(child.name, "child1");
        assert!(!child.slots.is_empty());
        assert_eq!(tree.get(A).unwrap().children, vec![id]);
    }

    #[test]
    fn add_comp_rejects_bad_names_and_missing_parent() {
        let state = make_state();
        let too_long = "a".repeat(32);
        for bad in ["", "1abc", "has space", "ünlü", "a-b", too_long.as_str()] {
            assert_eq!(
                err_code(apply(&state, add(A, bad))),
                "BAD_REQUEST",
                "name {bad:?} accepted"
            );
        }
        assert_eq!(err_code(apply(&state, add(MISSING, "ok1"))), "NOT_FOUND");
        assert_eq!(state.tree.read().unwrap().len(), 3);
    }

    #[test]
    fn delete_comp_removes_it_from_its_parent() {
        let state = make_state();
        ok(apply(&state, SoxOp::DeleteComp { comp_id: A }));
        let tree = state.tree.read().unwrap();
        assert!(tree.get(A).is_none());
        assert_eq!(tree.get(ROOT).unwrap().children, vec![B]);
    }

    #[test]
    fn delete_comp_refuses_system_ids_and_reports_missing() {
        let state = make_state();
        for id in 0..7u16 {
            assert_eq!(
                err_code(apply(&state, SoxOp::DeleteComp { comp_id: id })),
                "FORBIDDEN"
            );
        }
        assert_eq!(
            err_code(apply(&state, SoxOp::DeleteComp { comp_id: MISSING })),
            "NOT_FOUND"
        );
        assert_eq!(state.tree.read().unwrap().len(), 3);
    }

    #[test]
    fn delete_comp_drops_its_links_from_the_other_end() {
        let state = make_state();
        ok(apply(&state, link(A, 1, B, 1)));
        assert_eq!(link_count(&state, B), 1);

        ok(apply(&state, SoxOp::DeleteComp { comp_id: A }));
        assert_eq!(link_count(&state, B), 0);
    }

    #[test]
    fn rename_changes_the_name_and_validates_it() {
        let state = make_state();
        ok(apply(
            &state,
            SoxOp::Rename {
                comp_id: A,
                name: "renamed".into(),
            },
        ));
        assert_eq!(state.tree.read().unwrap().get(A).unwrap().name, "renamed");

        let bad = |comp_id, name: &str| {
            err_code(apply(
                &state,
                SoxOp::Rename {
                    comp_id,
                    name: name.into(),
                },
            ))
        };
        assert_eq!(bad(A, ""), "BAD_REQUEST");
        assert_eq!(bad(A, "9x"), "BAD_REQUEST");
        assert_eq!(bad(A, "şç"), "BAD_REQUEST");
        assert_eq!(bad(MISSING, "fine"), "NOT_FOUND");
        assert_eq!(state.tree.read().unwrap().get(A).unwrap().name, "renamed");
    }

    // ── links ──────────────────────────────────────

    #[test]
    fn add_link_is_stored_on_both_ends_and_delete_removes_both() {
        let state = make_state();
        ok(apply(&state, link(A, 1, B, 1)));
        assert_eq!(link_count(&state, A), 1);
        assert_eq!(link_count(&state, B), 1);

        ok(apply(
            &state,
            SoxOp::DeleteLink {
                from_comp: A,
                from_slot: 1,
                to_comp: B,
                to_slot: 1,
            },
        ));
        assert_eq!(link_count(&state, A), 0);
        assert_eq!(link_count(&state, B), 0);
    }

    #[test]
    fn duplicate_link_and_cycle_are_conflicts() {
        let state = make_state();
        ok(apply(&state, link(A, 1, B, 1)));
        assert_eq!(err_code(apply(&state, link(A, 1, B, 1))), "CONFLICT");
        assert_eq!(err_code(apply(&state, link(B, 1, A, 1))), "CONFLICT");
        assert_eq!(link_count(&state, A), 1);
        assert_eq!(link_count(&state, B), 1);
    }

    #[test]
    fn a_second_source_replaces_the_link_on_the_same_input() {
        let state = make_state();
        let c = ok(apply(&state, add(ROOT, "compC")))["compId"]
            .as_u64()
            .unwrap() as u16;
        ok(apply(&state, link(A, 1, B, 1)));
        ok(apply(&state, link(c, 1, B, 1)));

        let tree = state.tree.read().unwrap();
        let incoming: Vec<u16> = tree
            .get(B)
            .unwrap()
            .links
            .iter()
            .filter(|l| l.to_comp == B && l.to_slot == 1)
            .map(|l| l.from_comp)
            .collect();
        assert_eq!(incoming, vec![c]);
        assert!(tree.get(A).unwrap().links.is_empty());
    }

    #[test]
    fn delete_of_a_missing_link_is_not_found() {
        let state = make_state();
        assert_eq!(
            err_code(apply(
                &state,
                SoxOp::DeleteLink {
                    from_comp: A,
                    from_slot: 1,
                    to_comp: B,
                    to_slot: 1,
                },
            )),
            "NOT_FOUND"
        );
    }

    // ── position / invoke / reorder / query ────────

    #[test]
    fn update_pos_writes_the_meta_slot() {
        let state = make_state();
        ok(apply(
            &state,
            SoxOp::UpdatePos {
                comp_id: A,
                x: 12,
                y: 34,
            },
        ));
        assert_eq!(decode_position(&slot_value(&state, A, S_META)), (12, 34));
        assert_eq!(
            err_code(apply(
                &state,
                SoxOp::UpdatePos {
                    comp_id: MISSING,
                    x: 1,
                    y: 1,
                },
            )),
            "NOT_FOUND"
        );
    }

    #[test]
    fn invoke_runs_set_true_and_rejects_non_action_slots() {
        let state = make_state();
        ok(apply(
            &state,
            SoxOp::Invoke {
                comp_id: A,
                slot_idx: S_ACTION,
            },
        ));
        assert_eq!(slot_value(&state, A, S_BOOL), SlotValue::Bool(true));

        let bad = |comp_id, slot_idx| err_code(apply(&state, SoxOp::Invoke { comp_id, slot_idx }));
        assert_eq!(bad(A, S_FLOAT), "BAD_REQUEST");
        assert_eq!(bad(A, 99), "BAD_REQUEST");
        assert_eq!(bad(MISSING, S_ACTION), "NOT_FOUND");
    }

    #[test]
    fn reorder_sets_children_and_missing_is_not_found() {
        let state = make_state();
        ok(apply(
            &state,
            SoxOp::Reorder {
                comp_id: ROOT,
                children: vec![B, A],
            },
        ));
        assert_eq!(
            state.tree.read().unwrap().get(ROOT).unwrap().children,
            vec![B, A]
        );
        assert_eq!(
            err_code(apply(
                &state,
                SoxOp::Reorder {
                    comp_id: MISSING,
                    children: vec![],
                },
            )),
            "NOT_FOUND"
        );
    }

    #[test]
    fn query_rejects_unknown_query_type_and_unknown_type() {
        let state = make_state();
        let q = |query_type, kit_id, type_id| {
            apply(
                &state,
                SoxOp::Query {
                    query_type,
                    kit_id,
                    type_id,
                },
            )
        };
        assert_eq!(err_code(q('x', 0, 0)), "BAD_REQUEST");
        assert_eq!(err_code(q('s', 250, 250)), "NOT_FOUND");
    }

    // ── subscriptions ──────────────────────────────

    #[test]
    fn subscribe_unsubscribe_and_close_watch_track_the_session() {
        let state = make_state();
        let mut session = Session::default();

        let reply = ok(session.apply(
            &state,
            SoxOp::Subscribe {
                comp_ids: vec![A, B, MISSING],
            },
        ));
        assert_eq!(reply["count"], 3);
        // Only components that exist get a primed cache.
        assert_eq!(session.cov_cache.len(), 2);
        assert_eq!(session.cov_cache[&A].len(), 6);

        let reply = ok(session.apply(&state, SoxOp::Unsubscribe { comp_ids: vec![A] }));
        assert_eq!(reply["count"], 2);
        assert!(!session.subscribed.contains(&A));
        assert!(!session.cov_cache.contains_key(&A));

        let reply = ok(session.apply(
            &state,
            SoxOp::CloseWatch {
                watch_id: "w1".into(),
            },
        ));
        assert_eq!(reply["unsubscribed"], 2);
        assert!(session.subscribed.is_empty());
        assert!(session.cov_cache.is_empty());
        assert!(session.tree_cache.is_empty());
    }

    // ── tags ───────────────────────────────────────

    fn set_tags(comp_id: u16, tags: JsonValue) -> SoxOp {
        SoxOp::SetTags { comp_id, tags }
    }

    #[test]
    fn tags_round_trip_and_delete() {
        let state = make_state();
        ok(apply(
            &state,
            set_tags(A, json!({ "site": "roof", "n": 3 })),
        ));

        let tags = ok(apply(&state, SoxOp::ReadTags { comp_id: A }));
        assert_eq!(tags["tags"].as_object().unwrap().len(), 2);
        // Tags also show up in the component detail.
        let detail = ok(apply(&state, SoxOp::ReadComp { comp_id: A }));
        assert_eq!(detail["tags"].as_object().unwrap().len(), 2);

        let del = |key: &str| {
            ok(apply(
                &state,
                SoxOp::DeleteTag {
                    comp_id: A,
                    key: key.into(),
                },
            ))
        };
        assert_eq!(del("site")["removed"], true);
        assert_eq!(del("site")["removed"], false);
        let tags = ok(apply(&state, SoxOp::ReadTags { comp_id: A }));
        assert_eq!(tags["tags"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn set_tags_rejects_bad_input_and_keeps_nothing_of_a_failed_batch() {
        let state = make_state();
        assert_eq!(
            err_code(apply(&state, set_tags(A, json!([1, 2])))),
            "BAD_REQUEST"
        );
        assert_eq!(
            err_code(apply(&state, set_tags(A, json!("x")))),
            "BAD_REQUEST"
        );
        // One good and one bad name: nothing is stored.
        assert_eq!(
            err_code(apply(
                &state,
                set_tags(A, json!({ "good": 1, "Bad Name": 2 }))
            )),
            "BAD_REQUEST"
        );
        // Per-component limit is 4 in this test state.
        assert_eq!(
            err_code(apply(
                &state,
                set_tags(A, json!({ "a": 1, "b": 2, "c": 3, "d": 4, "e": 5 }))
            )),
            "LIMIT_EXCEEDED"
        );
        let tags = ok(apply(&state, SoxOp::ReadTags { comp_id: A }));
        assert!(tags["tags"].as_object().unwrap().is_empty());
    }

    #[test]
    fn tag_ops_without_a_store_are_not_supported() {
        let mut state = make_state();
        state.dyn_store = None;
        assert_eq!(
            err_code(apply(&state, SoxOp::ReadTags { comp_id: A })),
            "NOT_SUPPORTED"
        );
        assert_eq!(
            err_code(apply(&state, set_tags(A, json!({ "a": 1 })))),
            "NOT_SUPPORTED"
        );
        assert_eq!(
            err_code(apply(
                &state,
                SoxOp::DeleteTag {
                    comp_id: A,
                    key: "a".into(),
                },
            )),
            "NOT_SUPPORTED"
        );
    }

    #[test]
    fn set_format_accepts_json_and_trio_only() {
        let state = make_state();
        let f = |s: &str| apply(&state, SoxOp::SetFormat { format: s.into() });
        ok(f("json"));
        ok(f("trio"));
        assert_eq!(err_code(f("xml")), "BAD_REQUEST");
        assert_eq!(err_code(f("")), "BAD_REQUEST");
    }
}
