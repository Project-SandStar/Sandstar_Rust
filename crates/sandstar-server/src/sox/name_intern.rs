//! Name interning for SOX virtual components.
//!
//! Provides a thread-safe intern table that stores component names once and
//! references them by compact 2-byte IDs. This module also centralises the
//! Sedona-compatible name validation rules so they are not duplicated across
//! REST handlers.
//!
//! # Design
//!
//! The intern table lives alongside the existing `String`-based component
//! names — it is an *optimisation layer*, not a replacement. The component
//! tree keeps `name: String` for easy serialisation, while the intern table
//! accelerates lookups (e.g. channel bridge "chXXXX" matching) and provides
//! deduplication statistics.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

/// Maximum component name length for Sedona editor compatibility.
pub const MAX_NAME_LEN: usize = 31;

/// Most names the table holds: IDs 1..=65 534 (0 is the sentinel and
/// `u16::MAX` is never handed out).
pub const MAX_INTERNED_NAMES: usize = u16::MAX as usize - 1;

// ── NameId ───────────────────────────────────────────

/// Compact name reference (2 bytes, `Copy`).
///
/// ID 0 is reserved as the invalid/empty sentinel.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct NameId(pub u16);

impl NameId {
    /// Sentinel value representing an invalid or empty name.
    pub const INVALID: NameId = NameId(0);

    /// Returns `true` if this ID refers to a real interned name.
    pub fn is_valid(&self) -> bool {
        self.0 != 0
    }
}

impl std::fmt::Display for NameId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NameId({})", self.0)
    }
}

// ── NameInternTable ──────────────────────────────────

/// Thread-safe name intern table.
///
/// Names are stored once and referenced by compact 2-byte IDs.
/// The table uses double-checked locking: a fast read-lock path for
/// already-interned names, falling back to a write lock only on first
/// insertion.
pub struct NameInternTable {
    /// Index 0 is reserved for the empty/invalid sentinel.
    names: RwLock<Vec<String>>,
    /// Reverse lookup: name string → intern ID.
    lookup: RwLock<HashMap<String, u16>>,
    /// Most names held; at most [`MAX_INTERNED_NAMES`].
    limit: usize,
    /// Set once the table first refuses a name, so the warning fires once.
    full_warned: AtomicBool,
}

impl NameInternTable {
    /// Create a new, empty intern table with index 0 reserved.
    pub fn new() -> Self {
        Self::with_limit(MAX_INTERNED_NAMES)
    }

    /// Table that holds at most `limit` names (capped at
    /// [`MAX_INTERNED_NAMES`]). Lets tests fill the table quickly.
    pub(crate) fn with_limit(limit: usize) -> Self {
        let mut names = Vec::with_capacity(512);
        names.push(String::new()); // ID 0 = invalid/empty
        Self {
            names: RwLock::new(names),
            lookup: RwLock::new(HashMap::with_capacity(512)),
            limit: limit.min(MAX_INTERNED_NAMES),
            full_warned: AtomicBool::new(false),
        }
    }

    /// Intern a name, returning its ID.
    ///
    /// If the name is already interned the existing ID is returned without
    /// allocation. Thread-safe via double-checked locking.
    ///
    /// When the table is full a new name is not stored and
    /// [`NameId::INVALID`] is returned (R-47: the old `assert!` aborted the
    /// process). The component keeps its name either way; the table is only
    /// a lookup and statistics layer. The caller decides what to do about a
    /// full table: [`Self::retain`] drops names no longer in use, and
    /// [`Self::warn_full_once`] reports it.
    pub fn intern(&self, name: &str) -> NameId {
        // Fast path: read lock only.
        {
            let lookup = crate::lock::read_or_recover(&self.lookup, "SOX name table");
            if let Some(&id) = lookup.get(name) {
                return NameId(id);
            }
        }
        // Slow path: acquire write locks and double-check.
        let mut names = crate::lock::write_or_recover(&self.names, "SOX name table");
        let mut lookup = crate::lock::write_or_recover(&self.lookup, "SOX name table");
        if let Some(&id) = lookup.get(name) {
            return NameId(id);
        }
        // names[0] is the sentinel, so the next ID is names.len().
        let next = names.len();
        let id = match u16::try_from(next) {
            Ok(id) if next <= self.limit => id,
            _ => return NameId::INVALID,
        };
        names.push(name.to_string());
        lookup.insert(name.to_string(), id);
        NameId(id)
    }

    /// Keep only the names `keep` accepts and renumber them in their
    /// current order. IDs change, which is safe because nothing outside the
    /// table stores a [`NameId`]; anything that does must re-intern.
    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        let mut names = crate::lock::write_or_recover(&self.names, "SOX name table");
        let mut lookup = crate::lock::write_or_recover(&self.lookup, "SOX name table");
        names.retain(|n| n.is_empty() || keep(n));
        lookup.clear();
        for (i, n) in names.iter().enumerate().skip(1) {
            // INVARIANT: retain only removes, so len stays <= limit + 1 and
            // every index fits in u16; try_from keeps it checked anyway.
            if let Ok(id) = u16::try_from(i) {
                lookup.insert(n.clone(), id);
            }
        }
    }

    /// Log once per table that it is full even with unused names dropped.
    pub fn warn_full_once(&self) {
        if !self.full_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                limit = self.limit,
                "SOX name table full of names in use: new names are not interned (components keep their names)"
            );
        }
    }

    /// Resolve a [`NameId`] to its string.
    ///
    /// Returns an empty string for invalid or out-of-range IDs.
    pub fn resolve(&self, id: NameId) -> String {
        let names = crate::lock::read_or_recover(&self.names, "SOX name table");
        names.get(id.0 as usize).cloned().unwrap_or_default()
    }

    /// Check whether a name has already been interned.
    pub fn contains(&self, name: &str) -> bool {
        let lookup = crate::lock::read_or_recover(&self.lookup, "SOX name table");
        lookup.contains_key(name)
    }

    /// Look up the ID for an already-interned name.
    ///
    /// Returns `None` if the name has not been interned.
    pub fn get_id(&self, name: &str) -> Option<NameId> {
        let lookup = crate::lock::read_or_recover(&self.lookup, "SOX name table");
        lookup.get(name).map(|&id| NameId(id))
    }

    /// Validate a component name against Sedona rules.
    ///
    /// Returns `None` if valid, `Some(error_message)` if invalid.
    ///
    /// Rules:
    /// - Non-empty
    /// - At most [`MAX_NAME_LEN`] characters (31)
    /// - First character must be ASCII alphabetic
    /// - Remaining characters must be ASCII alphanumeric or underscore
    pub fn validate_name(name: &str) -> Option<&'static str> {
        if name.is_empty() {
            return Some("name cannot be empty");
        }
        if name.len() > MAX_NAME_LEN {
            return Some("name too long (max 31 chars)");
        }
        let bytes = name.as_bytes();
        if !bytes[0].is_ascii_alphabetic() {
            return Some("name must start with a letter");
        }
        for &b in &bytes[1..] {
            if !b.is_ascii_alphanumeric() && b != b'_' {
                return Some("name can only contain letters, numbers, and underscores");
            }
        }
        None
    }

    /// Number of interned names (excluding the reserved empty entry).
    pub fn len(&self) -> usize {
        crate::lock::read_or_recover(&self.names, "SOX name table").len() - 1
    }

    /// Whether the table contains no interned names.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Statistics: `(name_count, total_bytes, avg_length)`.
    pub fn stats(&self) -> (usize, usize, f32) {
        let names = crate::lock::read_or_recover(&self.names, "SOX name table");
        let count = names.len() - 1;
        let total_bytes: usize = names[1..].iter().map(|s| s.len()).sum();
        let avg = if count > 0 {
            total_bytes as f32 / count as f32
        } else {
            0.0
        };
        (count, total_bytes, avg)
    }

    /// Return all interned `(NameId, String)` pairs (for debugging / serialisation).
    pub fn all_names(&self) -> Vec<(NameId, String)> {
        let names = crate::lock::read_or_recover(&self.names, "SOX name table");
        names
            .iter()
            .enumerate()
            .skip(1)
            .filter_map(|(i, n)| u16::try_from(i).ok().map(|id| (NameId(id), n.clone())))
            .collect()
    }
}

impl Default for NameInternTable {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- NameId basics --

    #[test]
    fn name_id_invalid_is_zero() {
        assert_eq!(NameId::INVALID, NameId(0));
        assert!(!NameId::INVALID.is_valid());
    }

    #[test]
    fn name_id_valid() {
        assert!(NameId(1).is_valid());
        assert!(NameId(42).is_valid());
    }

    #[test]
    fn name_id_display() {
        assert_eq!(format!("{}", NameId(7)), "NameId(7)");
    }

    // -- Intern & resolve --

    #[test]
    fn intern_and_resolve_roundtrip() {
        let table = NameInternTable::new();
        let id = table.intern("Fan1");
        assert!(id.is_valid());
        assert_eq!(table.resolve(id), "Fan1");
    }

    #[test]
    fn duplicate_intern_returns_same_id() {
        let table = NameInternTable::new();
        let id1 = table.intern("Temp3");
        let id2 = table.intern("Temp3");
        assert_eq!(id1, id2);
    }

    #[test]
    fn different_names_get_different_ids() {
        let table = NameInternTable::new();
        let a = table.intern("alpha");
        let b = table.intern("beta");
        assert_ne!(a, b);
    }

    #[test]
    fn resolve_invalid_returns_empty() {
        let table = NameInternTable::new();
        assert_eq!(table.resolve(NameId::INVALID), "");
        assert_eq!(table.resolve(NameId(9999)), "");
    }

    // -- contains / get_id --

    #[test]
    fn contains_and_get_id() {
        let table = NameInternTable::new();
        assert!(!table.contains("missing"));
        assert!(table.get_id("missing").is_none());

        let id = table.intern("present");
        assert!(table.contains("present"));
        assert_eq!(table.get_id("present"), Some(id));
    }

    // -- validate_name --

    #[test]
    fn validate_name_valid_names() {
        assert!(NameInternTable::validate_name("a").is_none());
        assert!(NameInternTable::validate_name("myComp").is_none());
        assert!(NameInternTable::validate_name("comp_1").is_none());
        assert!(NameInternTable::validate_name("Add2").is_none());
        assert!(NameInternTable::validate_name("Z").is_none());
        // Exactly 31 chars
        let exact = "a".repeat(31);
        assert!(NameInternTable::validate_name(&exact).is_none());
    }

    #[test]
    fn validate_name_empty() {
        assert_eq!(
            NameInternTable::validate_name(""),
            Some("name cannot be empty")
        );
    }

    #[test]
    fn validate_name_too_long() {
        let long = "a".repeat(32);
        assert_eq!(
            NameInternTable::validate_name(&long),
            Some("name too long (max 31 chars)")
        );
    }

    #[test]
    fn validate_name_bad_first_char() {
        assert_eq!(
            NameInternTable::validate_name("1comp"),
            Some("name must start with a letter")
        );
        assert_eq!(
            NameInternTable::validate_name("_comp"),
            Some("name must start with a letter")
        );
    }

    #[test]
    fn validate_name_invalid_chars() {
        assert_eq!(
            NameInternTable::validate_name("my-comp"),
            Some("name can only contain letters, numbers, and underscores")
        );
        assert_eq!(
            NameInternTable::validate_name("a b"),
            Some("name can only contain letters, numbers, and underscores")
        );
        assert_eq!(
            NameInternTable::validate_name("a.b"),
            Some("name can only contain letters, numbers, and underscores")
        );
    }

    // -- len / is_empty / stats --

    #[test]
    fn len_and_is_empty() {
        let table = NameInternTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);

        table.intern("x");
        assert!(!table.is_empty());
        assert_eq!(table.len(), 1);

        table.intern("y");
        assert_eq!(table.len(), 2);

        // Duplicate does not increase count.
        table.intern("x");
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn stats_computation() {
        let table = NameInternTable::new();
        assert_eq!(table.stats(), (0, 0, 0.0));

        table.intern("ab"); // 2 bytes
        table.intern("cdef"); // 4 bytes
        let (count, total, avg) = table.stats();
        assert_eq!(count, 2);
        assert_eq!(total, 6);
        assert!((avg - 3.0).abs() < f32::EPSILON);
    }

    // -- all_names --

    #[test]
    fn all_names_returns_interned_pairs() {
        let table = NameInternTable::new();
        let id_a = table.intern("alpha");
        let id_b = table.intern("beta");
        let all = table.all_names();
        assert_eq!(all.len(), 2);
        assert!(all.contains(&(id_a, "alpha".to_string())));
        assert!(all.contains(&(id_b, "beta".to_string())));
    }

    // -- Full table (R-47) --

    #[test]
    fn full_table_refuses_new_names_without_panicking() {
        let table = NameInternTable::with_limit(3);
        let ids: Vec<NameId> = ["a", "b", "c"].iter().map(|n| table.intern(n)).collect();
        assert!(ids.iter().all(NameId::is_valid));

        // The old code asserted here once the table ran out of IDs.
        assert_eq!(table.intern("d"), NameId::INVALID);
        assert_eq!(table.intern("e"), NameId::INVALID);
        assert_eq!(table.len(), 3);
        assert!(!table.contains("d"));

        // Names already in the table keep resolving.
        assert_eq!(table.intern("b"), ids[1]);
        assert_eq!(table.resolve(ids[2]), "c");
    }

    #[test]
    fn interning_past_the_id_space_does_not_panic() {
        // Real size: the old assert! fired on the 65 535th distinct name.
        let table = NameInternTable::new();
        for i in 0..MAX_INTERNED_NAMES {
            assert!(table.intern(&format!("n{i}")).is_valid());
        }
        assert_eq!(table.intern("oneTooMany"), NameId::INVALID);
        assert_eq!(table.len(), MAX_INTERNED_NAMES);
    }

    #[test]
    fn retain_drops_unused_names_and_frees_room() {
        let table = NameInternTable::with_limit(3);
        for n in ["a", "b", "c"] {
            table.intern(n);
        }
        assert_eq!(table.intern("d"), NameId::INVALID);

        table.retain(|n| n != "b");
        assert_eq!(table.len(), 2);
        assert!(!table.contains("b"));
        // Renumbered in order, lookup and resolve agree.
        for n in ["a", "c"] {
            let id = table.get_id(n).unwrap();
            assert_eq!(table.resolve(id), n);
        }
        assert!(table.intern("d").is_valid());
        assert_eq!(table.intern("e"), NameId::INVALID);
    }

    #[test]
    fn limit_is_capped_at_the_id_space() {
        let table = NameInternTable::with_limit(usize::MAX);
        assert_eq!(table.limit, MAX_INTERNED_NAMES);
        assert_eq!(MAX_INTERNED_NAMES, 65_534);
    }

    // -- Thread safety --

    #[test]
    fn concurrent_intern_from_multiple_threads() {
        use std::sync::Arc;
        use std::thread;

        let table = Arc::new(NameInternTable::new());
        let mut handles = Vec::new();

        // 8 threads each intern the same 100 names
        for t in 0..8 {
            let table = table.clone();
            handles.push(thread::spawn(move || {
                let mut ids = Vec::new();
                for i in 0..100 {
                    let name = format!("name_{i}");
                    ids.push((name, table.intern(&format!("name_{i}"))));
                }
                // Also intern some thread-specific names
                table.intern(&format!("thread_{t}"));
                ids
            }));
        }

        // Collect all thread results.
        let mut all_ids: Vec<Vec<(String, NameId)>> = Vec::new();
        for h in handles {
            all_ids.push(h.join().unwrap());
        }

        // All threads must agree on the ID for each name.
        for i in 0..100 {
            let expected = all_ids[0][i].1;
            for thread_ids in &all_ids[1..] {
                assert_eq!(thread_ids[i].1, expected, "disagreement on name_{i}");
            }
        }

        // 100 shared names + 8 thread-specific names = 108
        assert_eq!(table.len(), 108);
    }
}
