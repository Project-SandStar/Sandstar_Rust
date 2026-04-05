//! Binary Trio encoder/decoder for roxWarp.
//!
//! Encodes Haystack values as MessagePack maps using `rmp-serde`. Each value
//! in the map is a [`TrioValue`] — a Rust enum that preserves Haystack type
//! fidelity (Marker, Ref, Number+Unit, DateTime, Remove, etc.).
//!
//! # Binary format
//!
//! A binary Trio message is a MessagePack map where keys are tag name strings
//! and values are Haystack-typed scalars. The `rmp-serde` crate handles the
//! low-level MessagePack encoding; our `TrioValue` enum maps Haystack types
//! to serde-compatible representations.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::RoxWarpError;

// ── TrioValue ────────────────────────────────────────

/// Haystack value types for binary Trio encoding.
///
/// Covers the core Haystack scalar types used in roxWarp messaging.
/// Complex types (Grid, Bin) are omitted for Phase 1 — they can be added
/// later without breaking wire compatibility.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_kind")]
pub enum TrioValue {
    /// Haystack `null`.
    #[serde(rename = "null")]
    Null,

    /// Haystack Marker — indicates tag presence.
    #[serde(rename = "marker")]
    Marker,

    /// Haystack NA (not available).
    #[serde(rename = "na")]
    NA,

    /// Haystack Remove — signals tag removal in diffs.
    #[serde(rename = "remove")]
    Remove,

    /// Haystack Bool.
    #[serde(rename = "bool")]
    Bool { val: bool },

    /// Haystack Number (no unit).
    #[serde(rename = "number")]
    Number { val: f64 },

    /// Haystack Number with unit string.
    #[serde(rename = "number:unit")]
    NumberUnit { val: f64, unit: String },

    /// Compact integer (fits in i64 without fractional part).
    #[serde(rename = "int")]
    Int { val: i64 },

    /// Haystack Str.
    #[serde(rename = "str")]
    Str { val: String },

    /// Haystack Ref (id only).
    #[serde(rename = "ref")]
    Ref { id: String },

    /// Haystack Ref with display name.
    #[serde(rename = "ref:dis")]
    RefDis { id: String, dis: String },

    /// Haystack Uri.
    #[serde(rename = "uri")]
    Uri { val: String },

    /// Haystack Date.
    #[serde(rename = "date")]
    Date {
        year: i16,
        month: u8,
        day: u8,
    },

    /// Haystack Time.
    #[serde(rename = "time")]
    Time {
        hour: u8,
        min: u8,
        sec: u8,
        ms: u16,
    },

    /// Haystack DateTime.
    #[serde(rename = "dateTime")]
    DateTime { ms: i64, tz: String },

    /// Haystack Coord.
    #[serde(rename = "coord")]
    Coord { lat: f32, lng: f32 },

    /// Haystack List.
    #[serde(rename = "list")]
    List { vals: Vec<TrioValue> },

    /// Haystack Dict (recursive).
    #[serde(rename = "dict")]
    Dict { tags: HashMap<String, TrioValue> },
}

/// A Trio dict — the fundamental unit of roxWarp messaging.
pub type TrioDict = HashMap<String, TrioValue>;

// ── Constructors ─────────────────────────────────────

impl TrioValue {
    /// Create a Marker value.
    pub fn marker() -> Self {
        Self::Marker
    }

    /// Create a Bool value.
    pub fn bool(val: bool) -> Self {
        Self::Bool { val }
    }

    /// Create a Number value (no unit).
    pub fn number(val: f64) -> Self {
        Self::Number { val }
    }

    /// Create a Number value with a unit string.
    pub fn number_unit(val: f64, unit: impl Into<String>) -> Self {
        Self::NumberUnit {
            val,
            unit: unit.into(),
        }
    }

    /// Create an integer value.
    pub fn int(val: i64) -> Self {
        Self::Int { val }
    }

    /// Create a Str value.
    pub fn str(val: impl Into<String>) -> Self {
        Self::Str { val: val.into() }
    }

    /// Create a Ref value (id only).
    pub fn ref_id(id: impl Into<String>) -> Self {
        Self::Ref { id: id.into() }
    }

    /// Create a Ref value with display name.
    pub fn ref_dis(id: impl Into<String>, dis: impl Into<String>) -> Self {
        Self::RefDis {
            id: id.into(),
            dis: dis.into(),
        }
    }

    /// Create a DateTime value from Unix milliseconds and timezone.
    pub fn date_time(ms: i64, tz: impl Into<String>) -> Self {
        Self::DateTime {
            ms,
            tz: tz.into(),
        }
    }

    /// Create a Remove sentinel (for diff encoding).
    pub fn remove() -> Self {
        Self::Remove
    }
}

impl fmt::Display for TrioValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => write!(f, "N"),
            Self::Marker => write!(f, "M"),
            Self::NA => write!(f, "NA"),
            Self::Remove => write!(f, "R"),
            Self::Bool { val } => write!(f, "{val}"),
            Self::Number { val } => write!(f, "{val}"),
            Self::NumberUnit { val, unit } => write!(f, "{val}{unit}"),
            Self::Int { val } => write!(f, "{val}"),
            Self::Str { val } => write!(f, "\"{val}\""),
            Self::Ref { id } => write!(f, "@{id}"),
            Self::RefDis { id, dis } => write!(f, "@{id} \"{dis}\""),
            Self::Uri { val } => write!(f, "`{val}`"),
            Self::Date { year, month, day } => {
                write!(f, "{year:04}-{month:02}-{day:02}")
            }
            Self::Time { hour, min, sec, ms } => {
                write!(f, "{hour:02}:{min:02}:{sec:02}.{ms:03}")
            }
            Self::DateTime { ms, tz } => write!(f, "{ms} {tz}"),
            Self::Coord { lat, lng } => write!(f, "C({lat},{lng})"),
            Self::List { vals } => {
                write!(f, "[")?;
                for (i, v) in vals.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            Self::Dict { tags } => {
                write!(f, "{{")?;
                for (i, (k, v)) in tags.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{k}:{v}")?;
                }
                write!(f, "}}")
            }
        }
    }
}

// ── MessagePack encode/decode ────────────────────────

/// Encode a TrioDict to MessagePack bytes.
pub fn encode(dict: &TrioDict) -> Result<Vec<u8>, RoxWarpError> {
    rmp_serde::to_vec(dict).map_err(|e| RoxWarpError::Encode(e.to_string()))
}

/// Decode MessagePack bytes to a TrioDict.
pub fn decode(bytes: &[u8]) -> Result<TrioDict, RoxWarpError> {
    rmp_serde::from_slice(bytes).map_err(|e| RoxWarpError::Decode(e.to_string()))
}

/// Encode a single TrioValue to MessagePack bytes.
pub fn encode_value(value: &TrioValue) -> Result<Vec<u8>, RoxWarpError> {
    rmp_serde::to_vec(value).map_err(|e| RoxWarpError::Encode(e.to_string()))
}

/// Decode MessagePack bytes to a single TrioValue.
pub fn decode_value(bytes: &[u8]) -> Result<TrioValue, RoxWarpError> {
    rmp_serde::from_slice(bytes).map_err(|e| RoxWarpError::Decode(e.to_string()))
}

// ── Trio text format ─────────────────────────────────

/// Encode a TrioDict to Trio text format (for debug mode).
///
/// Produces one `key:value` per line, suitable for human inspection
/// and the `?debug=trio` WebSocket query parameter.
pub fn to_trio_text(dict: &TrioDict) -> String {
    let mut lines = Vec::with_capacity(dict.len());
    for (key, val) in dict {
        match val {
            TrioValue::Marker => lines.push(key.clone()),
            _ => lines.push(format!("{key}:{val}")),
        }
    }
    lines.join("\n")
}

/// Decode Trio text format to a TrioDict.
///
/// Parses `key:value` lines. A bare tag name (no colon) is interpreted as
/// a Marker. Values are parsed as best-effort: numbers, booleans, strings.
pub fn from_trio_text(text: &str) -> Result<TrioDict, RoxWarpError> {
    let mut dict = TrioDict::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if let Some(colon_pos) = line.find(':') {
            let key = line[..colon_pos].trim().to_string();
            let val_str = line[colon_pos + 1..].trim();
            let val = parse_trio_scalar(val_str);
            dict.insert(key, val);
        } else {
            // Bare tag name = Marker
            dict.insert(line.to_string(), TrioValue::Marker);
        }
    }
    Ok(dict)
}

/// Best-effort parse of a Trio scalar value string.
fn parse_trio_scalar(s: &str) -> TrioValue {
    // Null
    if s == "N" {
        return TrioValue::Null;
    }
    // Marker
    if s == "M" {
        return TrioValue::Marker;
    }
    // NA
    if s == "NA" {
        return TrioValue::NA;
    }
    // Remove
    if s == "R" {
        return TrioValue::Remove;
    }
    // Bool
    if s == "true" {
        return TrioValue::Bool { val: true };
    }
    if s == "false" {
        return TrioValue::Bool { val: false };
    }
    // Ref
    if let Some(rest) = s.strip_prefix('@') {
        return TrioValue::Ref {
            id: rest.to_string(),
        };
    }
    // Uri
    if s.starts_with('`') && s.ends_with('`') && s.len() >= 2 {
        return TrioValue::Uri {
            val: s[1..s.len() - 1].to_string(),
        };
    }
    // Quoted string
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        return TrioValue::Str {
            val: s[1..s.len() - 1].to_string(),
        };
    }
    // Number (possibly with unit)
    if let Some(num_end) = s.find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+' && c != 'e' && c != 'E') {
        if let Ok(val) = s[..num_end].parse::<f64>() {
            let unit = s[num_end..].trim().to_string();
            if unit.is_empty() {
                return TrioValue::Number { val };
            } else {
                return TrioValue::NumberUnit { val, unit };
            }
        }
    }
    // Pure number
    if let Ok(val) = s.parse::<i64>() {
        return TrioValue::Int { val };
    }
    if let Ok(val) = s.parse::<f64>() {
        return TrioValue::Number { val };
    }
    // Fallback: treat as string
    TrioValue::Str {
        val: s.to_string(),
    }
}

// ── Tests ────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip_basic() {
        let mut dict = TrioDict::new();
        dict.insert("point".into(), TrioValue::Marker);
        dict.insert("dis".into(), TrioValue::str("Zone Temp"));
        dict.insert("curVal".into(), TrioValue::number(72.5));

        let bytes = encode(&dict).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded.get("point").unwrap(), &TrioValue::Marker);
        assert_eq!(
            decoded.get("dis").unwrap(),
            &TrioValue::Str {
                val: "Zone Temp".into()
            }
        );
    }

    #[test]
    fn encode_decode_all_scalar_types() {
        let mut dict = TrioDict::new();
        dict.insert("null_tag".into(), TrioValue::Null);
        dict.insert("marker_tag".into(), TrioValue::Marker);
        dict.insert("na_tag".into(), TrioValue::NA);
        dict.insert("remove_tag".into(), TrioValue::Remove);
        dict.insert("bool_tag".into(), TrioValue::bool(true));
        dict.insert("num_tag".into(), TrioValue::number(42.0));
        dict.insert("num_unit_tag".into(), TrioValue::number_unit(73.2, "degF"));
        dict.insert("int_tag".into(), TrioValue::int(999));
        dict.insert("str_tag".into(), TrioValue::str("hello"));
        dict.insert("ref_tag".into(), TrioValue::ref_id("p:abc-123"));
        dict.insert(
            "ref_dis_tag".into(),
            TrioValue::ref_dis("p:abc-123", "Zone Temp"),
        );
        dict.insert(
            "dt_tag".into(),
            TrioValue::date_time(1706000000_000, "New_York"),
        );
        dict.insert(
            "uri_tag".into(),
            TrioValue::Uri {
                val: "/api/read".into(),
            },
        );
        dict.insert(
            "date_tag".into(),
            TrioValue::Date {
                year: 2026,
                month: 4,
                day: 4,
            },
        );
        dict.insert(
            "time_tag".into(),
            TrioValue::Time {
                hour: 14,
                min: 30,
                sec: 0,
                ms: 0,
            },
        );
        dict.insert(
            "coord_tag".into(),
            TrioValue::Coord {
                lat: 40.7128,
                lng: -74.0060,
            },
        );

        let bytes = encode(&dict).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.len(), dict.len());

        // Verify a few specific values
        assert_eq!(decoded.get("null_tag").unwrap(), &TrioValue::Null);
        assert_eq!(decoded.get("marker_tag").unwrap(), &TrioValue::Marker);
        assert_eq!(decoded.get("remove_tag").unwrap(), &TrioValue::Remove);
        assert_eq!(
            decoded.get("bool_tag").unwrap(),
            &TrioValue::Bool { val: true }
        );

        if let TrioValue::NumberUnit { val, unit } =
            decoded.get("num_unit_tag").unwrap()
        {
            assert!((val - 73.2).abs() < f64::EPSILON);
            assert_eq!(unit, "degF");
        } else {
            panic!("expected NumberUnit");
        }

        if let TrioValue::DateTime { ms, tz } = decoded.get("dt_tag").unwrap() {
            assert_eq!(*ms, 1706000000_000);
            assert_eq!(tz, "New_York");
        } else {
            panic!("expected DateTime");
        }
    }

    #[test]
    fn encode_decode_list() {
        let list = TrioValue::List {
            vals: vec![
                TrioValue::int(1),
                TrioValue::str("two"),
                TrioValue::number(3.0),
            ],
        };
        let mut dict = TrioDict::new();
        dict.insert("items".into(), list.clone());

        let bytes = encode(&dict).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.get("items").unwrap(), &list);
    }

    #[test]
    fn encode_decode_nested_dict() {
        let mut inner = HashMap::new();
        inner.insert("x".into(), TrioValue::int(10));
        inner.insert("y".into(), TrioValue::int(20));

        let mut dict = TrioDict::new();
        dict.insert("point".into(), TrioValue::Marker);
        dict.insert("geo".into(), TrioValue::Dict { tags: inner });

        let bytes = encode(&dict).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.len(), 2);

        if let TrioValue::Dict { tags } = decoded.get("geo").unwrap() {
            assert_eq!(tags.len(), 2);
            assert_eq!(tags.get("x").unwrap(), &TrioValue::Int { val: 10 });
        } else {
            panic!("expected Dict");
        }
    }

    #[test]
    fn encode_value_roundtrip() {
        let val = TrioValue::number_unit(73.2, "degF");
        let bytes = encode_value(&val).unwrap();
        let decoded = decode_value(&bytes).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn msgpack_is_compact() {
        // A typical COV event dict
        let mut dict = TrioDict::new();
        dict.insert("type".into(), TrioValue::str("cov"));
        dict.insert("compId".into(), TrioValue::int(5));
        dict.insert("slotId".into(), TrioValue::int(0));
        dict.insert("value".into(), TrioValue::number_unit(73.2, "degF"));
        dict.insert("ts".into(), TrioValue::int(1706000000));

        let bytes = encode(&dict).unwrap();
        // MessagePack should be significantly smaller than JSON
        let json = serde_json::to_string(&dict).unwrap();
        assert!(
            bytes.len() < json.len(),
            "MessagePack ({} bytes) should be smaller than JSON ({} bytes)",
            bytes.len(),
            json.len()
        );
    }

    #[test]
    fn trio_text_encode() {
        let mut dict = TrioDict::new();
        dict.insert("point".into(), TrioValue::Marker);
        dict.insert("curVal".into(), TrioValue::number_unit(72.5, "degF"));

        let text = to_trio_text(&dict);
        // Should contain both tags
        assert!(text.contains("point"));
        assert!(text.contains("curVal:72.5degF"));
    }

    #[test]
    fn trio_text_decode_markers() {
        let text = "point\ndis:\"Zone Temp\"\ncurVal:72.5degF";
        let dict = from_trio_text(text).unwrap();
        assert_eq!(dict.get("point").unwrap(), &TrioValue::Marker);
        assert_eq!(
            dict.get("dis").unwrap(),
            &TrioValue::Str {
                val: "Zone Temp".into()
            }
        );
        if let TrioValue::NumberUnit { val, unit } = dict.get("curVal").unwrap() {
            assert!((val - 72.5).abs() < f64::EPSILON);
            assert_eq!(unit, "degF");
        } else {
            panic!("expected NumberUnit");
        }
    }

    #[test]
    fn trio_text_decode_various_types() {
        let text = "\
            null_val:N\n\
            marker_val:M\n\
            na_val:NA\n\
            remove_val:R\n\
            bool_val:true\n\
            int_val:42\n\
            float_val:3.14\n\
            ref_val:@p:abc-123\n\
            str_val:\"hello world\"\n\
            uri_val:`/api/read`\n\
            // comment line\n\
            \n\
            bare_marker";
        let dict = from_trio_text(text).unwrap();
        assert_eq!(dict.get("null_val").unwrap(), &TrioValue::Null);
        assert_eq!(dict.get("marker_val").unwrap(), &TrioValue::Marker);
        assert_eq!(dict.get("na_val").unwrap(), &TrioValue::NA);
        assert_eq!(dict.get("remove_val").unwrap(), &TrioValue::Remove);
        assert_eq!(
            dict.get("bool_val").unwrap(),
            &TrioValue::Bool { val: true }
        );
        assert_eq!(dict.get("int_val").unwrap(), &TrioValue::Int { val: 42 });
        assert_eq!(
            dict.get("ref_val").unwrap(),
            &TrioValue::Ref {
                id: "p:abc-123".into()
            }
        );
        assert_eq!(dict.get("bare_marker").unwrap(), &TrioValue::Marker);
    }

    #[test]
    fn trio_text_empty_and_comments() {
        let text = "// just a comment\n\n";
        let dict = from_trio_text(text).unwrap();
        assert!(dict.is_empty());
    }

    #[test]
    fn display_formatting() {
        assert_eq!(format!("{}", TrioValue::Null), "N");
        assert_eq!(format!("{}", TrioValue::Marker), "M");
        assert_eq!(format!("{}", TrioValue::bool(true)), "true");
        assert_eq!(format!("{}", TrioValue::number(42.0)), "42");
        assert_eq!(format!("{}", TrioValue::number_unit(73.2, "degF")), "73.2degF");
        assert_eq!(format!("{}", TrioValue::str("hello")), "\"hello\"");
        assert_eq!(format!("{}", TrioValue::ref_id("abc")), "@abc");
        assert_eq!(
            format!("{}", TrioValue::ref_dis("abc", "My Ref")),
            "@abc \"My Ref\""
        );
    }
}
