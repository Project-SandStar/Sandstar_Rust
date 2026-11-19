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

    let mut store = crate::lock::write_or_recover(dyn_store, "dyn slot store");
    for (key, val) in obj {
        let dv = serde_json::from_value::<DynValue>(val.clone())
            .unwrap_or_else(|_| crate::rest::tags::json_to_dynvalue(val));
        if let Err(e) = store.set(comp_id, key.clone(), dv) {
            return SoxResponse::error("LIMIT_EXCEEDED", e);
        }
    }
    SoxResponse::ok(serde_json::json!({ "ok": true }))
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

// ── Name validation ────────────────────────────────

fn validate_name(name: &str) -> Option<&'static str> {
    crate::sox::name_intern::NameInternTable::validate_name(name)
}
