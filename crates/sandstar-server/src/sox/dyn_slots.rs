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
use std::fmt;
use std::path::Path;
use tracing::{debug, info, warn};

/// Default maximum tags per component.
pub const DEFAULT_MAX_PER_COMP: usize = 64;
/// Default maximum total tags across all components.
pub const DEFAULT_MAX_TOTAL: usize = 10_000;

/// Common Haystack / protocol tag names pre-interned at startup.
const COMMON_TAG_NAMES: &[&str] = &[
    "dis",
    "navName",
    "unit",
    "kind",
    "point",
    "sensor",
    "cmd",
    "equip",
    "site",
    "geoAddr",
    "geoCoord",
    "tz",
    "modbusAddr",
    "modbusReg",
    "modbusType",
    "modbusScale",
    "bacnetObj",
    "bacnetType",
    "bacnetProp",
    "devEUI",
    "appEUI",
    "devAddr",
    "mqttTopic",
    "mqttQos",
    "channel",
    "direction",
    "enabled",
    "status",
];

// ── String Interner ──────────────────────────────────────────

/// String interner for tag name deduplication.
///
/// Common tag names like "modbusAddr", "bacnetObj", "devEUI", "unit", "dis"
/// are stored once and referenced by a `u16` index. This saves memory when
/// many components carry the same tag names (typical in driver discovery
/// scenarios with hundreds of learned points).
pub struct TagNameInterner {
    names: Vec<String>,
    lookup: HashMap<String, u16>,
}

impl TagNameInterner {
    /// Create a new interner pre-populated with common Haystack/protocol tag names.
    pub fn new() -> Self {
        let mut interner = Self {
            names: Vec::with_capacity(256),
            lookup: HashMap::with_capacity(256),
        };
        for &name in COMMON_TAG_NAMES {
            interner.intern(name);
        }
        interner
    }

    /// Intern a tag name, returning its stable u16 ID.
    ///
    /// If the name was already interned, returns the existing ID.
    /// Panics (in debug) if more than 65535 unique names are interned.
    pub fn intern(&mut self, name: &str) -> u16 {
        if let Some(&id) = self.lookup.get(name) {
            return id;
        }
        let id = self.names.len() as u16;
        debug_assert!(
            (self.names.len()) < u16::MAX as usize,
            "tag name interner overflow"
        );
        self.names.push(name.to_string());
        self.lookup.insert(name.to_string(), id);
        id
    }

    /// Resolve an interned ID back to the tag name string.
    pub fn resolve(&self, id: u16) -> Option<&str> {
        self.names.get(id as usize).map(|s| s.as_str())
    }

    /// Look up the ID for a tag name without interning it.
    pub fn get_id(&self, name: &str) -> Option<u16> {
        self.lookup.get(name).copied()
    }

    /// Number of unique tag names currently interned.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether the interner is empty.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Memory statistics: `(interned_count, total_string_bytes)`.
    pub fn stats(&self) -> (usize, usize) {
        let total_bytes: usize = self.names.iter().map(|s| s.len()).sum();
        (self.names.len(), total_bytes)
    }

    /// Return the list of all interned names (in ID order).
    pub fn all_names(&self) -> &[String] {
        &self.names
    }
}

impl Default for TagNameInterner {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for TagNameInterner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TagNameInterner")
            .field("count", &self.names.len())
            .finish()
    }
}

// ── Computed / Virtual Slots (Layer 3) ───────────────────────

/// A numeric binary operation for computed slot formulas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NumericOp {
    Add,
    Sub,
    Mul,
    Div,
}

impl fmt::Display for NumericOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Add => write!(f, "+"),
            Self::Sub => write!(f, "-"),
            Self::Mul => write!(f, "*"),
            Self::Div => write!(f, "/"),
        }
    }
}

/// Formula that defines how a computed slot derives its value.
///
/// Computed slots are evaluated at read time and never persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ComputedFormula {
    /// Always returns a fixed constant value.
    Constant(DynValue),
    /// Copies the value of another tag on the same component.
    CopyTag(String),
    /// Concatenates the string representations of multiple tags.
    Concat(Vec<String>),
    /// Applies a numeric binary operation: `left op right`.
    NumericOp {
        left: String,
        op: NumericOp,
        right: String,
    },
    /// Conditional: if `tag` exists on the component, return `true_val`;
    /// otherwise return `false_val`.
    TagExists {
        tag: String,
        true_val: DynValue,
        false_val: DynValue,
    },
}

/// A computed (virtual) slot definition.
///
/// Computed slots appear alongside regular dynamic tags when reading a
/// component, but they are not persisted — their values are derived from
/// formulas evaluated against the component's current tags.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputedSlot {
    /// The tag name that this computed slot produces.
    pub name: String,
    /// The formula used to calculate the value.
    pub formula: ComputedFormula,
}

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

/// Interner statistics returned by [`DynSlotStore::interner_stats`].
#[derive(Debug, Clone, Serialize)]
pub struct InternerStats {
    /// Total unique tag names interned.
    pub interned_count: usize,
    /// Total bytes used by interned strings.
    pub total_string_bytes: usize,
    /// Number of pre-interned common tag names.
    pub pre_interned: usize,
}

impl DynValue {
    /// Extract a numeric value as `f64`, converting Int to Float.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            DynValue::Float(f) => Some(*f),
            DynValue::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    /// Human-readable string representation for concatenation formulas.
    pub fn to_display_string(&self) -> String {
        match self {
            DynValue::Null => String::new(),
            DynValue::Marker => "\u{2713}".to_string(), // checkmark
            DynValue::Bool(b) => b.to_string(),
            DynValue::Int(i) => i.to_string(),
            DynValue::Float(f) => f.to_string(),
            DynValue::Str(s) => s.clone(),
            DynValue::Ref(r) => r.clone(),
        }
    }
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
    /// String interner for tag name deduplication and stats.
    interner: TagNameInterner,
    /// comp_id → list of computed (virtual) slot definitions.
    computed: HashMap<u16, Vec<ComputedSlot>>,
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
            interner: TagNameInterner::new(),
            computed: HashMap::new(),
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
        // Intern the tag name for dedup stats / future wire optimization.
        self.interner.intern(&key);

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
    pub fn set_all(&mut self, comp_id: u16, tags: HashMap<String, DynValue>) -> Result<(), String> {
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

        // Intern all tag names.
        for key in tags.keys() {
            self.interner.intern(key);
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

    /// Remove all dynamic tags for a component (including computed slots).
    pub fn remove_all(&mut self, comp_id: u16) {
        if let Some(tags) = self.slots.remove(&comp_id) {
            self.total -= tags.len();
            self.dirty = true;
        }
        self.computed.remove(&comp_id);
    }

    // ── Computed / Virtual Slots ─────────────────────────────

    /// Register a computed (virtual) slot for a component.
    ///
    /// Computed slots are evaluated at read time and never persisted.
    /// If a computed slot with the same name already exists, it is replaced.
    pub fn add_computed(&mut self, comp_id: u16, slot: ComputedSlot) {
        let slots = self.computed.entry(comp_id).or_default();
        // Replace existing computed slot with same name.
        if let Some(existing) = slots.iter_mut().find(|s| s.name == slot.name) {
            *existing = slot;
        } else {
            slots.push(slot);
        }
    }

    /// Remove a computed slot by name. Returns true if it existed.
    pub fn remove_computed(&mut self, comp_id: u16, name: &str) -> bool {
        if let Some(slots) = self.computed.get_mut(&comp_id) {
            let before = slots.len();
            slots.retain(|s| s.name != name);
            let after = slots.len();
            let removed = after < before;
            if slots.is_empty() {
                self.computed.remove(&comp_id);
            }
            removed
        } else {
            false
        }
    }

    /// Get the computed slot definitions for a component.
    pub fn get_computed(&self, comp_id: u16) -> Option<&[ComputedSlot]> {
        self.computed.get(&comp_id).map(|v| v.as_slice())
    }

    /// Get all tags for a component, including computed (virtual) slot values.
    ///
    /// Computed slots are evaluated against the component's current stored tags.
    /// If a computed slot has the same name as a stored tag, the stored tag wins.
    pub fn get_all_with_computed(&self, comp_id: u16) -> HashMap<String, DynValue> {
        let mut result = self.slots.get(&comp_id).cloned().unwrap_or_default();

        if let Some(computed) = self.computed.get(&comp_id) {
            for slot in computed {
                // Stored tags take precedence over computed slots.
                if !result.contains_key(&slot.name) {
                    let value = self.evaluate_formula(comp_id, &slot.formula);
                    result.insert(slot.name.clone(), value);
                }
            }
        }

        result
    }

    /// Evaluate a computed formula against the stored tags of a component.
    fn evaluate_formula(&self, comp_id: u16, formula: &ComputedFormula) -> DynValue {
        let tags = self.slots.get(&comp_id);

        match formula {
            ComputedFormula::Constant(val) => val.clone(),

            ComputedFormula::CopyTag(source) => tags
                .and_then(|t| t.get(source))
                .cloned()
                .unwrap_or(DynValue::Null),

            ComputedFormula::Concat(sources) => {
                let mut buf = String::new();
                if let Some(tags) = tags {
                    for src in sources {
                        if let Some(val) = tags.get(src) {
                            buf.push_str(&val.to_display_string());
                        }
                    }
                }
                DynValue::Str(buf)
            }

            ComputedFormula::NumericOp { left, op, right } => {
                let lval = tags.and_then(|t| t.get(left)).and_then(|v| v.as_f64());
                let rval = tags.and_then(|t| t.get(right)).and_then(|v| v.as_f64());
                match (lval, rval) {
                    (Some(l), Some(r)) => {
                        let result = match op {
                            NumericOp::Add => l + r,
                            NumericOp::Sub => l - r,
                            NumericOp::Mul => l * r,
                            NumericOp::Div => {
                                if r == 0.0 {
                                    return DynValue::Null;
                                }
                                l / r
                            }
                        };
                        DynValue::Float(result)
                    }
                    _ => DynValue::Null,
                }
            }

            ComputedFormula::TagExists {
                tag,
                true_val,
                false_val,
            } => {
                let exists = tags.map(|t| t.contains_key(tag)).unwrap_or(false);
                if exists {
                    true_val.clone()
                } else {
                    false_val.clone()
                }
            }
        }
    }

    // ── Interner Access ──────────────────────────────────────

    /// Get a reference to the tag name interner.
    pub fn interner(&self) -> &TagNameInterner {
        &self.interner
    }

    /// Get interner statistics: `{ interned_count, total_string_bytes, pre_interned }`.
    pub fn interner_stats(&self) -> InternerStats {
        let (count, bytes) = self.interner.stats();
        InternerStats {
            interned_count: count,
            total_string_bytes: bytes,
            pre_interned: COMMON_TAG_NAMES.len(),
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
        let json =
            serde_json::to_string_pretty(&data).map_err(|e| format!("serialize dyn_slots: {e}"))?;

        // Write atomically: write to temp file then rename.
        let tmp_path = format!("{path}.tmp");
        std::fs::write(&tmp_path, json).map_err(|e| format!("write {tmp_path}: {e}"))?;
        std::fs::rename(&tmp_path, path)
            .map_err(|e| format!("rename {tmp_path} -> {path}: {e}"))?;

        info!(
            path,
            total = self.total,
            components = self.slots.len(),
            "dyn_slots saved"
        );
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

        let json = std::fs::read_to_string(p).map_err(|e| format!("read {path}: {e}"))?;
        let data: PersistData = match serde_json::from_str(&json) {
            Ok(d) => d,
            Err(e) => {
                // Corrupt file: log warning and start fresh rather than blocking startup.
                warn!(path, error = %e, "dyn_slots: corrupt persistence file, starting empty");
                return Ok(0);
            }
        };

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
        // Intern all loaded tag names.
        for tags in data.slots.values() {
            for key in tags.keys() {
                self.interner.intern(key);
            }
        }
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
        store
            .set(1, "str".into(), DynValue::Str("hello".into()))
            .unwrap();
        store
            .set(1, "ref".into(), DynValue::Ref("@p:demo".into()))
            .unwrap();

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
        store
            .set(10, "devEUI".into(), DynValue::Str("A81758".into()))
            .unwrap();
        store.set(10, "rssi".into(), DynValue::Int(-72)).unwrap();
        store
            .set(20, "address".into(), DynValue::Int(40001))
            .unwrap();
        store
            .set(20, "enabled".into(), DynValue::Bool(true))
            .unwrap();
        store.set(20, "point".into(), DynValue::Marker).unwrap();
        store.set(30, "scale".into(), DynValue::Float(0.1)).unwrap();
        store
            .set(30, "ref".into(), DynValue::Ref("@p:demo:r:abc".into()))
            .unwrap();
        store.set(30, "empty".into(), DynValue::Null).unwrap();

        store.save(path_str).expect("save should succeed");
        assert!(path.exists());

        // Load into a new store.
        let mut store2 = DynSlotStore::with_defaults();
        let loaded = store2.load(path_str).expect("load should succeed");
        assert_eq!(loaded, 8);
        assert_eq!(store2.total_count(), 8);
        assert_eq!(
            store2.get(10, "devEUI"),
            Some(&DynValue::Str("A81758".into()))
        );
        assert_eq!(store2.get(10, "rssi"), Some(&DynValue::Int(-72)));
        assert_eq!(store2.get(20, "address"), Some(&DynValue::Int(40001)));
        assert_eq!(store2.get(20, "enabled"), Some(&DynValue::Bool(true)));
        assert_eq!(store2.get(20, "point"), Some(&DynValue::Marker));
        assert_eq!(store2.get(30, "scale"), Some(&DynValue::Float(0.1)));
        assert_eq!(
            store2.get(30, "ref"),
            Some(&DynValue::Ref("@p:demo:r:abc".into()))
        );
        assert_eq!(store2.get(30, "empty"), Some(&DynValue::Null));

        // Loaded store should not be dirty.
        assert!(!store2.is_dirty());
    }

    #[test]
    fn load_nonexistent_file_is_noop() {
        let mut store = DynSlotStore::with_defaults();
        let loaded = store
            .load("/nonexistent/path/dyn_slots.json")
            .expect("should succeed");
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
    fn load_corrupt_file_starts_empty() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("dyn_slots.json");
        std::fs::write(&path, "{ this is not valid json !!!").expect("write corrupt");

        let mut store = DynSlotStore::with_defaults();
        let loaded = store
            .load(path.to_str().unwrap())
            .expect("should not error on corrupt");
        assert_eq!(loaded, 0);
        assert_eq!(store.total_count(), 0);
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

    // ── TagNameInterner Tests ────────────────────────────────

    #[test]
    fn interner_pre_interns_common_names() {
        let interner = TagNameInterner::new();
        assert!(
            interner.len() >= 28,
            "should have pre-interned common names"
        );
        // Verify a few known common names.
        assert!(interner.get_id("dis").is_some());
        assert!(interner.get_id("unit").is_some());
        assert!(interner.get_id("modbusAddr").is_some());
        assert!(interner.get_id("devEUI").is_some());
        assert!(interner.get_id("enabled").is_some());
    }

    #[test]
    fn interner_intern_and_resolve() {
        let mut interner = TagNameInterner::new();
        let id = interner.intern("customTag");
        assert_eq!(interner.resolve(id), Some("customTag"));
    }

    #[test]
    fn interner_dedup_same_name() {
        let mut interner = TagNameInterner::new();
        let id1 = interner.intern("myTag");
        let id2 = interner.intern("myTag");
        assert_eq!(id1, id2, "same name must return same ID");
        let before = interner.len();
        interner.intern("myTag");
        assert_eq!(interner.len(), before, "no growth on duplicate intern");
    }

    #[test]
    fn interner_get_id_without_interning() {
        let interner = TagNameInterner::new();
        assert!(interner.get_id("dis").is_some());
        assert!(interner.get_id("nonExistentTag12345").is_none());
    }

    #[test]
    fn interner_stats_reports_bytes() {
        let mut interner = TagNameInterner::new();
        let (count_before, bytes_before) = interner.stats();
        interner.intern("extra");
        let (count_after, bytes_after) = interner.stats();
        assert_eq!(count_after, count_before + 1);
        assert_eq!(bytes_after, bytes_before + 5); // "extra" = 5 bytes
    }

    #[test]
    fn interner_resolve_invalid_id() {
        let interner = TagNameInterner::new();
        assert!(interner.resolve(60000).is_none());
    }

    #[test]
    fn store_interns_tag_names_on_set() {
        let mut store = DynSlotStore::with_defaults();
        let before = store.interner().len();
        store
            .set(1, "customDriverTag".into(), DynValue::Marker)
            .unwrap();
        assert!(store.interner().get_id("customDriverTag").is_some());
        assert_eq!(store.interner().len(), before + 1);
    }

    #[test]
    fn store_interns_tag_names_on_set_all() {
        let mut store = DynSlotStore::with_defaults();
        let mut tags = HashMap::new();
        tags.insert("bulkTag1".into(), DynValue::Int(1));
        tags.insert("bulkTag2".into(), DynValue::Int(2));
        store.set_all(10, tags).unwrap();
        assert!(store.interner().get_id("bulkTag1").is_some());
        assert!(store.interner().get_id("bulkTag2").is_some());
    }

    #[test]
    fn store_interner_stats() {
        let store = DynSlotStore::with_defaults();
        let stats = store.interner_stats();
        assert!(stats.interned_count >= 28);
        assert_eq!(stats.pre_interned, COMMON_TAG_NAMES.len());
        assert!(stats.total_string_bytes > 0);
    }

    // ── Computed Slot Tests ──────────────────────────────────

    #[test]
    fn computed_constant() {
        let mut store = DynSlotStore::with_defaults();
        store.add_computed(
            10,
            ComputedSlot {
                name: "version".into(),
                formula: ComputedFormula::Constant(DynValue::Str("1.0".into())),
            },
        );
        let all = store.get_all_with_computed(10);
        assert_eq!(all.get("version"), Some(&DynValue::Str("1.0".into())));
    }

    #[test]
    fn computed_copy_tag() {
        let mut store = DynSlotStore::with_defaults();
        store
            .set(10, "dis".into(), DynValue::Str("Room Temp".into()))
            .unwrap();
        store.add_computed(
            10,
            ComputedSlot {
                name: "label".into(),
                formula: ComputedFormula::CopyTag("dis".into()),
            },
        );
        let all = store.get_all_with_computed(10);
        assert_eq!(all.get("label"), Some(&DynValue::Str("Room Temp".into())));
    }

    #[test]
    fn computed_copy_tag_missing_source() {
        let mut store = DynSlotStore::with_defaults();
        store.add_computed(
            10,
            ComputedSlot {
                name: "label".into(),
                formula: ComputedFormula::CopyTag("nonexistent".into()),
            },
        );
        let all = store.get_all_with_computed(10);
        assert_eq!(all.get("label"), Some(&DynValue::Null));
    }

    #[test]
    fn computed_concat() {
        let mut store = DynSlotStore::with_defaults();
        store
            .set(10, "first".into(), DynValue::Str("Room".into()))
            .unwrap();
        store
            .set(10, "sep".into(), DynValue::Str(" - ".into()))
            .unwrap();
        store
            .set(10, "second".into(), DynValue::Str("Temp".into()))
            .unwrap();
        store.add_computed(
            10,
            ComputedSlot {
                name: "fullName".into(),
                formula: ComputedFormula::Concat(vec![
                    "first".into(),
                    "sep".into(),
                    "second".into(),
                ]),
            },
        );
        let all = store.get_all_with_computed(10);
        assert_eq!(
            all.get("fullName"),
            Some(&DynValue::Str("Room - Temp".into()))
        );
    }

    #[test]
    fn computed_numeric_add() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "a".into(), DynValue::Float(10.0)).unwrap();
        store.set(10, "b".into(), DynValue::Int(5)).unwrap();
        store.add_computed(
            10,
            ComputedSlot {
                name: "sum".into(),
                formula: ComputedFormula::NumericOp {
                    left: "a".into(),
                    op: NumericOp::Add,
                    right: "b".into(),
                },
            },
        );
        let all = store.get_all_with_computed(10);
        assert_eq!(all.get("sum"), Some(&DynValue::Float(15.0)));
    }

    #[test]
    fn computed_numeric_sub() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "a".into(), DynValue::Float(10.0)).unwrap();
        store.set(10, "b".into(), DynValue::Float(3.0)).unwrap();
        store.add_computed(
            10,
            ComputedSlot {
                name: "diff".into(),
                formula: ComputedFormula::NumericOp {
                    left: "a".into(),
                    op: NumericOp::Sub,
                    right: "b".into(),
                },
            },
        );
        let all = store.get_all_with_computed(10);
        assert_eq!(all.get("diff"), Some(&DynValue::Float(7.0)));
    }

    #[test]
    fn computed_numeric_mul() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "a".into(), DynValue::Float(4.0)).unwrap();
        store.set(10, "b".into(), DynValue::Float(2.5)).unwrap();
        store.add_computed(
            10,
            ComputedSlot {
                name: "product".into(),
                formula: ComputedFormula::NumericOp {
                    left: "a".into(),
                    op: NumericOp::Mul,
                    right: "b".into(),
                },
            },
        );
        let all = store.get_all_with_computed(10);
        assert_eq!(all.get("product"), Some(&DynValue::Float(10.0)));
    }

    #[test]
    fn computed_numeric_div() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "a".into(), DynValue::Float(10.0)).unwrap();
        store.set(10, "b".into(), DynValue::Float(4.0)).unwrap();
        store.add_computed(
            10,
            ComputedSlot {
                name: "ratio".into(),
                formula: ComputedFormula::NumericOp {
                    left: "a".into(),
                    op: NumericOp::Div,
                    right: "b".into(),
                },
            },
        );
        let all = store.get_all_with_computed(10);
        assert_eq!(all.get("ratio"), Some(&DynValue::Float(2.5)));
    }

    #[test]
    fn computed_numeric_div_by_zero() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "a".into(), DynValue::Float(10.0)).unwrap();
        store.set(10, "b".into(), DynValue::Float(0.0)).unwrap();
        store.add_computed(
            10,
            ComputedSlot {
                name: "ratio".into(),
                formula: ComputedFormula::NumericOp {
                    left: "a".into(),
                    op: NumericOp::Div,
                    right: "b".into(),
                },
            },
        );
        let all = store.get_all_with_computed(10);
        assert_eq!(all.get("ratio"), Some(&DynValue::Null));
    }

    #[test]
    fn computed_numeric_missing_operand() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "a".into(), DynValue::Float(10.0)).unwrap();
        // "b" is missing
        store.add_computed(
            10,
            ComputedSlot {
                name: "sum".into(),
                formula: ComputedFormula::NumericOp {
                    left: "a".into(),
                    op: NumericOp::Add,
                    right: "b".into(),
                },
            },
        );
        let all = store.get_all_with_computed(10);
        assert_eq!(all.get("sum"), Some(&DynValue::Null));
    }

    #[test]
    fn computed_tag_exists_true() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "point".into(), DynValue::Marker).unwrap();
        store.add_computed(
            10,
            ComputedSlot {
                name: "isPoint".into(),
                formula: ComputedFormula::TagExists {
                    tag: "point".into(),
                    true_val: DynValue::Bool(true),
                    false_val: DynValue::Bool(false),
                },
            },
        );
        let all = store.get_all_with_computed(10);
        assert_eq!(all.get("isPoint"), Some(&DynValue::Bool(true)));
    }

    #[test]
    fn computed_tag_exists_false() {
        let mut store = DynSlotStore::with_defaults();
        // "point" not set
        store.add_computed(
            10,
            ComputedSlot {
                name: "isPoint".into(),
                formula: ComputedFormula::TagExists {
                    tag: "point".into(),
                    true_val: DynValue::Bool(true),
                    false_val: DynValue::Bool(false),
                },
            },
        );
        let all = store.get_all_with_computed(10);
        assert_eq!(all.get("isPoint"), Some(&DynValue::Bool(false)));
    }

    #[test]
    fn computed_stored_tag_takes_precedence() {
        let mut store = DynSlotStore::with_defaults();
        store
            .set(10, "label".into(), DynValue::Str("stored".into()))
            .unwrap();
        store.add_computed(
            10,
            ComputedSlot {
                name: "label".into(),
                formula: ComputedFormula::Constant(DynValue::Str("computed".into())),
            },
        );
        let all = store.get_all_with_computed(10);
        // Stored tag should win over computed slot with same name.
        assert_eq!(all.get("label"), Some(&DynValue::Str("stored".into())));
    }

    #[test]
    fn computed_not_persisted() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("dyn_slots.json");
        let path_str = path.to_str().unwrap();

        let mut store = DynSlotStore::with_defaults();
        store.set(10, "a".into(), DynValue::Int(1)).unwrap();
        store.add_computed(
            10,
            ComputedSlot {
                name: "doubled".into(),
                formula: ComputedFormula::Constant(DynValue::Int(2)),
            },
        );
        store.save(path_str).unwrap();

        // Load into a new store — computed slots should NOT be restored.
        let mut store2 = DynSlotStore::with_defaults();
        store2.load(path_str).unwrap();
        assert_eq!(store2.get_computed(10), None);
        // Stored tags are restored.
        assert_eq!(store2.get(10, "a"), Some(&DynValue::Int(1)));
    }

    #[test]
    fn computed_get_all_without_computed_excludes_them() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "a".into(), DynValue::Int(1)).unwrap();
        store.add_computed(
            10,
            ComputedSlot {
                name: "computed_tag".into(),
                formula: ComputedFormula::Constant(DynValue::Int(99)),
            },
        );
        // get_all (the original method) should NOT include computed slots.
        let stored = store.get_all(10).unwrap();
        assert!(!stored.contains_key("computed_tag"));
        assert_eq!(stored.len(), 1);
    }

    #[test]
    fn computed_replace_existing_slot() {
        let mut store = DynSlotStore::with_defaults();
        store.add_computed(
            10,
            ComputedSlot {
                name: "ver".into(),
                formula: ComputedFormula::Constant(DynValue::Str("1.0".into())),
            },
        );
        store.add_computed(
            10,
            ComputedSlot {
                name: "ver".into(),
                formula: ComputedFormula::Constant(DynValue::Str("2.0".into())),
            },
        );
        let slots = store.get_computed(10).unwrap();
        assert_eq!(slots.len(), 1);
        let all = store.get_all_with_computed(10);
        assert_eq!(all.get("ver"), Some(&DynValue::Str("2.0".into())));
    }

    #[test]
    fn computed_remove() {
        let mut store = DynSlotStore::with_defaults();
        store.add_computed(
            10,
            ComputedSlot {
                name: "temp".into(),
                formula: ComputedFormula::Constant(DynValue::Int(0)),
            },
        );
        assert!(store.remove_computed(10, "temp"));
        assert!(!store.remove_computed(10, "temp")); // already gone
        assert!(store.get_computed(10).is_none());
    }

    #[test]
    fn computed_remove_all_cleans_computed() {
        let mut store = DynSlotStore::with_defaults();
        store.set(10, "a".into(), DynValue::Int(1)).unwrap();
        store.add_computed(
            10,
            ComputedSlot {
                name: "comp_tag".into(),
                formula: ComputedFormula::Constant(DynValue::Int(2)),
            },
        );
        store.remove_all(10);
        assert!(store.get_computed(10).is_none());
        assert!(store.get_all(10).is_none());
    }

    #[test]
    fn dyn_value_as_f64() {
        assert_eq!(DynValue::Float(3.14).as_f64(), Some(3.14));
        assert_eq!(DynValue::Int(42).as_f64(), Some(42.0));
        assert_eq!(DynValue::Str("nope".into()).as_f64(), None);
        assert_eq!(DynValue::Null.as_f64(), None);
        assert_eq!(DynValue::Marker.as_f64(), None);
    }

    #[test]
    fn dyn_value_display_string() {
        assert_eq!(DynValue::Null.to_display_string(), "");
        assert_eq!(DynValue::Bool(true).to_display_string(), "true");
        assert_eq!(DynValue::Int(42).to_display_string(), "42");
        assert_eq!(DynValue::Float(1.5).to_display_string(), "1.5");
        assert_eq!(DynValue::Str("hello".into()).to_display_string(), "hello");
        assert_eq!(DynValue::Ref("@ref".into()).to_display_string(), "@ref");
    }

    #[test]
    fn computed_on_empty_component() {
        let mut store = DynSlotStore::with_defaults();
        // Component 10 has NO stored tags, only computed slots.
        store.add_computed(
            10,
            ComputedSlot {
                name: "always".into(),
                formula: ComputedFormula::Constant(DynValue::Marker),
            },
        );
        let all = store.get_all_with_computed(10);
        assert_eq!(all.len(), 1);
        assert_eq!(all.get("always"), Some(&DynValue::Marker));
    }

    #[test]
    fn interner_all_names() {
        let mut interner = TagNameInterner::new();
        interner.intern("extra1");
        interner.intern("extra2");
        let names = interner.all_names();
        assert!(names.contains(&"dis".to_string()));
        assert!(names.contains(&"extra1".to_string()));
        assert!(names.contains(&"extra2".to_string()));
    }

    #[test]
    fn interner_is_empty() {
        // Default interner is not empty (has pre-interned names).
        let interner = TagNameInterner::new();
        assert!(!interner.is_empty());
    }

    #[test]
    fn persistence_preserves_interner_state() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("dyn_slots.json");
        let path_str = path.to_str().unwrap();

        let mut store = DynSlotStore::with_defaults();
        store.set(10, "customTag".into(), DynValue::Int(1)).unwrap();
        store.save(path_str).unwrap();

        let mut store2 = DynSlotStore::with_defaults();
        store2.load(path_str).unwrap();
        // After load, the custom tag name should be interned.
        assert!(store2.interner().get_id("customTag").is_some());
    }
}
