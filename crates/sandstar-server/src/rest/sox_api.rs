//! REST API endpoints for the SOX component tree.
//!
//! Exposes the virtual component tree (normally only accessible via
//! the SOX/DASP UDP protocol) over HTTP for the DDC visual editor.
//!
//! Endpoints:
//! - `GET  /api/sox/tree`                — full component tree
//! - `GET  /api/sox/comp/{id}`           — single component with slots and links
//! - `POST /api/sox/comp`                — add a new component
//! - `DELETE /api/sox/comp/{id}`         — delete a component
//! - `PUT  /api/sox/comp/{id}/name`      — rename a component
//! - `PUT  /api/sox/comp/{id}/slot/{idx}` — write a slot value
//! - `POST /api/sox/comp/{id}/invoke/{slot}` — invoke an action slot
//! - `POST /api/sox/link`                — add a link
//! - `DELETE /api/sox/link`              — delete a link
//! - `PUT  /api/sox/comp/{id}/pos`       — update component position
//! - `GET  /api/sox/palette`             — available component types

use std::sync::Arc;

use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::sox::sox_handlers::{
    ComponentTree, ManifestDb, SlotValue, VirtualComponent, VirtualSlot,
    DEFAULT_KITS, SLOT_FLAG_ACTION, SLOT_FLAG_CONFIG,
};
use crate::sox::sox_protocol::SoxValueType;
use crate::sox::SharedComponentTree;

// ── State ───────────────────────────────────────────────────

/// Shared state passed to SOX API handlers.
#[derive(Clone)]
pub struct SoxApiState {
    pub tree: SharedComponentTree,
    pub manifest_db: Arc<ManifestDb>,
}

// ── JSON types ──────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TreeComponentJson {
    comp_id: u16,
    parent_id: u16,
    name: String,
    type_name: String,
    kit_id: u8,
    type_id: u8,
    children: Vec<u16>,
    position: (u8, u8),
    is_channel: bool,
    is_system: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SlotJson {
    index: usize,
    name: String,
    type_id: u8,
    type_name: &'static str,
    flags: u8,
    /// Convenience direction: "in" (config), "out" (runtime), "action", or "property".
    direction: &'static str,
    value: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LinkJson {
    from_comp: u16,
    from_slot: u8,
    to_comp: u16,
    to_slot: u8,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompDetailJson {
    comp_id: u16,
    parent_id: u16,
    name: String,
    type_name: String,
    kit_id: u8,
    type_id: u8,
    children: Vec<u16>,
    position: (u8, u8),
    is_channel: bool,
    is_system: bool,
    slots: Vec<SlotJson>,
    links: Vec<LinkJson>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCompRequest {
    parent_id: u16,
    kit_id: u8,
    type_id: u8,
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AddCompResponse {
    comp_id: u16,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameRequest {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotWriteRequest {
    value: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkRequest {
    from_comp: u16,
    from_slot: u8,
    to_comp: u16,
    to_slot: u8,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PosRequest {
    x: u8,
    y: u8,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PaletteEntry {
    kit_id: u8,
    type_id: u8,
    kit_name: String,
    type_name: String,
    category: &'static str,
    slots: Vec<PaletteSlotJson>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PaletteSlotJson {
    name: String,
    type_id: u8,
    type_name: &'static str,
    flags: u8,
    /// Convenience direction: "in" (config), "out" (runtime), "action", or "property".
    direction: &'static str,
}

// ── Helper functions ────────────────────────────────────────

/// Map a SOX value type ID to a human-readable name.
pub(crate) fn sox_type_name(type_id: u8) -> &'static str {
    match type_id {
        t if t == SoxValueType::Void as u8 => "void",
        t if t == SoxValueType::Bool as u8 => "bool",
        t if t == SoxValueType::Byte as u8 => "byte",
        t if t == SoxValueType::Short as u8 => "short",
        t if t == SoxValueType::Int as u8 => "int",
        t if t == SoxValueType::Long as u8 => "long",
        t if t == SoxValueType::Float as u8 => "float",
        t if t == SoxValueType::Double as u8 => "double",
        t if t == SoxValueType::Buf as u8 => "buf",
        _ => "unknown",
    }
}

/// Derive a convenience direction string from slot flags and name.
///
/// - Config (0x01) = "in" (user-configurable input)
/// - Action (0x02) = "action" (invocable)
/// - Runtime (0x04) — Sedona marks all linkable slots as runtime.
///   We classify them as "in" or "out" based on name conventions:
///   - **Outputs**: "out", "xgy", "xey", "xly", "div0", "status",
///     "raise", "lower" — these are computed results.
///   - **Inputs**: everything else (in, in1, in2, sel, x, y, cv,
///     up, dn, set, reset, etc.) — these are link targets.
/// - No flags (0x00) = "property" (read-only property)
pub(crate) fn slot_direction(flags: u8, name: &str) -> &'static str {
    if flags & SLOT_FLAG_ACTION != 0 {
        "action"
    } else if flags & SLOT_FLAG_CONFIG != 0 {
        "in"
    } else if flags & 0x04 != 0 {
        // SLOT_FLAG_RUNTIME — classify by known output names.
        match name {
            "out" | "xgy" | "xey" | "xly" | "div0" | "status" | "raise" | "lower" => "out",
            _ => "in",
        }
    } else {
        "property"
    }
}

/// Decode x,y position from the meta slot value.
///
/// Encoding: `0x01 | (x << 16) | (y << 24)` stored as an Int.
pub(crate) fn decode_position(meta: &SlotValue) -> (u8, u8) {
    match meta {
        SlotValue::Int(v) => {
            let bits = *v as u32;
            let x = ((bits >> 16) & 0xFF) as u8;
            let y = ((bits >> 24) & 0xFF) as u8;
            (x, y)
        }
        _ => (0, 0),
    }
}

/// Encode x,y position into a meta slot Int value.
pub(crate) fn encode_position(x: u8, y: u8) -> SlotValue {
    let bits: u32 = 0x01 | ((x as u32) << 16) | ((y as u32) << 24);
    SlotValue::Int(bits as i32)
}

/// Convert a SlotValue to a JSON value.
pub(crate) fn slot_value_to_json(value: &SlotValue) -> serde_json::Value {
    match value {
        SlotValue::Bool(v) => serde_json::Value::Bool(*v),
        SlotValue::Int(v) => serde_json::json!(*v),
        SlotValue::Long(v) => serde_json::json!(*v),
        SlotValue::Float(v) => {
            if v.is_finite() {
                serde_json::json!(*v)
            } else {
                serde_json::Value::Null
            }
        }
        SlotValue::Double(v) => {
            if v.is_finite() {
                serde_json::json!(*v)
            } else {
                serde_json::Value::Null
            }
        }
        SlotValue::Str(s) => serde_json::Value::String(s.clone()),
        SlotValue::Buf(b) => {
            let hex_str: String = b.iter().map(|byte| format!("{:02x}", byte)).collect();
            serde_json::json!(format!("0x{}", hex_str))
        }
        SlotValue::Null => serde_json::Value::Null,
    }
}

/// Coerce a JSON value to a typed SlotValue based on the slot's type_id.
pub(crate) fn json_to_slot_value(json: &serde_json::Value, type_id: u8) -> Option<SlotValue> {
    match type_id {
        t if t == SoxValueType::Bool as u8 => {
            json.as_bool().map(SlotValue::Bool)
        }
        t if t == SoxValueType::Int as u8 => {
            json.as_i64().map(|v| SlotValue::Int(v as i32))
        }
        t if t == SoxValueType::Long as u8 => {
            json.as_i64().map(SlotValue::Long)
        }
        t if t == SoxValueType::Float as u8 => {
            json.as_f64().map(|v| SlotValue::Float(v as f32))
        }
        t if t == SoxValueType::Double as u8 => {
            json.as_f64().map(SlotValue::Double)
        }
        t if t == SoxValueType::Buf as u8 => {
            json.as_str().map(|s| SlotValue::Str(s.to_string()))
        }
        // Byte / Short treated as Int
        t if t == SoxValueType::Byte as u8 || t == SoxValueType::Short as u8 => {
            json.as_i64().map(|v| SlotValue::Int(v as i32))
        }
        _ => None,
    }
}

/// Map a (kit_id, type_id) to a category name for the palette.
pub(crate) fn category_for_type(kit_id: u8, type_id: u8) -> &'static str {
    match (kit_id, type_id) {
        // Arithmetic
        (2, 3) | (2, 4) | (2, 49) | (2, 50) | (2, 37) | (2, 38) | (2, 18) => "Arithmetic",
        // Math
        (2, 39) | (2, 23) | (2, 34) | (2, 35) | (2, 32) | (2, 47) => "Math",
        // Logic
        (2, 5) | (2, 6) | (2, 42) | (2, 43) | (2, 40) | (2, 59) => "Logic",
        // Comparator
        (2, 12) => "Comparator",
        // Conversion
        (2, 10) | (2, 22) | (2, 26) => "Conversion",
        // Switch
        (2, 1) | (2, 11) | (2, 28) => "Switch",
        // Hysteresis
        (2, 25) | (2, 48) | (2, 46) => "Hysteresis",
        // Constant
        (2, 14) | (2, 15) | (2, 13) => "Constant",
        // Actuator
        (2, 57) | (2, 56) | (2, 58) => "Actuator",
        // Stateful
        (2, 19) | (2, 20) | (2, 16) | (2, 44) | (2, 54) | (2, 55) => "Stateful",
        // Sequencer
        (2, 31) => "Sequencer",
        // Sensor
        (1, 100) => "Sensor",
        // Default
        _ => "Other",
    }
}

/// Check if a component is a system component (id < 7).
pub(crate) fn is_system_comp(comp_id: u16) -> bool {
    comp_id < 7
}

/// Build a TreeComponentJson from a VirtualComponent.
fn comp_to_tree_json(comp: &VirtualComponent, tree: &ComponentTree) -> TreeComponentJson {
    let position = if let Some(meta_slot) = comp.slots.first() {
        decode_position(&meta_slot.value)
    } else {
        (0, 0)
    };
    TreeComponentJson {
        comp_id: comp.comp_id,
        parent_id: comp.parent_id,
        name: comp.name.clone(),
        type_name: comp.type_name.clone(),
        kit_id: comp.kit_id,
        type_id: comp.type_id,
        children: comp.children.clone(),
        position,
        is_channel: tree.is_channel_comp(comp.comp_id),
        is_system: is_system_comp(comp.comp_id),
    }
}

/// Build a CompDetailJson from a VirtualComponent.
fn comp_to_detail_json(comp: &VirtualComponent, tree: &ComponentTree) -> CompDetailJson {
    let position = if let Some(meta_slot) = comp.slots.first() {
        decode_position(&meta_slot.value)
    } else {
        (0, 0)
    };
    let slots: Vec<SlotJson> = comp
        .slots
        .iter()
        .enumerate()
        .map(|(idx, slot)| SlotJson {
            index: idx,
            name: slot.name.clone(),
            type_id: slot.type_id,
            type_name: sox_type_name(slot.type_id),
            flags: slot.flags,
            direction: slot_direction(slot.flags, &slot.name),
            value: slot_value_to_json(&slot.value),
        })
        .collect();
    let links: Vec<LinkJson> = comp
        .links
        .iter()
        .map(|l| LinkJson {
            from_comp: l.from_comp,
            from_slot: l.from_slot,
            to_comp: l.to_comp,
            to_slot: l.to_slot,
        })
        .collect();
    CompDetailJson {
        comp_id: comp.comp_id,
        parent_id: comp.parent_id,
        name: comp.name.clone(),
        type_name: comp.type_name.clone(),
        kit_id: comp.kit_id,
        type_id: comp.type_id,
        children: comp.children.clone(),
        position,
        is_channel: tree.is_channel_comp(comp.comp_id),
        is_system: is_system_comp(comp.comp_id),
        slots,
        links,
    }
}

// ── Handlers ────────────────────────────────────────────────

/// GET /api/sox/tree — full component tree.
///
/// Returns `{ "components": [...] }` so the editor can destructure easily.
pub async fn get_tree(State(state): State<SoxApiState>) -> Response {
    let tree = state.tree.read().unwrap();
    let mut comps: Vec<TreeComponentJson> = tree
        .comp_ids()
        .into_iter()
        .filter_map(|id| tree.get(id).map(|c| comp_to_tree_json(c, &tree)))
        .collect();
    // Sort by comp_id for stable output.
    comps.sort_by_key(|c| c.comp_id);
    Json(serde_json::json!({ "components": comps })).into_response()
}

/// GET /api/sox/comp/{id} — single component detail.
pub async fn get_comp(
    State(state): State<SoxApiState>,
    Path(id): Path<u16>,
) -> Response {
    let tree = state.tree.read().unwrap();
    match tree.get(id) {
        Some(comp) => Json(comp_to_detail_json(comp, &tree)).into_response(),
        None => (StatusCode::NOT_FOUND, format!("component {id} not found")).into_response(),
    }
}

/// POST /api/sox/comp — add a new component.
pub async fn add_comp(
    State(state): State<SoxApiState>,
    Json(req): Json<AddCompRequest>,
) -> Response {
    let mut tree = state.tree.write().unwrap();

    // Validate parent exists.
    if tree.get(req.parent_id).is_none() {
        return (StatusCode::BAD_REQUEST, format!("parent {} not found", req.parent_id))
            .into_response();
    }

    // Resolve slots from manifest_db or use empty.
    let slots = match state.manifest_db.get_slots(req.kit_id, req.type_id) {
        Some(manifest_slots) => ManifestDb::slots_to_virtual(manifest_slots),
        None => {
            // Minimal: just a meta slot.
            vec![VirtualSlot {
                name: "meta".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(1),
            }]
        }
    };

    // Resolve type name.
    let type_name = state
        .manifest_db
        .type_name(req.kit_id, req.type_id)
        .unwrap_or_else(|| format!("Type_{}_{}", req.kit_id, req.type_id));

    let comp_id = tree.next_comp_id();
    let comp = VirtualComponent {
        comp_id,
        parent_id: req.parent_id,
        name: req.name,
        type_name,
        kit_id: req.kit_id,
        type_id: req.type_id,
        children: Vec::new(),
        slots,
        links: Vec::new(),
    };

    tree.add(comp);
    tree.mark_user_added(comp_id);
    tree.mark_dirty();

    (StatusCode::CREATED, Json(AddCompResponse { comp_id })).into_response()
}

/// DELETE /api/sox/comp/{id} — delete a component.
pub async fn delete_comp(
    State(state): State<SoxApiState>,
    Path(id): Path<u16>,
) -> Response {
    if id < 7 {
        return (StatusCode::FORBIDDEN, "cannot delete system components (id < 7)")
            .into_response();
    }

    let mut tree = state.tree.write().unwrap();

    // Remove all links involving this component from other components.
    let link_peers: Vec<(u16, u8, u16, u8)> = tree
        .get(id)
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

    match tree.remove(id) {
        Some(_) => {
            tree.mark_dirty();
            StatusCode::NO_CONTENT.into_response()
        }
        None => (StatusCode::NOT_FOUND, format!("component {id} not found")).into_response(),
    }
}

/// Max component name length for Sedona editor compatibility.
const MAX_NAME_LEN: usize = 31;

/// PUT /api/sox/comp/{id}/name — rename a component.
pub async fn rename_comp(
    State(state): State<SoxApiState>,
    Path(id): Path<u16>,
    Json(req): Json<RenameRequest>,
) -> Response {
    // Validate name for Sedona compatibility
    if req.name.is_empty() {
        return (StatusCode::BAD_REQUEST, "name cannot be empty").into_response();
    }
    if req.name.len() > MAX_NAME_LEN {
        return (StatusCode::BAD_REQUEST, format!("name too long (max {MAX_NAME_LEN} chars)")).into_response();
    }
    if !req.name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return (StatusCode::BAD_REQUEST, "name must start with a letter").into_response();
    }
    if !req.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return (StatusCode::BAD_REQUEST, "name can only contain letters, numbers, and underscores").into_response();
    }
    let mut tree = state.tree.write().unwrap();
    if tree.rename(id, req.name) {
        tree.mark_dirty();
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, format!("component {id} not found")).into_response()
    }
}

/// PUT /api/sox/comp/{id}/slot/{idx} — write a slot value.
pub async fn write_slot(
    State(state): State<SoxApiState>,
    Path((id, idx)): Path<(u16, usize)>,
    Json(req): Json<SlotWriteRequest>,
) -> Response {
    let mut tree = state.tree.write().unwrap();
    let slot_type_id = {
        let comp = match tree.get(id) {
            Some(c) => c,
            None => {
                return (StatusCode::NOT_FOUND, format!("component {id} not found"))
                    .into_response()
            }
        };
        match comp.slots.get(idx) {
            Some(slot) => slot.type_id,
            None => {
                return (StatusCode::BAD_REQUEST, format!("slot index {idx} out of range"))
                    .into_response()
            }
        }
    };

    let new_value = match json_to_slot_value(&req.value, slot_type_id) {
        Some(v) => v,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                format!("cannot coerce value to type_id {slot_type_id}"),
            )
                .into_response()
        }
    };

    if let Some(comp) = tree.get_mut(id) {
        if let Some(slot) = comp.slots.get_mut(idx) {
            slot.value = new_value;
        }
    }
    tree.mark_dirty();
    StatusCode::NO_CONTENT.into_response()
}

/// POST /api/sox/comp/{id}/invoke/{slot} — invoke an action slot.
///
/// For action slots like "set", "setTrue", "setFalse":
/// - setTrue: set the bool slot to true
/// - setFalse: set the bool slot to false
/// - set: set to the value from request body (or toggle bool)
pub async fn invoke_action(
    State(state): State<SoxApiState>,
    Path((id, slot_name)): Path<(u16, String)>,
    body: Option<Json<SlotWriteRequest>>,
) -> Response {
    let mut tree = state.tree.write().unwrap();

    let comp = match tree.get_mut(id) {
        Some(c) => c,
        None => {
            return (StatusCode::NOT_FOUND, format!("component {id} not found")).into_response()
        }
    };

    // Find the slot by name.
    let slot_idx = comp
        .slots
        .iter()
        .position(|s| s.name == slot_name);
    let slot_idx = match slot_idx {
        Some(i) => i,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                format!("slot '{}' not found on component {id}", slot_name),
            )
                .into_response()
        }
    };

    // Apply action.
    match slot_name.as_str() {
        "setTrue" => {
            // Find the target bool slot (usually the first runtime bool).
            for slot in &mut comp.slots {
                if slot.type_id == SoxValueType::Bool as u8
                    && slot.flags & SLOT_FLAG_ACTION == 0
                {
                    slot.value = SlotValue::Bool(true);
                    break;
                }
            }
        }
        "setFalse" => {
            for slot in &mut comp.slots {
                if slot.type_id == SoxValueType::Bool as u8
                    && slot.flags & SLOT_FLAG_ACTION == 0
                {
                    slot.value = SlotValue::Bool(false);
                    break;
                }
            }
        }
        _ => {
            // Generic action: if body provided, write value to the slot.
            if let Some(Json(req)) = body {
                let type_id = comp.slots[slot_idx].type_id;
                if let Some(val) = json_to_slot_value(&req.value, type_id) {
                    comp.slots[slot_idx].value = val;
                }
            }
        }
    }

    tree.mark_dirty();
    StatusCode::NO_CONTENT.into_response()
}

/// POST /api/sox/link — add a link.
pub async fn add_link(
    State(state): State<SoxApiState>,
    Json(req): Json<LinkRequest>,
) -> Response {
    let mut tree = state.tree.write().unwrap();
    if tree.add_link(req.from_comp, req.from_slot, req.to_comp, req.to_slot) {
        tree.mark_dirty();
        StatusCode::CREATED.into_response()
    } else {
        (
            StatusCode::CONFLICT,
            "link already exists or would create a cycle",
        )
            .into_response()
    }
}

/// DELETE /api/sox/link — delete a link.
pub async fn delete_link(
    State(state): State<SoxApiState>,
    Json(req): Json<LinkRequest>,
) -> Response {
    let mut tree = state.tree.write().unwrap();
    if tree.remove_link(req.from_comp, req.from_slot, req.to_comp, req.to_slot) {
        tree.mark_dirty();
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "link not found").into_response()
    }
}

/// PUT /api/sox/comp/{id}/pos — update component position.
pub async fn update_pos(
    State(state): State<SoxApiState>,
    Path(id): Path<u16>,
    Json(req): Json<PosRequest>,
) -> Response {
    let mut tree = state.tree.write().unwrap();
    let comp = match tree.get_mut(id) {
        Some(c) => c,
        None => {
            return (StatusCode::NOT_FOUND, format!("component {id} not found")).into_response()
        }
    };

    // Meta slot is always slot 0.
    if comp.slots.is_empty() {
        return (StatusCode::BAD_REQUEST, "component has no meta slot").into_response();
    }
    comp.slots[0].value = encode_position(req.x, req.y);
    tree.mark_dirty();
    StatusCode::NO_CONTENT.into_response()
}

/// GET /api/sox/palette — available component types.
pub async fn get_palette(State(state): State<SoxApiState>) -> Response {
    let mut entries: Vec<PaletteEntry> = state
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
            let slot_list: Vec<PaletteSlotJson> = slots
                .iter()
                .map(|s| PaletteSlotJson {
                    name: s.name.clone(),
                    type_id: s.type_id,
                    type_name: sox_type_name(s.type_id),
                    flags: s.flags,
                    direction: slot_direction(s.flags, &s.name),
                })
                .collect();
            PaletteEntry {
                kit_id,
                type_id,
                kit_name,
                type_name,
                category,
                slots: slot_list,
            }
        })
        .collect();
    // Sort for stable output.
    entries.sort_by(|a, b| (a.kit_id, a.type_id).cmp(&(b.kit_id, b.type_id)));
    Json(entries).into_response()
}

/// Build the SOX API sub-router (public read + protected write routes).
///
/// The caller is responsible for merging these into the main app
/// and applying auth middleware to the write routes.
pub fn public_router(state: SoxApiState) -> axum::Router {
    use axum::routing::get;
    axum::Router::new()
        .route("/api/sox/tree", get(get_tree))
        .route("/api/sox/comp/{id}", get(get_comp))
        .route("/api/sox/palette", get(get_palette))
        .with_state(state)
}

pub fn protected_router(state: SoxApiState) -> axum::Router {
    use axum::routing::{delete, post, put};
    axum::Router::new()
        .route("/api/sox/comp", post(add_comp))
        .route("/api/sox/comp/{id}", delete(delete_comp))
        .route("/api/sox/comp/{id}/name", put(rename_comp))
        .route("/api/sox/comp/{id}/slot/{idx}", put(write_slot))
        .route("/api/sox/comp/{id}/invoke/{slot}", post(invoke_action))
        .route("/api/sox/link", post(add_link).delete(delete_link))
        .route("/api/sox/comp/{id}/pos", put(update_pos))
        .with_state(state)
}
