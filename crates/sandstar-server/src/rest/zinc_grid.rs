//! Zinc 3.0 wire format: grid builder and serializer.
//!
//! Implements the subset of Zinc needed by Haystack REST ops:
//! `ver:"3.0"` header, typed columns, and row data with markers,
//! numbers, strings, booleans, and date-times.
//!
//! Reference: <https://project-haystack.org/doc/docHaystack/Zinc>

use std::fmt::Write;

/// A Zinc scalar value.
#[derive(Debug, Clone)]
pub enum ZincValue {
    /// Marker tag (present / absent).
    Marker,
    /// Null / missing value.
    Null,
    /// String literal.
    Str(String),
    /// Numeric value with optional unit.
    Num(f64, Option<&'static str>),
    /// Boolean.
    Bool(bool),
    /// Pre-formatted date-time string (ISO 8601 UTC).
    DateTime(String),
}

/// A single column definition.
pub struct ZincColumn {
    pub name: &'static str,
    pub meta: Vec<(&'static str, ZincValue)>,
}

impl ZincColumn {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            meta: Vec::new(),
        }
    }

    pub fn with_meta(name: &'static str, meta: Vec<(&'static str, ZincValue)>) -> Self {
        Self { name, meta }
    }
}

/// A Zinc grid: metadata + columns + rows.
pub struct ZincGrid {
    pub meta: Vec<(&'static str, ZincValue)>,
    pub columns: Vec<ZincColumn>,
    pub rows: Vec<Vec<ZincValue>>,
}

impl Default for ZincGrid {
    fn default() -> Self {
        Self::new()
    }
}

impl ZincGrid {
    pub fn new() -> Self {
        Self {
            meta: Vec::new(),
            columns: Vec::new(),
            rows: Vec::new(),
        }
    }

    /// Serialize the grid to a Zinc 3.0 wire-format string.
    pub fn to_zinc(&self) -> String {
        let mut out = String::with_capacity(1024);

        // ── Header line: ver:"3.0" [meta] ──
        out.push_str("ver:\"3.0\"");
        for (key, val) in &self.meta {
            out.push(' ');
            write_tag_pair(&mut out, key, val);
        }
        out.push('\n');

        // ── Column line ──
        for (i, col) in self.columns.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(col.name);
            for (key, val) in &col.meta {
                out.push(' ');
                write_tag_pair(&mut out, key, val);
            }
        }
        out.push('\n');

        // ── Rows ──
        for row in &self.rows {
            for (i, val) in row.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_zinc_value(&mut out, val);
            }
            out.push('\n');
        }

        out
    }
}

/// Write a tag key-value pair: `key:val` or just `key` for markers.
fn write_tag_pair(out: &mut String, key: &str, val: &ZincValue) {
    out.push_str(key);
    match val {
        ZincValue::Marker => {} // marker = key only, no colon
        _ => {
            out.push(':');
            write_zinc_value(out, val);
        }
    }
}

/// Write a single Zinc scalar value.
fn write_zinc_value(out: &mut String, val: &ZincValue) {
    match val {
        ZincValue::Marker => out.push('M'),
        ZincValue::Null => out.push('N'),
        ZincValue::Str(s) => {
            out.push('"');
            for ch in s.chars() {
                match ch {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    _ => out.push(ch),
                }
            }
            out.push('"');
        }
        ZincValue::Num(n, unit) => {
            if n.is_nan() {
                out.push_str("NaN");
            } else if n.is_infinite() {
                out.push_str(if *n > 0.0 { "INF" } else { "-INF" });
            } else {
                let _ = write!(out, "{}", n);
            }
            if let Some(u) = unit {
                out.push_str(u);
            }
        }
        ZincValue::Bool(b) => {
            out.push_str(if *b { "T" } else { "F" });
        }
        ZincValue::DateTime(dt) => {
            let _ = write!(out, "{}", dt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_grid() {
        let g = ZincGrid {
            meta: vec![],
            columns: vec![ZincColumn::new("empty")],
            rows: vec![],
        };
        let zinc = g.to_zinc();
        assert!(zinc.starts_with("ver:\"3.0\"\n"));
        assert!(zinc.contains("empty\n"));
    }

    #[test]
    fn single_row_grid() {
        let g = ZincGrid {
            meta: vec![],
            columns: vec![
                ZincColumn::new("id"),
                ZincColumn::new("dis"),
                ZincColumn::new("cur"),
            ],
            rows: vec![vec![
                ZincValue::Num(1113.0, None),
                ZincValue::Str("AI1 Thermistor".into()),
                ZincValue::Num(72.5, Some("°F")),
            ]],
        };
        let zinc = g.to_zinc();
        assert!(zinc.contains("id,dis,cur\n"));
        assert!(zinc.contains("1113"));
        assert!(zinc.contains("\"AI1 Thermistor\""));
        assert!(zinc.contains("72.5°F"));
    }

    #[test]
    fn grid_with_meta() {
        let g = ZincGrid {
            meta: vec![(
                "hisStart",
                ZincValue::DateTime("2024-01-01T00:00:00Z".into()),
            )],
            columns: vec![ZincColumn::new("ts"), ZincColumn::new("val")],
            rows: vec![],
        };
        let zinc = g.to_zinc();
        assert!(zinc.starts_with("ver:\"3.0\" hisStart:2024-01-01T00:00:00Z\n"));
    }

    #[test]
    fn marker_and_null() {
        let g = ZincGrid {
            meta: vec![],
            columns: vec![ZincColumn::new("point"), ZincColumn::new("dis")],
            rows: vec![vec![ZincValue::Marker, ZincValue::Null]],
        };
        let zinc = g.to_zinc();
        assert!(zinc.contains("M,N\n"));
    }

    #[test]
    fn bool_values() {
        let g = ZincGrid {
            meta: vec![],
            columns: vec![ZincColumn::new("enabled")],
            rows: vec![vec![ZincValue::Bool(true)], vec![ZincValue::Bool(false)]],
        };
        let zinc = g.to_zinc();
        assert!(zinc.contains("T\n"));
        assert!(zinc.contains("F\n"));
    }

    #[test]
    fn string_escaping() {
        let g = ZincGrid {
            meta: vec![],
            columns: vec![ZincColumn::new("dis")],
            rows: vec![vec![ZincValue::Str("line1\nline2".into())]],
        };
        let zinc = g.to_zinc();
        assert!(zinc.contains("\"line1\\nline2\""));
    }

    #[test]
    fn special_numbers() {
        let g = ZincGrid {
            meta: vec![],
            columns: vec![ZincColumn::new("val")],
            rows: vec![
                vec![ZincValue::Num(f64::NAN, None)],
                vec![ZincValue::Num(f64::INFINITY, None)],
                vec![ZincValue::Num(f64::NEG_INFINITY, None)],
            ],
        };
        let zinc = g.to_zinc();
        assert!(zinc.contains("NaN\n"));
        assert!(zinc.contains("INF\n"));
        assert!(zinc.contains("-INF\n"));
    }

    #[test]
    fn column_meta() {
        let g = ZincGrid {
            meta: vec![],
            columns: vec![ZincColumn::with_meta(
                "val",
                vec![("unit", ZincValue::Str("°F".into()))],
            )],
            rows: vec![],
        };
        let zinc = g.to_zinc();
        assert!(zinc.contains("val unit:\"°F\"\n"));
    }
}
