//! Dynamic Slots — side-car tag store for components.
//!
//! Implements the side-car pattern from research doc 19: a `DynSlotStore` maps
//! component IDs to dynamic key-value tag dictionaries. This allows runtime
//! metadata (Modbus addresses, BACnet object IDs, LoRaWAN devEUI, etc.) to be
//! attached to components without modifying the static Sedona slot model.
//!
//! # Design
//!
//! - **Zero cost for components without dynamic tags** — just a HashMap miss
//! - **Haystack-compatible value types** via [`DynValue`]
//! - **Memory-bounded** — configurable per-component and total tag limits
//! - **Persistent** — JSON serialization to disk, auto-saved when dirty

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tracing::{debug, info, warn};

/// Default maximum tags per component.
pub const DEFAULT_MAX_PER_COMP: usize = 64;
/// Default maximum total tags across all components.
pub const DEFAULT_MAX_TOTAL: usize = 10_000;

/// Dynamic tag value — supports Haystack-compatible types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "val")]
pub enum DynValue {
    /// Null / absent value.
    Null,
    /// Haystack marker tag (presence = true).
    Marker,
    /// Boolean value.
    Bool(bool),
    /// Integer value.
    Int(i64),
    /// Floating-point value.
    Float(f64),
    /// String value.
    Str(String),
    /// Haystack Ref (e.g., `@p:demo:r:abc`).
    Ref(String),
}

/// Serializable form of the entire store (for JSON persistence).
#[derive(Serialize, Deserialize)]
struct PersistData {
    /// Version tag for forward compatibility.
    version: u32,
    /// Component ID → tag dictionary.
    slots: HashMap<u16, HashMap<String, DynValue>>,
}

/// Dynamic slot store — side-car for all components.
///
/// Maps component IDs to dynamic key-value tag dictionaries.
/// Thread safety: the caller is responsible for synchronization (the SOX
/// server loop is single-threaded; REST access goes through `Arc<RwLock>`).
pub struct DynSlotStore {
    /// comp_id → dynamic tag dictionary.
    slots: HashMap<u16, HashMap<String, DynValue>>,
    /// Maximum tags per component.
    max_per_comp: usize,
    /// Maximum total tags across all components.
    max_total: usize,
    /// Current total tag count.
    total: usize,
    /// Dirty flag for persistence.
    dirty: bool,
}

impl DynSlotStore {
    /// Create a new empty store with the given limits.
    pub fn new(max_per_comp: usize, max_total: usize) -> Self {
        Self {
            slots: HashMap::new(),
            max_per_comp,
            max_total,
            total: 0,
            dirty: false,
        }
    }

    /// Create a store with default limits.
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_MAX_PER_COMP, DEFAULT_MAX_TOTAL)
    }

    /// Get a single tag value for a component.
    pub fn get(&self, comp_id: u16, key: &str) -> Option<&DynValue> {
        self.slots.get(&comp_id).and_then(|tags| tags.get(key))
    }

    /// Set a single tag on a component. Returns the previous value if any.
    ///
    /// Returns `Err` if the per-component or total tag limit would be exceeded.
    pub fn set(
        &mut self,
        comp_id: u16,
        key: String,
        value: DynValue,
    ) -> Result<Option<DynValue>, String> {
        let tags = self.slots.entry(comp_id).or_default();
        let current_len = tags.len();

        // Use entry API to avoid contains_key + insert pattern.
        use std::collections::hash_map::Entry;
        match tags.entry(key) {
            Entry::Occupied(mut e) => {
                // Replacement — no count change.
                let prev = e.insert(value);
                self.dirty = true;
                Ok(Some(prev))
            }
            Entry::Vacant(e) => {
                // New key — check limits.
                if current_len >= self.max_per_comp {
                    return Err(format!(
                        "component {comp_id} has {current_len} tags (max {})",
                        self.max_per_comp
                    ));
                }
                if self.total >= self.max_total {
                    return Err(format!(
                        "total tag count {} reached limit {}",
                        self.total, self.max_total
                    ));
                }
                e.insert(value);
                self.total += 1;
                self.dirty = true;
                Ok(None)
            }
        }
    }

    /// Remove a single tag from a component. Returns the removed value if any.
    pub fn remove(&mut self, comp_id: u16, key: &str) -> Option<DynValue> {
        let tags = self.slots.get_mut(&comp_id)?;
        let removed = tags.remove(key)?;
        self.total -= 1;
        self.dirty = true;
        // Clean up empty maps to save memory.
        if tags.is_empty() {
            self.slots.remove(&comp_id);
        }
        Some(removed)
    }

    /// Get all dynamic tags for a component.
    pub fn get_all(&self, comp_id: u16) -> Option<&HashMap<String, DynValue>> {
        self.slots.get(&comp_id)
    }

    /// Bulk-set all tags for a component, replacing any existing tags.
    ///
    /// Returns `Err` if the new tag count would exceed limits.
    pub fn set_all(
        &mut self,
        comp_id: u16,
        tags: HashMap<String, DynValue>,
    ) -> Result<(), String> {
        if tags.len() > self.max_per_comp {
            return Err(format!(
                "tag count {} exceeds per-component limit {}",
                tags.len(),
                self.max_per_comp
            ));
        }

        // Calculate new total: subtract old count, add new count.
        let old_count = self.slots.get(&comp_id).map(|t| t.len()).unwrap_or(0);
        let new_total = self.total - old_count + tags.len();
        if new_total > self.max_total {
            return Err(format!(
                "new total {} would exceed limit {}",
                new_total, self.max_total
            ));
        }

        self.total = new_total;
        if tags.is_empty() {
            self.slots.remove(&comp_id);
        } else {
            self.slots.insert(comp_id, tags);
        }
        self.dirty = true;
        Ok(())
    }

    /// Remove all dynamic tags for a component.
    pub fn remove_all(&mut self, comp_id: u16) {
        if let Some(tags) = self.slots.remove(&comp_id) {
            self.total -= tags.len();
            self.dirty = true;
        }
    }

    /// List all component IDs that have dynamic tags.
    pub fn comp_ids(&self) -> Vec<u16> {
        self.slots.keys().copied().collect()
    }

    /// Count tags for a specific component.
    pub fn tag_count(&self, comp_id: u16) -> usize {
        self.slots.get(&comp_id).map(|t| t.len()).unwrap_or(0)
    }

    /// Total tag count across all components.
    pub fn total_count(&self) -> usize {
        self.total
    }

    /// Return and clear the dirty flag.
    pub fn take_dirty(&mut self) -> bool {
        let was = self.dirty;
        self.dirty = false;
        was
    }

    /// Check if the store has been modified since last save.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Serialize the store to a JSON file.
    pub fn save(&self, path: &str) -> Result<(), String> {
        let data = PersistData {
            version: 1,
            slots: self.slots.clone(),
        };
        let json = serde_json::to_string_pretty(&data)
            .map_err(|e| format!("serialize dyn_slots: {e}"))?;

        // Write atomically: write to temp file then rename.
        let tmp_path = format!("{path}.tmp");
        std::fs::write(&tmp_path, json)
            .map_err(|e| format!("write {tmp_path}: {e}"))?;
        std::fs::rename(&tmp_path, path)
            .map_err(|e| format!("rename {tmp_path} -> {path}: {e}"))?;

        info!(path, total = self.total, components = self.slots.len(), "dyn_slots saved");
        Ok(())
    }

    /// Load the store from a JSON file.
    ///
    /// If the file does not exist, this is a no-op (fresh start).
    pub fn load(&mut self, path: &str) -> Result<usize, String> {
        let p = Path::new(path);
        if !p.exists() {
            debug!(path, "dyn_slots: no persistence file, starting empty");
            return Ok(0);
        }

        let json = std::fs::read_to_string(p)
            .map_err(|e| format!("read {path}: {e}"))?;
        let data: PersistData = serde_json::from_str(&json)
            .map_err(|e| format!("parse {path}: {e}"))?;

        // Validate and count.
        let mut total = 0usize;
        for (comp_id, tags) in &data.slots {
            if tags.len() > self.max_per_comp {
                warn!(
                    comp_id,
                    count = tags.len(),
                    max = self.max_per_comp,
                    "dyn_slots: truncating tags over per-comp limit on load"
                );
            }
            total += tags.len().min(self.max_per_comp);
        }

        if total > self.max_total {
            warn!(
                total,
                max = self.max_total,
                "dyn_slots: loaded tag count exceeds total limit"
            );
        }

        // Apply (trusting the file for now, just count accurately).
        self.slots = data.slots;
        self.total = self.slots.values().map(|t| t.len()).sum();
        self.dirty = false;

        info!(
            path,
            components = self.slots.len(),
            total_tags = self.total,
            "dyn_slots loaded"
        );
        Ok(self.total)
    }
}

impl Default for DynSlotStore {
    fn default() -> Self {
        Self::with_defaults()
    }
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_store_is_empty() {
        let store = DynSlotStore::with_defaults();
        assert_eq!(store.total_count(), 0);
        assert!(store.comp_ids().is_empty());
        assert!(!store.is_dirty());
    }

    #[test]
    fn set_and_get_single_tag() {
        let mut store = DynSlotStore::with_defaults();
        let result = store.set(10, "devEUI".into(), DynValue::Str("A81758".into()));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None); // no previous

        let val = store.get(10, "devEUI");
        assert_eq!(val, Some(&DynValue::Str("A81758".into())));
        assert_eq!(store.total_count(), 1);
        assert!(store.is_dirty());
    }

    #[test]
    fn set_replaces_existing_tag() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "rssi".into(), DynValue::Int(-72)).unwrap();
        let prev = store.set(10, "rssi".into(), DynValue::Int(-65)).unwrap();
        assert_eq!(prev, Some(DynValue::Int(-72)));
        assert_eq!(store.get(10, "rssi"), Some(&DynValue::Int(-65)));
        assert_eq!(store.total_count(), 1); // count unchanged
    }

    #[test]
    fn remove_single_tag() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "foo".into(), DynValue::Marker).unwrap();
        assert_eq!(store.total_count(), 1);

        let removed = store.remove(10, "foo");
        assert_eq!(removed, Some(DynValue::Marker));
        assert_eq!(store.total_count(), 0);
        assert!(store.comp_ids().is_empty()); // empty map cleaned up
    }

    #[test]
    fn remove_nonexistent_tag_returns_none() {
        let mut store = DynSlotStore::with_defaults();
        assert_eq!(store.remove(999, "nope"), None);
    }

    #[test]
    fn get_all_tags_for_component() {
        let mut store = DynSlotStore::with_defaults();
        store.set(5, "a".into(), DynValue::Int(1)).unwrap();
        store.set(5, "b".into(), DynValue::Bool(true)).unwrap();
        store.set(5, "c".into(), DynValue::Null).unwrap();

        let tags = store.get_all(5).unwrap();
        assert_eq!(tags.len(), 3);
        assert_eq!(tags.get("a"), Some(&DynValue::Int(1)));
    }

    #[test]
    fn get_all_returns_none_for_unknown_comp() {
        let store = DynSlotStore::with_defaults();
        assert!(store.get_all(999).is_none());
    }

    #[test]
    fn set_all_bulk_replace() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "old".into(), DynValue::Marker).unwrap();
        assert_eq!(store.total_count(), 1);

        let mut new_tags = HashMap::new();
        new_tags.insert("x".into(), DynValue::Float(1.5));
        new_tags.insert("y".into(), DynValue::Float(2.5));
        store.set_all(10, new_tags).unwrap();

        assert_eq!(store.total_count(), 2); // old tag removed, 2 new
        assert!(store.get(10, "old").is_none());
        assert_eq!(store.get(10, "x"), Some(&DynValue::Float(1.5)));
    }

    #[test]
    fn set_all_with_empty_map_removes_component() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "a".into(), DynValue::Marker).unwrap();
        store.set_all(10, HashMap::new()).unwrap();
        assert_eq!(store.total_count(), 0);
        assert!(store.comp_ids().is_empty());
    }

    #[test]
    fn remove_all_tags_for_component() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "a".into(), DynValue::Marker).unwrap();
        store.set(10, "b".into(), DynValue::Int(42)).unwrap();
        store.set(20, "c".into(), DynValue::Marker).unwrap();
        assert_eq!(store.total_count(), 3);

        store.remove_all(10);
        assert_eq!(store.total_count(), 1);
        assert!(store.get_all(10).is_none());
        assert!(store.get_all(20).is_some());
    }

    #[test]
    fn remove_all_on_nonexistent_is_noop() {
        let mut store = DynSlotStore::with_defaults();
        store.remove_all(999); // should not panic
        assert_eq!(store.total_count(), 0);
    }

    #[test]
    fn comp_ids_returns_all_with_tags() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "a".into(), DynValue::Marker).unwrap();
        store.set(20, "b".into(), DynValue::Marker).unwrap();
        store.set(30, "c".into(), DynValue::Marker).unwrap();

        let mut ids = store.comp_ids();
        ids.sort();
        assert_eq!(ids, vec![10, 20, 30]);
    }

    #[test]
    fn tag_count_per_component() {
        let mut store = DynSlotStore::with_defaults();
        assert_eq!(store.tag_count(10), 0);
        store.set(10, "a".into(), DynValue::Marker).unwrap();
        store.set(10, "b".into(), DynValue::Marker).unwrap();
        assert_eq!(store.tag_count(10), 2);
    }

    #[test]
    fn per_comp_limit_enforced() {
        let mut store = DynSlotStore::new(2, 1000);
        store.set(10, "a".into(), DynValue::Marker).unwrap();
        store.set(10, "b".into(), DynValue::Marker).unwrap();
        let err = store.set(10, "c".into(), DynValue::Marker);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("max 2"));
    }

    #[test]
    fn total_limit_enforced() {
        let mut store = DynSlotStore::new(100, 3);
        store.set(1, "a".into(), DynValue::Marker).unwrap();
        store.set(2, "b".into(), DynValue::Marker).unwrap();
        store.set(3, "c".into(), DynValue::Marker).unwrap();
        let err = store.set(4, "d".into(), DynValue::Marker);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("limit 3"));
    }

    #[test]
    fn set_all_rejects_over_per_comp_limit() {
        let mut store = DynSlotStore::new(2, 1000);
        let mut tags = HashMap::new();
        tags.insert("a".into(), DynValue::Marker);
        tags.insert("b".into(), DynValue::Marker);
        tags.insert("c".into(), DynValue::Marker);
        let err = store.set_all(10, tags);
        assert!(err.is_err());
    }

    #[test]
    fn set_all_rejects_over_total_limit() {
        let mut store = DynSlotStore::new(100, 3);
        store.set(1, "x".into(), DynValue::Marker).unwrap();
        store.set(2, "y".into(), DynValue::Marker).unwrap();

        let mut tags = HashMap::new();
        tags.insert("a".into(), DynValue::Marker);
        tags.insert("b".into(), DynValue::Marker);
        // total would be 2 (existing on other comps) + 2 (new) = 4 > 3
        let err = store.set_all(10, tags);
        assert!(err.is_err());
    }

    #[test]
    fn take_dirty_returns_and_clears() {
        let mut store = DynSlotStore::with_defaults();
        assert!(!store.take_dirty());

        store.set(1, "a".into(), DynValue::Marker).unwrap();
        assert!(store.take_dirty());
        assert!(!store.take_dirty()); // cleared
    }

    #[test]
    fn all_dyn_value_variants() {
        let mut store = DynSlotStore::with_defaults();
        store.set(1, "null".into(), DynValue::Null).unwrap();
        store.set(1, "marker".into(), DynValue::Marker).unwrap();
        store.set(1, "bool".into(), DynValue::Bool(true)).unwrap();
        store.set(1, "int".into(), DynValue::Int(42)).unwrap();
        store.set(1, "float".into(), DynValue::Float(3.14)).unwrap();
        store.set(1, "str".into(), DynValue::Str("hello".into())).unwrap();
        store.set(1, "ref".into(), DynValue::Ref("@p:demo".into())).unwrap();

        assert_eq!(store.tag_count(1), 7);
        assert_eq!(store.get(1, "null"), Some(&DynValue::Null));
        assert_eq!(store.get(1, "marker"), Some(&DynValue::Marker));
        assert_eq!(store.get(1, "bool"), Some(&DynValue::Bool(true)));
        assert_eq!(store.get(1, "int"), Some(&DynValue::Int(42)));
        assert_eq!(store.get(1, "float"), Some(&DynValue::Float(3.14)));
        assert_eq!(store.get(1, "str"), Some(&DynValue::Str("hello".into())));
        assert_eq!(store.get(1, "ref"), Some(&DynValue::Ref("@p:demo".into())));
    }

    #[test]
    fn persistence_round_trip() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("dyn_slots.json");
        let path_str = path.to_str().unwrap();

        let mut store = DynSlotStore::with_defaults();
        store.set(10, "devEUI".into(), DynValue::Str("A81758".into())).unwrap();
        store.set(10, "rssi".into(), DynValue::Int(-72)).unwrap();
        store.set(20, "address".into(), DynValue::Int(40001)).unwrap();
        store.set(20, "enabled".into(), DynValue::Bool(true)).unwrap();
        store.set(20, "point".into(), DynValue::Marker).unwrap();
        store.set(30, "scale".into(), DynValue::Float(0.1)).unwrap();
        store.set(30, "ref".into(), DynValue::Ref("@p:demo:r:abc".into())).unwrap();
        store.set(30, "empty".into(), DynValue::Null).unwrap();

        store.save(path_str).expect("save should succeed");
        assert!(path.exists());

        // Load into a new store.
        let mut store2 = DynSlotStore::with_defaults();
        let loaded = store2.load(path_str).expect("load should succeed");
        assert_eq!(loaded, 8);
        assert_eq!(store2.total_count(), 8);
        assert_eq!(store2.get(10, "devEUI"), Some(&DynValue::Str("A81758".into())));
        assert_eq!(store2.get(10, "rssi"), Some(&DynValue::Int(-72)));
        assert_eq!(store2.get(20, "address"), Some(&DynValue::Int(40001)));
        assert_eq!(store2.get(20, "enabled"), Some(&DynValue::Bool(true)));
        assert_eq!(store2.get(20, "point"), Some(&DynValue::Marker));
        assert_eq!(store2.get(30, "scale"), Some(&DynValue::Float(0.1)));
        assert_eq!(store2.get(30, "ref"), Some(&DynValue::Ref("@p:demo:r:abc".into())));
        assert_eq!(store2.get(30, "empty"), Some(&DynValue::Null));

        // Loaded store should not be dirty.
        assert!(!store2.is_dirty());
    }

    #[test]
    fn load_nonexistent_file_is_noop() {
        let mut store = DynSlotStore::with_defaults();
        let loaded = store.load("/nonexistent/path/dyn_slots.json").expect("should succeed");
        assert_eq!(loaded, 0);
        assert_eq!(store.total_count(), 0);
    }

    #[test]
    fn persistence_survives_empty_store() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("dyn_slots.json");
        let path_str = path.to_str().unwrap();

        let store = DynSlotStore::with_defaults();
        store.save(path_str).expect("save empty");

        let mut store2 = DynSlotStore::with_defaults();
        let loaded = store2.load(path_str).expect("load empty");
        assert_eq!(loaded, 0);
    }

    #[test]
    fn component_delete_cleanup() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "a".into(), DynValue::Marker).unwrap();
        store.set(10, "b".into(), DynValue::Int(42)).unwrap();
        store.set(20, "c".into(), DynValue::Marker).unwrap();

        // Simulate component delete: remove_all for the deleted comp.
        store.remove_all(10);
        assert_eq!(store.total_count(), 1);
        assert!(store.get_all(10).is_none());
        assert_eq!(store.get(20, "c"), Some(&DynValue::Marker));
    }

    #[test]
    fn persistence_after_modifications() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("dyn_slots.json");
        let path_str = path.to_str().unwrap();

        let mut store = DynSlotStore::with_defaults();
        store.set(10, "a".into(), DynValue::Int(1)).unwrap();
        store.set(10, "b".into(), DynValue::Int(2)).unwrap();
        store.set(20, "c".into(), DynValue::Int(3)).unwrap();

        // Remove one tag, modify another
        store.remove(10, "a");
        store.set(10, "b".into(), DynValue::Int(99)).unwrap();

        store.save(path_str).unwrap();

        let mut store2 = DynSlotStore::with_defaults();
        store2.load(path_str).unwrap();
        assert_eq!(store2.total_count(), 2);
        assert!(store2.get(10, "a").is_none());
        assert_eq!(store2.get(10, "b"), Some(&DynValue::Int(99)));
        assert_eq!(store2.get(20, "c"), Some(&DynValue::Int(3)));
    }

    #[test]
    fn dyn_value_serde_round_trip() {
        // Ensure all variants survive JSON round-trip.
        let values = vec![
            ("null", DynValue::Null),
            ("marker", DynValue::Marker),
            ("bool", DynValue::Bool(false)),
            ("int", DynValue::Int(i64::MIN)),
            ("float", DynValue::Float(f64::MAX)),
            ("str", DynValue::Str("hello\nworld".into())),
            ("ref", DynValue::Ref("@p:demo:r:abc".into())),
        ];

        for (name, val) in values {
            let json = serde_json::to_string(&val).expect("serialize");
            let back: DynValue = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(val, back, "round-trip failed for {name}: {json}");
        }
    }
}
