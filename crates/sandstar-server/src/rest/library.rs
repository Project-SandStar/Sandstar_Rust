//! `/library/` and `/tags` — editor-compatibility stubs.
//!
//! The Vue web editor's `LibraryApi` wraps a SkySpark-hosted library of
//! user-authored component bundles (save/export/import ZIPs of re-usable
//! DDC snippets). That storage is out of scope for Sandstar MVP — we're
//! the engine, not a library server. The editor wraps every library call
//! in `try { ... } catch { return [] }`, so serving an empty list keeps
//! the UI functional: the "My Library" panel just renders empty.
//!
//! Same story for `/tags`: it feeds the editor's Haystack-tag autocomplete
//! picker; an empty list disables autocomplete without breaking anything
//! else. When Sandstar gains a proper tag registry we can populate this
//! from `DynSlotStoreHandle` without changing the wire contract.
//!
//! Phase 14.0G-5 (2026-04-23).

use axum::response::Json;
use axum::routing::get;
use axum::Router;
use serde_json::Value as JsonValue;

use crate::sox::sox_ops::SoxState;

/// Registers `/library/` (GET) and `/tags` (GET) with empty-list responses.
///
/// Mutating library operations (POST, PUT, DELETE on `/library/*`) are not
/// routed — the editor catches the 404 / 405 and disables those actions
/// gracefully.
pub fn router(state: SoxState) -> Router {
    Router::new()
        .route("/library/", get(list_libraries))
        .route("/tags", get(list_tags))
        .with_state(state)
}

async fn list_libraries(
    axum::extract::Query(_q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Vec<JsonValue>> {
    // Expected shape per LibraryApi.ts:37 — Library[] = [{navName, fileName, tags}].
    // We have no saved libraries in Sandstar native; editor falls back cleanly.
    Json(vec![])
}

async fn list_tags() -> Json<Vec<JsonValue>> {
    // Autocomplete list of Haystack tag names. Empty keeps the editor's
    // tag-picker UI responsive but featureless.
    Json(vec![])
}

// ── Tests ──────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn list_libraries_empty() {
        let q = axum::extract::Query(std::collections::HashMap::new());
        let Json(libs) = list_libraries(q).await;
        assert!(libs.is_empty());
    }

    #[tokio::test]
    async fn list_tags_empty() {
        let Json(tags) = list_tags().await;
        assert!(tags.is_empty());
    }

    #[allow(dead_code)]
    fn _sanity_check(s: SoxState) -> Router {
        router(s)
    }

    #[allow(dead_code)]
    fn _json_shape() -> JsonValue {
        json!({"navName": "x", "fileName": "x.json", "tags": []})
    }
}
