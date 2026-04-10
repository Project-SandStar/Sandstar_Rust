//! Trio text encoding/decoding for RoWS and REST API content negotiation.
//!
//! Trio is the simple text format used by Project Haystack for tag-value pairs.
//! Each record is a set of `key:value` lines, with blank lines separating records.
//!
//! # Trio Encoding Rules
//!
//! - `key:value` format (colon separator, no space required)
//! - Marker tags: just the key name alone (no colon)
//! - Strings: `key:value` (no quotes needed unless special)
//! - Numbers: `key:72.5` or `key:72.5°F` (with unit)
//! - Bools: `key:true` or `key:false`
//! - Refs: `key:@ref-id "dis name"`
//! - Null: `key:N`
//! - Multi-line strings: `key:\n  line1\n  line2` (indented continuation)
//! - Record separator: blank line
//! - Comments: `// comment`

use std::collections::HashMap;
use std::fmt::Write;

use serde_json::Value;

// ── MIME type ────────────────────────────────────────

/// MIME type for Trio text format.
pub const TRIO_CONTENT_TYPE: &str = "text/trio";

// ── Encode ───────────────────────────────────────────

/// Encode a HashMap of tags to Trio text format.
///
/// JSON values are mapped to Trio encoding as follows:
/// - `null` → `key:N`
/// - `true` → bare `key` (marker tag)
/// - `false` → omitted (absent marker)
/// - number → `key:number`
/// - string → `key:value`
/// - array/object → `key:json_representation`
pub fn encode_trio(tags: &HashMap<String, Value>) -> String {
    let mut out = String::new();

    // Sort keys for deterministic output
    let mut keys: Vec<&String> = tags.keys().collect();
    keys.sort();

    for key in keys {
        let value = &tags[key];
        match value {
            Value::Null => {
                let _ = writeln!(out, "{key}:N");
            }
            Value::Bool(true) => {
                // Marker tag: bare key name
                let _ = writeln!(out, "{key}");
            }
            Value::Bool(false) => {
                // Absent — omit from output
            }
            Value::Number(n) => {
                let _ = writeln!(out, "{key}:{n}");
            }
            Value::String(s) => {
                // Check for multi-line strings
                if s.contains('\n') {
                    let _ = writeln!(out, "{key}:");
                    for line in s.lines() {
                        let _ = writeln!(out, "  {line}");
                    }
                } else {
                    let _ = writeln!(out, "{key}:{s}");
                }
            }
            Value::Array(_) | Value::Object(_) => {
                let _ = writeln!(out, "{key}:{value}");
            }
        }
    }
    out
}

/// Encode multiple records to Trio text format (blank-line separated).
pub fn encode_trio_records(records: &[HashMap<String, Value>]) -> String {
    let mut out = String::new();
    for (i, record) in records.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&encode_trio(record));
    }
    out
}

// ── Decode ───────────────────────────────────────────

/// Decode Trio text format to a HashMap of tag-value pairs.
///
/// A single record (first record if multiple present).
pub fn decode_trio(text: &str) -> HashMap<String, Value> {
    let records = decode_trio_records(text);
    records.into_iter().next().unwrap_or_default()
}

/// Decode Trio text with multiple records (separated by blank lines).
pub fn decode_trio_records(text: &str) -> Vec<HashMap<String, Value>> {
    let mut records = Vec::new();
    let mut current: HashMap<String, Value> = HashMap::new();
    let mut multiline_key: Option<String> = None;
    let mut multiline_buf = String::new();

    for line in text.lines() {
        // Handle multi-line continuation (indented with 2+ spaces)
        if let Some(ref key) = multiline_key {
            if line.starts_with("  ") {
                if !multiline_buf.is_empty() {
                    multiline_buf.push('\n');
                }
                multiline_buf.push_str(line.trim_start());
                continue;
            } else {
                // End of multi-line value
                current.insert(key.clone(), Value::String(multiline_buf.clone()));
                multiline_key = None;
                multiline_buf.clear();
            }
        }

        let trimmed = line.trim();

        // Blank line = record separator
        if trimmed.is_empty() {
            if !current.is_empty() {
                records.push(current);
                current = HashMap::new();
            }
            continue;
        }

        // Skip comments
        if trimmed.starts_with("//") {
            continue;
        }

        // Parse key:value or bare marker
        if let Some(colon) = trimmed.find(':') {
            let key = trimmed[..colon].trim().to_string();
            let val_str = trimmed[colon + 1..].trim();

            if val_str.is_empty() {
                // Start of multi-line value
                multiline_key = Some(key);
                multiline_buf.clear();
            } else {
                current.insert(key, parse_trio_value(val_str));
            }
        } else {
            // Bare tag name = Marker (true)
            current.insert(trimmed.to_string(), Value::Bool(true));
        }
    }

    // Flush any pending multi-line value
    if let Some(key) = multiline_key {
        current.insert(key, Value::String(multiline_buf));
    }

    // Flush last record
    if !current.is_empty() {
        records.push(current);
    }

    records
}

/// Parse a single Trio value string into a JSON Value.
fn parse_trio_value(val: &str) -> Value {
    // Null
    if val == "N" {
        return Value::Null;
    }
    // Bool
    if val == "true" {
        return Value::Bool(true);
    }
    if val == "false" {
        return Value::Bool(false);
    }
    // Ref: @ref-id or @ref-id "dis name"
    if val.starts_with('@') {
        return Value::String(val.to_string());
    }
    // Date: YYYY-MM-DD
    if val.len() == 10
        && val.chars().nth(4) == Some('-')
        && val.chars().nth(7) == Some('-')
        && val[..4].parse::<u16>().is_ok()
        && val[5..7].parse::<u8>().is_ok()
        && val[8..10].parse::<u8>().is_ok()
    {
        return Value::String(val.to_string());
    }
    // Time: HH:MM:SS
    if val.len() >= 8
        && val.chars().nth(2) == Some(':')
        && val.chars().nth(5) == Some(':')
        && val[..2].parse::<u8>().is_ok()
    {
        return Value::String(val.to_string());
    }
    // Number (possibly with unit suffix)
    // Try pure number first
    if let Ok(n) = val.parse::<i64>() {
        return serde_json::json!(n);
    }
    if let Ok(n) = val.parse::<f64>() {
        return serde_json::json!(n);
    }
    // Number with unit: find where digits end and unit begins
    if let Some(num_end) = val.find(|c: char| {
        !c.is_ascii_digit() && c != '.' && c != '-' && c != '+' && c != 'e' && c != 'E'
    }) {
        if num_end > 0 {
            if let Ok(_n) = val[..num_end].parse::<f64>() {
                // Return as string with unit preserved (e.g., "72.5°F")
                return Value::String(val.to_string());
            }
        }
    }
    // Default: string
    Value::String(val.to_string())
}

// ══════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Encode tests ─────────────────────────────────

    #[test]
    fn encode_marker_tag() {
        let mut tags = HashMap::new();
        tags.insert("point".to_string(), json!(true));
        let text = encode_trio(&tags);
        assert_eq!(text.trim(), "point");
    }

    #[test]
    fn encode_false_tag_omitted() {
        let mut tags = HashMap::new();
        tags.insert("hidden".to_string(), json!(false));
        let text = encode_trio(&tags);
        assert!(text.trim().is_empty(), "false tags should be omitted");
    }

    #[test]
    fn encode_null_value() {
        let mut tags = HashMap::new();
        tags.insert("empty".to_string(), Value::Null);
        let text = encode_trio(&tags);
        assert_eq!(text.trim(), "empty:N");
    }

    #[test]
    fn encode_number() {
        let mut tags = HashMap::new();
        tags.insert("curVal".to_string(), json!(72.5));
        let text = encode_trio(&tags);
        assert_eq!(text.trim(), "curVal:72.5");
    }

    #[test]
    fn encode_integer() {
        let mut tags = HashMap::new();
        tags.insert("count".to_string(), json!(42));
        let text = encode_trio(&tags);
        assert_eq!(text.trim(), "count:42");
    }

    #[test]
    fn encode_string() {
        let mut tags = HashMap::new();
        tags.insert("dis".to_string(), json!("Zone Temperature"));
        let text = encode_trio(&tags);
        assert_eq!(text.trim(), "dis:Zone Temperature");
    }

    #[test]
    fn encode_multiline_string() {
        let mut tags = HashMap::new();
        tags.insert("notes".to_string(), json!("line1\nline2\nline3"));
        let text = encode_trio(&tags);
        assert!(text.contains("notes:"));
        assert!(text.contains("  line1"));
        assert!(text.contains("  line2"));
        assert!(text.contains("  line3"));
    }

    #[test]
    fn encode_sorted_keys() {
        let mut tags = HashMap::new();
        tags.insert("z_last".to_string(), json!(1));
        tags.insert("a_first".to_string(), json!(2));
        let text = encode_trio(&tags);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("a_first"));
        assert!(lines[1].starts_with("z_last"));
    }

    #[test]
    fn encode_multiple_records() {
        let r1 = {
            let mut m = HashMap::new();
            m.insert("dis".to_string(), json!("Sensor 1"));
            m
        };
        let r2 = {
            let mut m = HashMap::new();
            m.insert("dis".to_string(), json!("Sensor 2"));
            m
        };
        let text = encode_trio_records(&[r1, r2]);
        assert!(text.contains("dis:Sensor 1"));
        assert!(text.contains("dis:Sensor 2"));
        // Records separated by blank line
        assert!(text.contains("\n\n"));
    }

    // ── Decode tests ─────────────────────────────────

    #[test]
    fn decode_marker_tag() {
        let text = "point\nsensor";
        let tags = decode_trio(text);
        assert_eq!(tags.get("point"), Some(&json!(true)));
        assert_eq!(tags.get("sensor"), Some(&json!(true)));
    }

    #[test]
    fn decode_null_value() {
        let text = "curVal:N";
        let tags = decode_trio(text);
        assert_eq!(tags.get("curVal"), Some(&Value::Null));
    }

    #[test]
    fn decode_bool_values() {
        let text = "enabled:true\ndisabled:false";
        let tags = decode_trio(text);
        assert_eq!(tags.get("enabled"), Some(&json!(true)));
        assert_eq!(tags.get("disabled"), Some(&json!(false)));
    }

    #[test]
    fn decode_number() {
        let text = "curVal:72.5";
        let tags = decode_trio(text);
        assert_eq!(tags.get("curVal"), Some(&json!(72.5)));
    }

    #[test]
    fn decode_integer() {
        let text = "count:42";
        let tags = decode_trio(text);
        assert_eq!(tags.get("count"), Some(&json!(42)));
    }

    #[test]
    fn decode_negative_number() {
        let text = "offset:-3.5";
        let tags = decode_trio(text);
        assert_eq!(tags.get("offset"), Some(&json!(-3.5)));
    }

    #[test]
    fn decode_ref() {
        let text = "equipRef:@p:abc-123";
        let tags = decode_trio(text);
        assert_eq!(tags.get("equipRef"), Some(&json!("@p:abc-123")));
    }

    #[test]
    fn decode_string_value() {
        let text = "dis:Zone Temperature";
        let tags = decode_trio(text);
        assert_eq!(tags.get("dis"), Some(&json!("Zone Temperature")));
    }

    #[test]
    fn decode_number_with_unit() {
        let text = "curVal:72.5°F";
        let tags = decode_trio(text);
        // Number with unit is stored as string
        assert_eq!(tags.get("curVal"), Some(&json!("72.5°F")));
    }

    #[test]
    fn decode_date() {
        let text = "date:2026-04-07";
        let tags = decode_trio(text);
        assert_eq!(tags.get("date"), Some(&json!("2026-04-07")));
    }

    #[test]
    fn decode_time() {
        let text = "time:12:30:00";
        let tags = decode_trio(text);
        assert_eq!(tags.get("time"), Some(&json!("12:30:00")));
    }

    #[test]
    fn decode_multiline_string() {
        let text = "notes:\n  line one\n  line two\n  line three\ndis:test";
        let tags = decode_trio(text);
        assert_eq!(
            tags.get("notes"),
            Some(&json!("line one\nline two\nline three"))
        );
        assert_eq!(tags.get("dis"), Some(&json!("test")));
    }

    #[test]
    fn decode_comments_skipped() {
        let text = "// this is a comment\npoint\n// another comment\ndis:Test";
        let tags = decode_trio(text);
        assert_eq!(tags.len(), 2);
        assert_eq!(tags.get("point"), Some(&json!(true)));
        assert_eq!(tags.get("dis"), Some(&json!("Test")));
    }

    #[test]
    fn decode_empty_lines_skipped() {
        let text = "\n\npoint\n\n";
        // Single record (blank lines around it)
        let tags = decode_trio(text);
        assert_eq!(tags.get("point"), Some(&json!(true)));
    }

    #[test]
    fn decode_multiple_records() {
        let text = "dis:Sensor 1\npoint\n\ndis:Sensor 2\npoint";
        let records = decode_trio_records(text);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].get("dis"), Some(&json!("Sensor 1")));
        assert_eq!(records[1].get("dis"), Some(&json!("Sensor 2")));
    }

    #[test]
    fn decode_empty_string() {
        let tags = decode_trio("");
        assert!(tags.is_empty());
    }

    // ── Roundtrip tests ──────────────────────────────

    #[test]
    fn roundtrip_basic_tags() {
        let mut original = HashMap::new();
        original.insert("point".to_string(), json!(true));
        original.insert("dis".to_string(), json!("Zone Temp"));
        original.insert("curVal".to_string(), json!(72.5));
        original.insert("empty".to_string(), Value::Null);

        let text = encode_trio(&original);
        let decoded = decode_trio(&text);

        assert_eq!(decoded.get("point"), Some(&json!(true)));
        assert_eq!(decoded.get("dis"), Some(&json!("Zone Temp")));
        assert_eq!(decoded.get("curVal"), Some(&json!(72.5)));
        assert_eq!(decoded.get("empty"), Some(&Value::Null));
    }

    #[test]
    fn roundtrip_integer() {
        let mut original = HashMap::new();
        original.insert("count".to_string(), json!(42));
        let text = encode_trio(&original);
        let decoded = decode_trio(&text);
        assert_eq!(decoded.get("count"), Some(&json!(42)));
    }

    #[test]
    fn roundtrip_marker_and_string() {
        let mut original = HashMap::new();
        original.insert("sensor".to_string(), json!(true));
        original.insert("kind".to_string(), json!("Number"));
        let text = encode_trio(&original);
        let decoded = decode_trio(&text);
        assert_eq!(decoded.get("sensor"), Some(&json!(true)));
        assert_eq!(decoded.get("kind"), Some(&json!("Number")));
    }

    // ── parse_trio_value edge cases ──────────────────

    #[test]
    fn parse_value_scientific_notation() {
        let val = parse_trio_value("1.5e3");
        assert_eq!(val, json!(1500.0));
    }

    #[test]
    fn parse_value_negative_integer() {
        let val = parse_trio_value("-42");
        assert_eq!(val, json!(-42));
    }

    #[test]
    fn parse_value_zero() {
        let val = parse_trio_value("0");
        assert_eq!(val, json!(0));
    }

    #[test]
    fn parse_value_plain_string() {
        let val = parse_trio_value("hello world");
        assert_eq!(val, json!("hello world"));
    }
}
