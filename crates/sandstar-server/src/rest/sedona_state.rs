//! `/live/sedonaState` — bulk tree loader.
//!
//! Replaces the Fantom pod's `App.save` + `.sab` download + server-side
//! OfflineApp parsing with a direct dump of the live `ComponentTree`.
//! The editor's `SedonaStateService.fetchSedonaState` consumes this
//! payload shape:
//!
//! ```json
//! {
//!   "schema":   { "kits": [{ "id", "name", "checksum" }] },
//!   "components": { "<id>": { id, name, type, kitId, typeId,
//!                              parentId, childrenIds, props, meta,
//!                              position: { x, y, groups } } },
//!   "links":    [{ fromCompId, fromSlotId, fromSlotName,
//!                  toCompId,   toSlotId,   toSlotName }],
//!   "maxId":     u16,
//!   "host":      string,
//!   "port":      u16,
//!   "timestamp": u64 (ms since epoch),
//!   "loadTimeMs": u64
//! }
//! ```
//!
//! Per-slot `value` is base64-encoded raw SOX slot bytes — same encoding
//! as `/live`'s `compProps.propValues`, so the editor's Sedona-aware
//! decoder handles both transports uniformly.
//!
//! Phase 14.0G-3 (2026-04-23).

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Json};
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use tracing::debug;

use crate::sox::sox_handlers::{ComponentTree, SlotValue, VirtualComponent, DEFAULT_KITS};
use crate::sox::sox_ops::SoxState;

use super::sox_api::decode_position;

// ── Query params ───────────────────────────────────

/// Query parameters accepted on `GET /live/sedonaState`. All are optional;
/// `h` and `p` echo back into the response so a client talking to multiple
/// devices can route the payload. `save_first` is accepted for Fantom
/// parity and is a no-op on native Sandstar — the live tree is always the
/// current RAM state.
#[derive(Debug, Deserialize)]
pub struct SedonaStateQuery {
    #[serde(default)]
    pub h: Option<String>,
    #[serde(default)]
    pub p: Option<u16>,
    #[serde(default)]
    #[allow(dead_code)]
    pub u: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub pw: Option<String>,
    #[serde(default, rename = "saveFirst")]
    #[allow(dead_code)]
    pub save_first: Option<String>,
}

// ── Handler ────────────────────────────────────────

pub async fn sedona_state_handler(
    State(state): State<SoxState>,
    Query(q): Query<SedonaStateQuery>,
) -> impl IntoResponse {
    let started = Instant::now();
    let tree = crate::lock::read_or_recover(&state.tree, "SOX component tree");
    let snapshot = build_snapshot(&tree, &q, started);
    Json(snapshot)
}

// ── Snapshot builder ───────────────────────────────

fn build_snapshot(tree: &ComponentTree, q: &SedonaStateQuery, started: Instant) -> JsonValue {
    // Schema: kit table (kit id here is the DEFAULT_KITS index, matching
    // how /live's `version` op reports them).
    let kits: Vec<JsonValue> = DEFAULT_KITS
        .iter()
        .enumerate()
        .map(|(idx, k)| {
            json!({
                "id": idx,
                "name": k.name,
                "checksum": k.checksum,
            })
        })
        .collect();

    // Components map keyed by compId stringified.
    let mut components_map = serde_json::Map::new();
    let mut max_id: u16 = 0;
    let all_ids = tree.comp_ids();

    for &comp_id in &all_ids {
        if comp_id > max_id {
            max_id = comp_id;
        }
        let Some(comp) = tree.get(comp_id) else {
            continue;
        };
        components_map.insert(comp_id.to_string(), component_to_json(comp));
    }

    // Links: each link is owned by both endpoints' `links` lists, so
    // emit once per link — only when we see it on the from-comp side.
    let mut links: Vec<JsonValue> = Vec::new();
    for &comp_id in &all_ids {
        let Some(comp) = tree.get(comp_id) else {
            continue;
        };
        for link in &comp.links {
            if link.from_comp != comp_id {
                continue;
            }
            let from_slot_name = tree
                .get(link.from_comp)
                .and_then(|c| c.slots.get(link.from_slot as usize))
                .map(|s| s.name.clone())
                .unwrap_or_default();
            let to_slot_name = tree
                .get(link.to_comp)
                .and_then(|c| c.slots.get(link.to_slot as usize))
                .map(|s| s.name.clone())
                .unwrap_or_default();
            links.push(json!({
                "fromCompId": link.from_comp,
                "fromSlotId": link.from_slot,
                "fromSlotName": from_slot_name,
                "toCompId": link.to_comp,
                "toSlotId": link.to_slot,
                "toSlotName": to_slot_name,
            }));
        }
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let host = q.h.clone().unwrap_or_default();
    let port = q.p.unwrap_or(1876);

    debug!(
        components = components_map.len(),
        links = links.len(),
        elapsed_ms,
        "/live/sedonaState served"
    );

    json!({
        "schema": { "kits": kits },
        "components": components_map,
        "links": links,
        "maxId": max_id,
        "host": host,
        "port": port,
        "timestamp": now_ms,
        "loadTimeMs": elapsed_ms,
    })
}

/// Serialise a single component into the editor's expected shape.
fn component_to_json(comp: &VirtualComponent) -> JsonValue {
    let kit_name = DEFAULT_KITS
        .get(comp.kit_id as usize)
        .map(|k| k.name.to_string())
        .unwrap_or_else(|| format!("kit{}", comp.kit_id));
    // `comp.type_name` from the manifest is sometimes already namespaced
    // (e.g. "sys::App"). Don't prefix twice — only prepend the kit if the
    // stored type name is a bare identifier.
    let qname = if comp.type_name.contains("::") {
        comp.type_name.clone()
    } else {
        format!("{}::{}", kit_name, comp.type_name)
    };

    // Build per-slot props dictionary. Each entry carries the slot's
    // position index (`id`), a text-encoded value (`value`), and the
    // Sedona type id (`typeId`) — matching the shape Fantom pod's
    // OfflineApp emits per slot (`val.encodeString` → plain text like
    // `"42"`, `"hello"`, `"true"`; NOT base64 binary).
    //
    // The editor consumes this via `ValueUtils.makeSome(typeId, value)`
    // which calls `Int.make(val)` etc. — those expect native JS primitives
    // coerced from the text form. Emitting base64 binary breaks the decode
    // and makes the editor fall back to readComp (which then crashes on
    // concatenated-buffer misalignment). See Phase 14.0G end-to-end bring-up
    // notes 2026-04-24.
    let mut props_map = serde_json::Map::with_capacity(comp.slots.len());
    for (idx, slot) in comp.slots.iter().enumerate() {
        props_map.insert(
            slot.name.clone(),
            json!({
                "id": idx,
                "value": slot_value_to_text(&slot.value),
                "typeId": slot.type_id,
            }),
        );
    }

    // Meta int + derived position. Convention matches `encode_position`
    // in `sox_api.rs`: bits 16-23 = x, bits 24-31 = y, bits 8-15 reserved
    // for groups (0 today — never encoded by the engine, editor tolerates 0).
    let (meta_int, position_x, position_y, groups) = match comp.slots.first() {
        Some(slot) => match &slot.value {
            SlotValue::Int(v) => {
                let bits = *v as u32;
                let (x, y) = decode_position(&slot.value);
                let groups = ((bits >> 8) & 0xFF) as u8;
                (*v, x, y, groups)
            }
            _ => {
                let (x, y) = decode_position(&slot.value);
                (0, x, y, 0)
            }
        },
        None => (0, 0, 0, 0),
    };

    json!({
        "id": comp.comp_id,
        "name": comp.name,
        "type": qname,
        "kitId": comp.kit_id,
        "typeId": comp.type_id,
        "parentId": comp.parent_id,
        "childrenIds": comp.children,
        "props": props_map,
        "meta": meta_int,
        "position": {
            "x": position_x,
            "y": position_y,
            "groups": groups,
        },
    })
}

/// Render a slot value as the text form the editor's `ValueUtils.makeSome`
/// expects (matches Fantom pod's `SedonaValue.encodeString`):
/// - Bool → `"true"` / `"false"`
/// - Int/Long → decimal integer string
/// - Float/Double → decimal float string
/// - Str/Buf → raw text (Str as UTF-8 string; Buf as lossy UTF-8 or hex if not text)
/// - Null → empty string
fn slot_value_to_text(value: &SlotValue) -> String {
    match value {
        SlotValue::Bool(b) => {
            if *b {
                "true".into()
            } else {
                "false".into()
            }
        }
        SlotValue::Int(v) => v.to_string(),
        SlotValue::Long(v) => v.to_string(),
        SlotValue::Float(v) => v.to_string(),
        SlotValue::Double(v) => v.to_string(),
        SlotValue::Str(s) => s.clone(),
        SlotValue::Buf(bytes) => {
            // Sedona Buf is almost always printable text (used like a string).
            // Try UTF-8 first; fall back to lossy (replacement char) so we
            // never panic on odd bytes.
            match std::str::from_utf8(bytes) {
                Ok(s) => s.trim_end_matches('\0').to_string(),
                Err(_) => String::from_utf8_lossy(bytes)
                    .trim_end_matches('\0')
                    .to_string(),
            }
        }
        SlotValue::Null => String::new(),
    }
}

// ── Tests ──────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sox::sox_handlers::{
        ComponentTree, Link, ManifestDb, VirtualComponent, VirtualSlot,
    };
    use crate::sox::sox_protocol::SoxValueType;
    use std::sync::Arc;

    fn test_tree() -> ComponentTree {
        let manifest_db = Arc::new(ManifestDb::load(""));
        let mut tree = ComponentTree::new_with_manifest(manifest_db);

        let root = VirtualComponent {
            comp_id: 1,
            parent_id: 0,
            name: "app".into(),
            type_name: "App".into(),
            kit_id: 0, // sys
            type_id: 0,
            children: vec![2, 3],
            slots: vec![VirtualSlot {
                name: "meta".into(),
                type_id: SoxValueType::Int as u8,
                flags: 0,
                value: SlotValue::Int(1),
            }],
            links: vec![],
        };
        let a = VirtualComponent {
            comp_id: 2,
            parent_id: 1,
            name: "constA".into(),
            type_name: "ConstFloat".into(),
            kit_id: 5, // control (kit idx may vary)
            type_id: 3,
            children: vec![],
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: SoxValueType::Int as u8,
                    flags: 0,
                    value: SlotValue::Int(0x01_20_10_01),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: SoxValueType::Float as u8,
                    flags: 0,
                    value: SlotValue::Float(42.0),
                },
            ],
            links: vec![Link {
                from_comp: 2,
                from_slot: 1,
                to_comp: 3,
                to_slot: 1,
            }],
        };
        let b = VirtualComponent {
            comp_id: 3,
            parent_id: 1,
            name: "add2".into(),
            type_name: "Add2".into(),
            kit_id: 5,
            type_id: 4,
            children: vec![],
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: SoxValueType::Int as u8,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "in1".into(),
                    type_id: SoxValueType::Float as u8,
                    flags: 0,
                    value: SlotValue::Float(0.0),
                },
            ],
            links: vec![Link {
                from_comp: 2,
                from_slot: 1,
                to_comp: 3,
                to_slot: 1,
            }],
        };
        tree.add(root);
        tree.add(a);
        tree.add(b);
        tree
    }

    #[test]
    fn snapshot_has_schema_kits() {
        let tree = test_tree();
        let q = SedonaStateQuery {
            h: Some("localhost".into()),
            p: Some(1876),
            u: None,
            pw: None,
            save_first: None,
        };
        let snap = build_snapshot(&tree, &q, Instant::now());
        assert!(snap["schema"]["kits"].is_array());
        assert!(snap["schema"]["kits"].as_array().unwrap().len() > 0);
        // Every kit entry carries id + name + checksum.
        let first = &snap["schema"]["kits"][0];
        assert!(first["id"].is_u64());
        assert!(first["name"].is_string());
        assert!(first["checksum"].is_u64());
    }

    #[test]
    fn snapshot_contains_all_components_keyed_by_id() {
        let tree = test_tree();
        let q = SedonaStateQuery {
            h: None,
            p: None,
            u: None,
            pw: None,
            save_first: None,
        };
        let snap = build_snapshot(&tree, &q, Instant::now());
        let comps = snap["components"].as_object().unwrap();
        assert_eq!(comps.len(), 3);
        assert!(comps.contains_key("1"));
        assert!(comps.contains_key("2"));
        assert!(comps.contains_key("3"));
    }

    #[test]
    fn component_has_qname_and_props() {
        let tree = test_tree();
        let q = SedonaStateQuery {
            h: None,
            p: None,
            u: None,
            pw: None,
            save_first: None,
        };
        let snap = build_snapshot(&tree, &q, Instant::now());
        let c2 = &snap["components"]["2"];
        assert_eq!(c2["id"], 2);
        assert_eq!(c2["name"], "constA");
        assert!(c2["type"].as_str().unwrap().ends_with("::ConstFloat"));
        let props = c2["props"].as_object().unwrap();
        assert!(props.contains_key("meta"));
        assert!(props.contains_key("out"));
        let out = &props["out"];
        assert_eq!(out["id"], 1);
        // Post-2026-04-24: value is text-encoded (Fantom `encodeString`
        // convention consumed by the editor's `ValueUtils.makeSome`).
        // Float 42.0 → "42".
        let v = out["value"].as_str().unwrap();
        assert_eq!(v, "42");
    }

    #[test]
    fn position_decoded_from_meta() {
        let tree = test_tree();
        let q = SedonaStateQuery {
            h: None,
            p: None,
            u: None,
            pw: None,
            save_first: None,
        };
        let snap = build_snapshot(&tree, &q, Instant::now());
        // comp 2's meta was 0x01_20_10_01 (little-endian per-byte view):
        //   byte 0 (bits  0- 7) = 0x01 → flag
        //   byte 1 (bits  8-15) = 0x10 → groups = 16
        //   byte 2 (bits 16-23) = 0x20 → x = 32
        //   byte 3 (bits 24-31) = 0x01 → y = 1
        let pos = &snap["components"]["2"]["position"];
        assert_eq!(pos["x"], 32);
        assert_eq!(pos["y"], 1);
        assert_eq!(pos["groups"], 16);
    }

    #[test]
    fn links_emitted_once_with_slot_names() {
        let tree = test_tree();
        let q = SedonaStateQuery {
            h: None,
            p: None,
            u: None,
            pw: None,
            save_first: None,
        };
        let snap = build_snapshot(&tree, &q, Instant::now());
        let links = snap["links"].as_array().unwrap();
        // The same Link object is in both comp 2's and comp 3's list, but
        // we should see it exactly once here (emitted from the from-side).
        assert_eq!(links.len(), 1);
        let l = &links[0];
        assert_eq!(l["fromCompId"], 2);
        assert_eq!(l["toCompId"], 3);
        assert_eq!(l["fromSlotName"], "out");
        assert_eq!(l["toSlotName"], "in1");
    }

    #[test]
    fn snapshot_echoes_host_and_port() {
        let tree = test_tree();
        let q = SedonaStateQuery {
            h: Some("192.168.1.4".into()),
            p: Some(1876),
            u: None,
            pw: None,
            save_first: Some("true".into()),
        };
        let snap = build_snapshot(&tree, &q, Instant::now());
        assert_eq!(snap["host"], "192.168.1.4");
        assert_eq!(snap["port"], 1876);
        assert_eq!(snap["maxId"], 3);
        assert!(snap["timestamp"].as_u64().unwrap() > 0);
    }
}
