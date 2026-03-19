//! Zinc 3.0 grid parser.
//!
//! A flat, simple parser modelled after the C `zinc.c` approach:
//! read file into memory, split header/columns/rows, provide typed
//! accessors by tag name.  No full Haystack type system — just enough
//! to load `database.zinc`.

use std::collections::HashMap;
use std::fmt;

/// A parsed Zinc 3.0 grid.
pub struct ZincGrid {
    columns: Vec<String>,
    col_index: HashMap<String, usize>,
    rows: Vec<Vec<String>>,
}

/// Zinc parse errors.
#[derive(Debug)]
pub enum ZincError {
    /// Missing or malformed `ver:"3.0"` header.
    InvalidHeader(String),
    /// No column headers found after the version line.
    NoColumns,
}

impl fmt::Display for ZincError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHeader(s) => write!(f, "invalid zinc header: {s}"),
            Self::NoColumns => write!(f, "no column headers found"),
        }
    }
}

impl std::error::Error for ZincError {}

impl ZincGrid {
    /// Parse a Zinc 3.0 grid from a string.
    pub fn parse(content: &str) -> Result<Self, ZincError> {
        let mut lines = content.lines();

        // 1. Version header
        let ver_line = loop {
            match lines.next() {
                Some(l) if l.trim().is_empty() => continue,
                Some(l) => break l.trim(),
                None => {
                    return Err(ZincError::InvalidHeader("empty input".to_string()));
                }
            }
        };

        if !ver_line.starts_with("ver:") {
            return Err(ZincError::InvalidHeader(ver_line.to_string()));
        }

        // 2. Column header line
        let col_line = loop {
            match lines.next() {
                Some(l) if l.trim().is_empty() => continue,
                Some(l) => break l,
                None => return Err(ZincError::NoColumns),
            }
        };

        let columns: Vec<String> = col_line.split(',').map(|s| s.trim().to_string()).collect();
        if columns.is_empty() || (columns.len() == 1 && columns[0].is_empty()) {
            return Err(ZincError::NoColumns);
        }

        let col_index: HashMap<String, usize> = columns
            .iter()
            .enumerate()
            .map(|(i, name)| (name.clone(), i))
            .collect();

        // 3. Data rows
        let mut rows = Vec::new();
        for line in lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let cells = parse_row(trimmed, columns.len());
            rows.push(cells);
        }

        Ok(Self {
            columns,
            col_index,
            rows,
        })
    }

    /// Number of data rows.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Number of columns.
    #[allow(dead_code)]
    pub fn col_count(&self) -> usize {
        self.columns.len()
    }

    /// Column names (for iterating all tags in a row).
    pub fn column_names(&self) -> &[String] {
        &self.columns
    }

    /// Get raw cell string at (row, tag).  Returns `None` if tag or row missing.
    fn cell(&self, row: usize, tag: &str) -> Option<&str> {
        let &col = self.col_index.get(tag)?;
        self.rows.get(row)?.get(col).map(|s| s.as_str())
    }

    // --- Typed accessors ---

    /// Check if cell is a Marker (`"M"`).
    pub fn marker(&self, row: usize, tag: &str) -> bool {
        self.cell(row, tag) == Some("M")
    }

    /// Check if cell is null (empty, `"N"`, or tag not present).
    pub fn is_null(&self, row: usize, tag: &str) -> bool {
        match self.cell(row, tag) {
            None => true,
            Some(s) => s.is_empty() || s == "N",
        }
    }

    /// Parse cell as f64 number.  Returns `default` if null/missing/non-numeric.
    pub fn number(&self, row: usize, tag: &str, default: f64) -> f64 {
        match self.cell(row, tag) {
            None => default,
            Some(s) if s.is_empty() || s == "N" || s == "M" => default,
            Some(s) => s.parse::<f64>().unwrap_or(default),
        }
    }

    /// Parse cell as i64 integer.  Returns `default` if null/missing/non-numeric.
    pub fn integer(&self, row: usize, tag: &str, default: i64) -> i64 {
        match self.cell(row, tag) {
            None => default,
            Some(s) if s.is_empty() || s == "N" || s == "M" => default,
            Some(s) => s.parse::<i64>().unwrap_or(default),
        }
    }

    /// Parse cell as string (strips surrounding quotes).
    /// Returns `default` if null/missing.
    pub fn string(&self, row: usize, tag: &str, default: &str) -> String {
        match self.cell(row, tag) {
            None => default.to_string(),
            Some(s) if s.is_empty() || s == "N" => default.to_string(),
            Some(s) => strip_quotes(s),
        }
    }
}

/// Strip outer double-quotes from a string and un-escape `\"` → `"`.
fn strip_quotes(s: &str) -> String {
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        let inner = &s[1..s.len() - 1];
        inner.replace("\\\"", "\"").replace("\\n", "\n")
    } else {
        s.to_string()
    }
}

/// Parse a single data row into cells, respecting quoted strings,
/// brackets, and ref values.
fn parse_row(line: &str, expected_cols: usize) -> Vec<String> {
    let mut cells = Vec::with_capacity(expected_cols);
    let mut current = String::new();
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        let ch = chars[i];

        match ch {
            // Quoted string — consume until matching unescaped quote
            '"' => {
                current.push('"');
                i += 1;
                while i < len {
                    if chars[i] == '\\' && i + 1 < len && chars[i + 1] == '"' {
                        // Escaped quote
                        current.push('\\');
                        current.push('"');
                        i += 2;
                    } else if chars[i] == '"' {
                        current.push('"');
                        i += 1;
                        // After closing quote there may be more content before comma
                        // (e.g., ref with display name: @ref "name")
                        // Continue accumulating until comma
                        break;
                    } else {
                        current.push(chars[i]);
                        i += 1;
                    }
                }
            }

            // Array — skip to matching bracket
            '[' => {
                let mut depth = 1;
                current.push('[');
                i += 1;
                while i < len && depth > 0 {
                    if chars[i] == '[' {
                        depth += 1;
                    } else if chars[i] == ']' {
                        depth -= 1;
                    }
                    current.push(chars[i]);
                    i += 1;
                }
            }

            // Object — skip to matching brace
            '{' => {
                let mut depth = 1;
                current.push('{');
                i += 1;
                while i < len && depth > 0 {
                    if chars[i] == '{' {
                        depth += 1;
                    } else if chars[i] == '}' {
                        depth -= 1;
                    }
                    current.push(chars[i]);
                    i += 1;
                }
            }

            // Ref value (@...) — consume until comma, handling quoted display name
            '@' => {
                current.push('@');
                i += 1;
                while i < len && chars[i] != ',' {
                    if chars[i] == '"' {
                        // Quoted display name within ref
                        current.push('"');
                        i += 1;
                        while i < len && chars[i] != '"' {
                            current.push(chars[i]);
                            i += 1;
                        }
                        if i < len {
                            current.push('"');
                            i += 1;
                        }
                    } else {
                        current.push(chars[i]);
                        i += 1;
                    }
                }
            }

            // Cell separator
            ',' => {
                cells.push(current.trim().to_string());
                current = String::new();
                i += 1;
            }

            // Regular character
            _ => {
                current.push(ch);
                i += 1;
            }
        }
    }

    // Push last cell
    cells.push(current.trim().to_string());

    // Pad with empty strings if fewer cells than columns
    while cells.len() < expected_cols {
        cells.push(String::new());
    }

    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_grid() {
        let input = "ver:\"3.0\"\nchannel,enabled,unit,analog\n612,M,\"cfm\",M\n1113,M,\"°F\",M\n";
        let grid = ZincGrid::parse(input).unwrap();
        assert_eq!(grid.row_count(), 2);
        assert_eq!(grid.col_count(), 4);
        assert_eq!(grid.integer(0, "channel", -1), 612);
        assert!(grid.marker(0, "enabled"));
        assert_eq!(grid.string(0, "unit", ""), "cfm");
        assert!(grid.marker(0, "analog"));
        assert_eq!(grid.integer(1, "channel", -1), 1113);
        assert_eq!(grid.string(1, "unit", ""), "°F");
    }

    #[test]
    fn test_markers_and_nulls() {
        let input = "ver:\"3.0\"\na,b,c\nM,N,\n";
        let grid = ZincGrid::parse(input).unwrap();
        assert!(grid.marker(0, "a"));
        assert!(!grid.marker(0, "b"));
        assert!(grid.is_null(0, "b"));
        assert!(grid.is_null(0, "c"));
        assert!(grid.is_null(0, "nonexistent"));
    }

    #[test]
    fn test_quoted_string_with_commas() {
        let input = "ver:\"3.0\"\nactions,channel\n\"ver:\\\"2.0\\\"\\nexpr,dis\",612\n";
        let grid = ZincGrid::parse(input).unwrap();
        assert_eq!(grid.row_count(), 1);
        assert_eq!(grid.integer(0, "channel", -1), 612);
        // The actions cell is a quoted string containing commas — should not split
        assert!(!grid.is_null(0, "actions"));
    }

    #[test]
    fn test_ref_value() {
        let input =
            "ver:\"3.0\"\nid,channel\n@p:abc-123 \"My Point\",612\n";
        let grid = ZincGrid::parse(input).unwrap();
        assert_eq!(grid.integer(0, "channel", -1), 612);
        assert!(!grid.is_null(0, "id"));
    }

    #[test]
    fn test_number_types() {
        let input = "ver:\"3.0\"\na,b,c,d\n3200,-40,0.5,\n";
        let grid = ZincGrid::parse(input).unwrap();
        assert_eq!(grid.number(0, "a", 0.0), 3200.0);
        assert_eq!(grid.number(0, "b", 0.0), -40.0);
        assert_eq!(grid.number(0, "c", 0.0), 0.5);
        assert_eq!(grid.number(0, "d", 99.0), 99.0); // empty → default
    }

    #[test]
    fn test_min_max_marker_pattern() {
        let input = "ver:\"3.0\"\nmin,minVal,max,maxVal\nM,-40,M,303\n";
        let grid = ZincGrid::parse(input).unwrap();
        assert!(grid.marker(0, "min"));
        assert_eq!(grid.number(0, "minVal", 0.0), -40.0);
        assert!(grid.marker(0, "max"));
        assert_eq!(grid.number(0, "maxVal", 0.0), 303.0);
    }

    #[test]
    fn test_empty_channel_skipped() {
        let input = "ver:\"3.0\"\nchannel,enabled,device\n,M,M\n612,M,\n";
        let grid = ZincGrid::parse(input).unwrap();
        assert_eq!(grid.row_count(), 2);
        assert_eq!(grid.integer(0, "channel", -1), -1); // empty → default
        assert_eq!(grid.integer(1, "channel", -1), 612);
    }

    #[test]
    fn test_invalid_header() {
        let result = ZincGrid::parse("not a zinc file\ncol1,col2\n");
        assert!(result.is_err());
    }

    #[test]
    fn test_row_padding() {
        // Row has fewer cells than columns → pad with empty
        let input = "ver:\"3.0\"\na,b,c,d,e\n1,2\n";
        let grid = ZincGrid::parse(input).unwrap();
        assert_eq!(grid.number(0, "a", 0.0), 1.0);
        assert_eq!(grid.number(0, "b", 0.0), 2.0);
        assert!(grid.is_null(0, "c"));
        assert!(grid.is_null(0, "d"));
        assert!(grid.is_null(0, "e"));
    }
}
