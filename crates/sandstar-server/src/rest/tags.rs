//! REST API endpoints for dynamic tags (DynSlotStore).
//!
//! These endpoints expose the side-car dynamic tag store over HTTP:
//!
//! - `GET  /api/tags`             — list all components with dynamic tags
//! - `GET  /api/tags/{comp_id}`   — get all tags for a component
//! - `GET  /api/tags/{comp_id}?computed=false` — exclude computed slots
//! - `PUT  /api/tags/{comp_id}`   — set tags (merge/replace)
//! - `DELETE /api/tags/{comp_id}/{key}` — remove a specific tag
//! - `GET  /api/tags/stats`       — interner and store statistics

use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::Router;
use serde::Deserialize;
use std::collections::HashMap;

use crate::sox::dyn_slots::DynValue;
use crate::sox::DynSlotStoreHandle;

/// Query parameters for GET /api/tags/{comp_id}.
#[derive(Debug, Deserialize)]
struct GetTagsQuery {
    /// Include computed (virtual) slot values. Default: true.
    computed: Option<bool>,
}

/// Convert a plain JSON value to DynValue automatically.
pub fn json_to_dynvalue(val: &serde_json::Value) -> DynValue {
    match val {
        serde_json::Value::Null => DynValue::Null,
        serde_json::Value::Bool(b) => DynValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                DynValue::Int(i)
            } else if let Some(f) = n.as_f64() {
                DynValue::Float(f)
            } else {
                DynValue::Null
            }
        }
        serde_json::Value::String(s) => DynValue::Str(s.clone()),
        _ => DynValue::Str(val.to_string()), // arrays/objects → JSON string
    }
}

/// Build the tags sub-router.
///
/// All routes are public (read) or protected (write) depending on how they
/// are merged into the main app. The caller is responsible for applying
/// auth middleware.
pub fn router(store: DynSlotStoreHandle) -> Router {
    Router::new()
        .route("/api/tags", get(list_tags))
        .route("/api/tags/stats", get(get_stats))
        .route("/api/tags/{comp_id}", get(get_tags).put(set_tags))
        .route("/api/tags/{comp_id}/{key}", delete(delete_tag))
        .with_state(store)
}

/// GET /api/tags — list all components that have dynamic tags.
///
/// Response: `{ "components": [{ "compId": 10, "tagCount": 3 }, ...] }`
async fn list_tags(State(store): State<DynSlotStoreHandle>) -> Response {
    let store = store.read().expect("dyn_slots lock poisoned");
    let mut comps: Vec<serde_json::Value> = store
        .comp_ids()
        .into_iter()
        .map(|id| {
            serde_json::json!({
                "compId": id,
                "tagCount": store.tag_count(id),
            })
        })
        .collect();
    // Sort by comp_id for deterministic output.
    comps.sort_by_key(|v| v["compId"].as_u64().unwrap_or(0));
    let body = serde_json::json!({
        "totalTags": store.total_count(),
        "components": comps,
    });
    Json(body).into_response()
}

/// GET /api/tags/{comp_id} — get all dynamic tags for a component.
///
/// Query params:
/// - `computed=false` — exclude computed (virtual) slot values
///
/// Response: `{ "compId": 10, "tags": { "devEUI": {"type":"Str","val":"A8..."}, ... } }`
async fn get_tags(
    State(store): State<DynSlotStoreHandle>,
    Path(comp_id): Path<u16>,
    Query(query): Query<GetTagsQuery>,
) -> Response {
    let include_computed = query.computed.unwrap_or(true);
    let store = store.read().expect("dyn_slots lock poisoned");

    if include_computed {
        let tags = store.get_all_with_computed(comp_id);
        let body = serde_json::json!({
            "compId": comp_id,
            "tags": tags,
        });
        Json(body).into_response()
    } else {
        match store.get_all(comp_id) {
            Some(tags) => {
                let body = serde_json::json!({
                    "compId": comp_id,
                    "tags": tags,
                });
                Json(body).into_response()
            }
            None => {
                let body = serde_json::json!({
                    "compId": comp_id,
                    "tags": {},
                });
                Json(body).into_response()
            }
        }
    }
}

/// GET /api/tags/stats — interner and store statistics.
///
/// Response: `{ "totalTags": 42, "components": 5, "interner": { ... } }`
async fn get_stats(State(store): State<DynSlotStoreHandle>) -> Response {
    let store = store.read().expect("dyn_slots lock poisoned");
    let stats = store.interner_stats();
    let body = serde_json::json!({
        "totalTags": store.total_count(),
        "components": store.comp_ids().len(),
        "interner": {
            "internedCount": stats.interned_count,
            "totalStringBytes": stats.total_string_bytes,
            "preInterned": stats.pre_interned,
        },
    });
    Json(body).into_response()
}

/// PUT /api/tags/{comp_id} — set dynamic tags for a component.
///
/// Request body: `{ "key1": {"type":"Str","val":"hello"}, "key2": {"type":"Marker"} }`
///
/// This merges tags: existing tags not in the request body are preserved.
/// To replace all tags, use `"_replace": true` in the body (removes tags not present).
async fn set_tags(
    State(store): State<DynSlotStoreHandle>,
    Path(comp_id): Path<u16>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let obj = match body.as_object() {
        Some(o) => o,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"err": "body must be a JSON object"})),
            )
                .into_response();
        }
    };

    // Check for _replace flag.
    let replace = obj
        .get("_replace")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Parse tags from the body (skip _replace key).
    // Accepts BOTH plain JSON values and tagged DynValue format:
    //   plain: {"key": "hello", "num": 42, "flag": true}
    //   tagged: {"key": {"type": "Str", "val": "hello"}}
    let mut new_tags: HashMap<String, DynValue> = HashMap::new();
    for (key, val) in obj {
        if key == "_replace" {
            continue;
        }
        // Try tagged format first, then auto-convert from plain JSON
        let dv = serde_json::from_value::<DynValue>(val.clone())
            .unwrap_or_else(|_| json_to_dynvalue(val));
        new_tags.insert(key.clone(), dv);
    }

    let mut store = store.write().expect("dyn_slots lock poisoned");

    if replace {
        // Full replacement.
        if let Err(e) = store.set_all(comp_id, new_tags) {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"err": e}))).into_response();
        }
    } else {
        // Merge: set each tag individually.
        for (key, val) in new_tags {
            if let Err(e) = store.set(comp_id, key, val) {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"err": e})))
                    .into_response();
            }
        }
    }

    let count = store.tag_count(comp_id);
    Json(serde_json::json!({
        "ok": true,
        "compId": comp_id,
        "tagCount": count,
    }))
    .into_response()
}

/// DELETE /api/tags/{comp_id}/{key} — remove a specific tag.
///
/// Response: `{ "ok": true, "removed": true }` or `{ "ok": true, "removed": false }`.
async fn delete_tag(
    State(store): State<DynSlotStoreHandle>,
    Path((comp_id, key)): Path<(u16, String)>,
) -> Response {
    let mut store = store.write().expect("dyn_slots lock poisoned");
    let removed = store.remove(comp_id, &key).is_some();
    Json(serde_json::json!({
        "ok": true,
        "compId": comp_id,
        "key": key,
        "removed": removed,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::{Arc, RwLock};
    use tower::ServiceExt;

    fn test_store() -> DynSlotStoreHandle {
        Arc::new(RwLock::new(
            crate::sox::dyn_slots::DynSlotStore::with_defaults(),
        ))
    }

    #[tokio::test]
    async fn list_tags_empty() {
        let app = router(test_store());
        let resp = app
            .oneshot(Request::get("/api/tags").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["totalTags"], 0);
        assert_eq!(body["components"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn put_and_get_tags() {
        let store = test_store();
        let app = router(store.clone());

        // PUT tags
        let body = serde_json::json!({
            "devEUI": {"type": "Str", "val": "A81758"},
            "rssi": {"type": "Int", "val": -72},
        });
        let resp = app
            .clone()
            .oneshot(
                Request::put("/api/tags/10")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["tagCount"], 2);

        // GET tags
        let resp = app
            .clone()
            .oneshot(Request::get("/api/tags/10").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["compId"], 10);
        assert_eq!(result["tags"]["devEUI"]["val"], "A81758");
        assert_eq!(result["tags"]["rssi"]["val"], -72);
    }

    #[tokio::test]
    async fn delete_tag_endpoint() {
        let store = test_store();
        {
            let mut s = store.write().unwrap();
            s.set(10, "foo".into(), DynValue::Marker).unwrap();
            s.set(10, "bar".into(), DynValue::Int(42)).unwrap();
        }
        let app = router(store.clone());

        let resp = app
            .clone()
            .oneshot(
                Request::delete("/api/tags/10/foo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["removed"], true);

        // Verify only "bar" remains
        let s = store.read().unwrap();
        assert_eq!(s.tag_count(10), 1);
        assert!(s.get(10, "foo").is_none());
        assert!(s.get(10, "bar").is_some());
    }

    #[tokio::test]
    async fn delete_nonexistent_tag() {
        let app = router(test_store());
        let resp = app
            .oneshot(
                Request::delete("/api/tags/10/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["removed"], false);
    }

    #[tokio::test]
    async fn put_tags_replace_mode() {
        let store = test_store();
        {
            let mut s = store.write().unwrap();
            s.set(10, "old".into(), DynValue::Marker).unwrap();
        }
        let app = router(store.clone());

        let body = serde_json::json!({
            "_replace": true,
            "new": {"type": "Marker"},
        });
        let resp = app
            .oneshot(
                Request::put("/api/tags/10")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let s = store.read().unwrap();
        assert!(s.get(10, "old").is_none());
        assert_eq!(s.get(10, "new"), Some(&DynValue::Marker));
    }

    #[tokio::test]
    async fn put_tags_plain_json_auto_converts() {
        let store = test_store();
        let app = router(store.clone());
        // Plain JSON values auto-convert: string→Str, number→Int/Float, bool→Bool
        let body = serde_json::json!({
            "name": "Wall Temp",
            "address": 40001,
            "active": true,
            "offset": 1.5,
        });
        let resp = app
            .oneshot(
                Request::put("/api/tags/10")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let s = store.read().unwrap();
        assert_eq!(s.get(10, "name"), Some(&DynValue::Str("Wall Temp".into())));
        assert_eq!(s.get(10, "address"), Some(&DynValue::Int(40001)));
        assert_eq!(s.get(10, "active"), Some(&DynValue::Bool(true)));
        assert_eq!(s.get(10, "offset"), Some(&DynValue::Float(1.5)));
    }

    #[tokio::test]
    async fn list_tags_after_puts() {
        let store = test_store();
        {
            let mut s = store.write().unwrap();
            s.set(10, "a".into(), DynValue::Marker).unwrap();
            s.set(20, "b".into(), DynValue::Marker).unwrap();
            s.set(20, "c".into(), DynValue::Int(1)).unwrap();
        }
        let app = router(store);

        let resp = app
            .oneshot(Request::get("/api/tags").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["totalTags"], 3);
        let comps = result["components"].as_array().unwrap();
        assert_eq!(comps.len(), 2);
    }

    #[tokio::test]
    async fn get_tags_nonexistent_comp() {
        let app = router(test_store());
        let resp = app
            .oneshot(Request::get("/api/tags/999").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["compId"], 999);
        assert_eq!(result["tags"].as_object().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn get_tags_includes_computed_by_default() {
        let store = test_store();
        {
            let mut s = store.write().unwrap();
            s.set(10, "dis".into(), DynValue::Str("Room Temp".into()))
                .unwrap();
            s.add_computed(
                10,
                crate::sox::dyn_slots::ComputedSlot {
                    name: "label".into(),
                    formula: crate::sox::dyn_slots::ComputedFormula::CopyTag("dis".into()),
                },
            );
        }
        let app = router(store);
        let resp = app
            .oneshot(Request::get("/api/tags/10").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        let tags = result["tags"].as_object().unwrap();
        assert_eq!(tags.len(), 2);
        // Both stored "dis" and computed "label" should be present.
        assert!(tags.contains_key("dis"));
        assert!(tags.contains_key("label"));
    }

    #[tokio::test]
    async fn get_tags_computed_false_excludes_computed() {
        let store = test_store();
        {
            let mut s = store.write().unwrap();
            s.set(10, "dis".into(), DynValue::Str("Room Temp".into()))
                .unwrap();
            s.add_computed(
                10,
                crate::sox::dyn_slots::ComputedSlot {
                    name: "label".into(),
                    formula: crate::sox::dyn_slots::ComputedFormula::CopyTag("dis".into()),
                },
            );
        }
        let app = router(store);
        let resp = app
            .oneshot(
                Request::get("/api/tags/10?computed=false")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        let tags = result["tags"].as_object().unwrap();
        assert_eq!(tags.len(), 1);
        assert!(tags.contains_key("dis"));
        assert!(!tags.contains_key("label"));
    }

    #[tokio::test]
    async fn get_stats_endpoint() {
        let store = test_store();
        {
            let mut s = store.write().unwrap();
            s.set(10, "dis".into(), DynValue::Str("Test".into()))
                .unwrap();
            s.set(20, "unit".into(), DynValue::Str("degF".into()))
                .unwrap();
        }
        let app = router(store);
        let resp = app
            .oneshot(Request::get("/api/tags/stats").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let result: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["totalTags"], 2);
        assert_eq!(result["components"], 2);
        // Interner stats should be present.
        let interner = &result["interner"];
        assert!(interner["internedCount"].as_u64().unwrap() >= 28);
        assert!(interner["totalStringBytes"].as_u64().unwrap() > 0);
        assert!(interner["preInterned"].as_u64().unwrap() >= 28);
    }
}
