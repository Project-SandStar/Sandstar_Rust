//! String table for roxWarp tag name compression.
//!
//! Frequently used tag names are assigned 1-byte indices (0-255) for compact
//! MessagePack encoding. During handshake, peers exchange their string tables
//! and merge them so both sides use consistent indices.
//!
//! See research doc §2.5: "String Table Optimization"

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ── Default Entries ─────────────────────────────────

/// Default string table entries — always available without negotiation.
/// Covers the most common roxWarp tag names.
pub const DEFAULT_TABLE: &[&str] = &[
    "type",         // 0
    "nodeId",       // 1
    "version",      // 2
    "versions",     // 3
    "fromVersion",  // 4
    "toVersion",    // 5
    "points",       // 6
    "channel",      // 7
    "value",        // 8
    "unit",         // 9
    "status",       // 10
    "timestamp",    // 11
    "capabilities", // 12
    "load",         // 13
    "wantFrom",     // 14
    "address",      // 15
];

// ── StringTable ─────────────────────────────────────

/// Bidirectional string table for tag name compression.
///
/// Maps tag name strings to 1-byte indices and back. Up to 256 entries.
/// Used in binary Trio encoding to replace repeated tag name strings
/// with single-byte indices, reducing message size by ~35%.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StringTable {
    /// Index -> name
    by_index: Vec<String>,
    /// Name -> index
    #[serde(skip)]
    by_name: HashMap<String, u8>,
}

impl StringTable {
    /// Create with default entries.
    pub fn new() -> Self {
        let mut table = Self {
            by_index: Vec::with_capacity(256),
            by_name: HashMap::with_capacity(256),
        };
        for &name in DEFAULT_TABLE {
            table.add(name.to_string());
        }
        table
    }

    /// Add an entry, returns its index (or existing index if already present).
    ///
    /// Returns `None` if the table is full (256 entries).
    pub fn add(&mut self, name: String) -> Option<u8> {
        if let Some(&idx) = self.by_name.get(&name) {
            return Some(idx);
        }
        if self.by_index.len() >= 256 {
            return None; // table full
        }
        let idx = self.by_index.len() as u8;
        self.by_name.insert(name.clone(), idx);
        self.by_index.push(name);
        Some(idx)
    }

    /// Encode: get index for a name (`None` if not in table).
    pub fn encode(&self, name: &str) -> Option<u8> {
        self.by_name.get(name).copied()
    }

    /// Decode: get name for an index.
    pub fn decode(&self, idx: u8) -> Option<&str> {
        self.by_index.get(idx as usize).map(|s| s.as_str())
    }

    /// Serialize table entries for exchange during handshake.
    pub fn to_entries(&self) -> Vec<String> {
        self.by_index.clone()
    }

    /// Merge additional entries from a peer's table.
    ///
    /// Any entries not already present are appended. Returns the number
    /// of new entries added.
    pub fn merge(&mut self, peer_entries: &[String]) -> usize {
        let mut added = 0;
        for entry in peer_entries {
            if !self.by_name.contains_key(entry) && self.add(entry.clone()).is_some() {
                added += 1;
            }
        }
        added
    }

    /// Number of entries in the table.
    pub fn len(&self) -> usize {
        self.by_index.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.by_index.is_empty()
    }

    /// Rebuild the `by_name` index after deserialization.
    ///
    /// Called automatically when constructing from serialized data
    /// (since `by_name` is `#[serde(skip)]`).
    pub fn rebuild_index(&mut self) {
        self.by_name.clear();
        for (idx, name) in self.by_index.iter().enumerate() {
            self.by_name.insert(name.clone(), idx as u8);
        }
    }
}

impl Default for StringTable {
    fn default() -> Self {
        Self::new()
    }
}

// ── Binary Trio integration ─────────────────────────

use super::binary_trio::TrioDict;

/// Prefix used to mark string-table-indexed keys in encoded dicts.
/// Keys starting with `~` followed by a decimal index are table references.
const TABLE_KEY_PREFIX: char = '~';

/// Encode a TrioDict with string table compression.
///
/// Keys that appear in the string table are replaced with `~<index>`
/// (e.g., `"~0"` for `"type"`, `"~8"` for `"value"`). Keys not in
/// the table are kept as regular strings.
pub fn encode_with_table(
    dict: &TrioDict,
    table: &StringTable,
) -> Result<Vec<u8>, super::RoxWarpError> {
    let mut compact: HashMap<String, &super::binary_trio::TrioValue> =
        HashMap::with_capacity(dict.len());

    for (key, val) in dict {
        if let Some(idx) = table.encode(key) {
            let compact_key = format!("{TABLE_KEY_PREFIX}{idx}");
            compact.insert(compact_key, val);
        } else {
            compact.insert(key.clone(), val);
        }
    }

    rmp_serde::to_vec(&compact).map_err(|e| super::RoxWarpError::Encode(e.to_string()))
}

/// Decode a TrioDict that was encoded with string table compression.
///
/// Keys starting with `~` followed by a number are resolved back to
/// full tag name strings via the string table.
pub fn decode_with_table(
    bytes: &[u8],
    table: &StringTable,
) -> Result<TrioDict, super::RoxWarpError> {
    let raw: HashMap<String, super::binary_trio::TrioValue> =
        rmp_serde::from_slice(bytes)
            .map_err(|e| super::RoxWarpError::Decode(e.to_string()))?;

    let mut dict = TrioDict::new();
    for (key, val) in raw {
        if let Some(idx_str) = key.strip_prefix(TABLE_KEY_PREFIX) {
            if let Ok(idx) = idx_str.parse::<u8>() {
                if let Some(name) = table.decode(idx) {
                    dict.insert(name.to_string(), val);
                    continue;
                }
            }
        }
        dict.insert(key, val);
    }
    Ok(dict)
}

// ── Tests ───────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roxwarp::binary_trio::TrioValue;

    #[test]
    fn new_table_has_defaults() {
        let table = StringTable::new();
        assert_eq!(table.len(), DEFAULT_TABLE.len());
        assert_eq!(table.encode("type"), Some(0));
        assert_eq!(table.encode("nodeId"), Some(1));
        assert_eq!(table.encode("address"), Some(15));
        assert_eq!(table.decode(0), Some("type"));
        assert_eq!(table.decode(15), Some("address"));
    }

    #[test]
    fn add_new_entry() {
        let mut table = StringTable::new();
        let initial = table.len();
        let idx = table.add("curVal".to_string());
        assert_eq!(idx, Some(initial as u8));
        assert_eq!(table.encode("curVal"), Some(initial as u8));
        assert_eq!(table.decode(initial as u8), Some("curVal"));
        assert_eq!(table.len(), initial + 1);
    }

    #[test]
    fn add_existing_entry_returns_same_index() {
        let mut table = StringTable::new();
        let idx1 = table.add("type".to_string());
        let idx2 = table.add("type".to_string());
        assert_eq!(idx1, idx2);
        assert_eq!(idx1, Some(0));
        assert_eq!(table.len(), DEFAULT_TABLE.len());
    }

    #[test]
    fn encode_decode_roundtrip() {
        let table = StringTable::new();
        for (i, &name) in DEFAULT_TABLE.iter().enumerate() {
            assert_eq!(table.encode(name), Some(i as u8));
            assert_eq!(table.decode(i as u8), Some(name));
        }
    }

    #[test]
    fn unknown_name_returns_none() {
        let table = StringTable::new();
        assert_eq!(table.encode("nonexistent"), None);
    }

    #[test]
    fn unknown_index_returns_none() {
        let table = StringTable::new();
        assert_eq!(table.decode(255), None);
    }

    #[test]
    fn merge_adds_new_entries() {
        let mut table = StringTable::new();
        let initial = table.len();
        let peer_entries = vec![
            "type".to_string(),     // already exists
            "curVal".to_string(),   // new
            "navName".to_string(),  // new
        ];
        let added = table.merge(&peer_entries);
        assert_eq!(added, 2);
        assert_eq!(table.len(), initial + 2);
        assert!(table.encode("curVal").is_some());
        assert!(table.encode("navName").is_some());
    }

    #[test]
    fn merge_no_duplicates() {
        let mut table = StringTable::new();
        let entries: Vec<String> = DEFAULT_TABLE.iter().map(|s| s.to_string()).collect();
        let added = table.merge(&entries);
        assert_eq!(added, 0);
        assert_eq!(table.len(), DEFAULT_TABLE.len());
    }

    #[test]
    fn to_entries_roundtrip() {
        let table = StringTable::new();
        let entries = table.to_entries();
        assert_eq!(entries.len(), DEFAULT_TABLE.len());
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry, DEFAULT_TABLE[i]);
        }
    }

    #[test]
    fn table_full_returns_none() {
        let mut table = StringTable {
            by_index: Vec::new(),
            by_name: HashMap::new(),
        };
        // Fill to 256
        for i in 0..256u16 {
            assert!(table.add(format!("tag_{i}")).is_some());
        }
        assert_eq!(table.len(), 256);
        // 257th should fail
        assert_eq!(table.add("overflow".to_string()), None);
    }

    #[test]
    fn default_impl() {
        let table = StringTable::default();
        assert_eq!(table.len(), DEFAULT_TABLE.len());
    }

    #[test]
    fn rebuild_index() {
        let mut table = StringTable::new();
        table.add("extra".to_string());
        // Simulate deserialize (by_name would be empty)
        let mut deserialized = StringTable {
            by_index: table.by_index.clone(),
            by_name: HashMap::new(),
        };
        assert_eq!(deserialized.encode("type"), None); // broken
        deserialized.rebuild_index();
        assert_eq!(deserialized.encode("type"), Some(0)); // fixed
        assert!(deserialized.encode("extra").is_some());
    }

    #[test]
    fn encode_decode_with_table_roundtrip() {
        let table = StringTable::new();

        let mut dict = TrioDict::new();
        dict.insert("type".into(), TrioValue::str("cov"));
        dict.insert("value".into(), TrioValue::number(73.2));
        dict.insert("custom_tag".into(), TrioValue::str("hello"));

        let bytes = encode_with_table(&dict, &table).unwrap();
        let decoded = decode_with_table(&bytes, &table).unwrap();

        assert_eq!(decoded.len(), 3);
        assert_eq!(
            decoded.get("type").unwrap(),
            &TrioValue::Str { val: "cov".into() }
        );
        assert_eq!(
            decoded.get("value").unwrap(),
            &TrioValue::Number { val: 73.2 }
        );
        assert_eq!(
            decoded.get("custom_tag").unwrap(),
            &TrioValue::Str { val: "hello".into() }
        );
    }

    #[test]
    fn encoded_with_table_is_smaller() {
        let table = StringTable::new();

        let mut dict = TrioDict::new();
        dict.insert("type".into(), TrioValue::str("cov"));
        dict.insert("nodeId".into(), TrioValue::str("node-a"));
        dict.insert("version".into(), TrioValue::int(42));
        dict.insert("timestamp".into(), TrioValue::int(1706000000));
        dict.insert("channel".into(), TrioValue::int(1113));
        dict.insert("value".into(), TrioValue::number(73.2));
        dict.insert("unit".into(), TrioValue::str("degF"));
        dict.insert("status".into(), TrioValue::str("ok"));

        let without_table = crate::roxwarp::binary_trio::encode(&dict).unwrap();
        let with_table = encode_with_table(&dict, &table).unwrap();

        assert!(
            with_table.len() < without_table.len(),
            "with table ({}) should be smaller than without ({})",
            with_table.len(),
            without_table.len()
        );
    }

    #[test]
    fn is_empty() {
        let empty = StringTable {
            by_index: Vec::new(),
            by_name: HashMap::new(),
        };
        assert!(empty.is_empty());
        assert!(!StringTable::new().is_empty());
    }
}
