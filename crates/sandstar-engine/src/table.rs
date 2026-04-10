use crate::error::{EngineError, Result};
use std::fs;

/// Direction of values in a lookup table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableDirection {
    /// Values increase with index (first < last).
    Increasing,
    /// Values decrease with index (first > last).
    Decreasing,
}

/// Temperature/unit ranges for a table.
#[derive(Debug, Clone, Copy)]
pub struct TableRanges {
    pub fahrenheit: (f64, f64),
    pub celsius: (f64, f64),
    pub kelvin: (f64, f64),
}

impl Default for TableRanges {
    fn default() -> Self {
        Self {
            fahrenheit: (0.0, 0.0),
            celsius: (0.0, 0.0),
            kelvin: (0.0, 0.0),
        }
    }
}

/// A single lookup table (maps C `TABLE_ITEM`).
#[derive(Debug, Clone)]
pub struct TableItem {
    pub tag: String,
    pub unit_type: String,
    pub path: String,
    pub values: Vec<f64>,
    pub direction: TableDirection,
    pub ranges: TableRanges,
}

impl TableItem {
    /// Get the first value in the table.
    pub fn first_value(&self) -> Option<f64> {
        self.values.first().copied()
    }

    /// Get the last value in the table.
    pub fn last_value(&self) -> Option<f64> {
        self.values.last().copied()
    }

    /// Get the low boundary value (smallest raw value in the table).
    pub fn low_value(&self) -> Option<f64> {
        match self.direction {
            TableDirection::Increasing => self.first_value(),
            TableDirection::Decreasing => self.last_value(),
        }
    }

    /// Get the high boundary value (largest raw value in the table).
    pub fn high_value(&self) -> Option<f64> {
        match self.direction {
            TableDirection::Increasing => self.last_value(),
            TableDirection::Decreasing => self.first_value(),
        }
    }
}

/// Collection of lookup tables (maps C `TABLE` struct).
pub struct TableStore {
    tables: Vec<Option<TableItem>>,
}

impl TableStore {
    pub fn new() -> Self {
        Self { tables: Vec::new() }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            tables: Vec::with_capacity(capacity),
        }
    }

    /// Load tables from a CSV configuration file.
    ///
    /// CSV format: tag,unit_type,path,range_F_min,range_F_max,range_C_min,range_C_max,range_K_min,range_K_max
    pub fn load_from_csv(&mut self, csv_path: &str) -> Result<()> {
        let content = fs::read_to_string(csv_path)?;

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let fields: Vec<&str> = line.split(',').collect();
            if fields.len() < 3 {
                continue;
            }

            let tag = fields[0].trim().to_string();
            let unit_type = fields[1].trim().to_string();
            let path = fields[2].trim().to_string();

            let parse_f64 = |i: usize| -> f64 {
                fields
                    .get(i)
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    .unwrap_or(0.0)
            };

            let ranges = TableRanges {
                fahrenheit: (parse_f64(3), parse_f64(4)),
                celsius: (parse_f64(5), parse_f64(6)),
                kelvin: (parse_f64(7), parse_f64(8)),
            };

            self.add(&tag, &unit_type, &path, ranges)?;
        }

        Ok(())
    }

    /// Add a table by loading its values from a data file.
    ///
    /// Data file format: one floating-point number per line, no headers.
    pub fn add(
        &mut self,
        tag: &str,
        unit_type: &str,
        path: &str,
        ranges: TableRanges,
    ) -> Result<()> {
        // Check for duplicate tag + unit_type
        if self.find_by_tag_and_unit(tag, unit_type).is_some() {
            return Ok(()); // Already loaded, skip silently (matches C behavior)
        }

        let values = Self::load_values(path)?;
        if values.is_empty() {
            return Err(EngineError::InvalidTableFile(format!(
                "no values in {}",
                path
            )));
        }

        let direction = if values.last().unwrap() > values.first().unwrap() {
            TableDirection::Increasing
        } else {
            TableDirection::Decreasing
        };

        let item = TableItem {
            tag: tag.to_string(),
            unit_type: unit_type.to_string(),
            path: path.to_string(),
            values,
            direction,
            ranges,
        };

        // Find an empty slot or push new
        if let Some(slot) = self.tables.iter_mut().find(|s| s.is_none()) {
            *slot = Some(item);
        } else {
            self.tables.push(Some(item));
        }

        Ok(())
    }

    /// Add a table directly from pre-loaded values (for testing).
    pub fn add_with_values(
        &mut self,
        tag: &str,
        unit_type: &str,
        values: Vec<f64>,
        ranges: TableRanges,
    ) -> Result<()> {
        if values.is_empty() {
            return Err(EngineError::InvalidTableFile("empty values".to_string()));
        }

        let direction = if values.last().unwrap() > values.first().unwrap() {
            TableDirection::Increasing
        } else {
            TableDirection::Decreasing
        };

        let item = TableItem {
            tag: tag.to_string(),
            unit_type: unit_type.to_string(),
            path: String::new(),
            values,
            direction,
            ranges,
        };

        if let Some(slot) = self.tables.iter_mut().find(|s| s.is_none()) {
            *slot = Some(item);
        } else {
            self.tables.push(Some(item));
        }

        Ok(())
    }

    /// Remove a table by index.
    pub fn remove(&mut self, index: usize) {
        if let Some(slot) = self.tables.get_mut(index) {
            *slot = None;
        }
    }

    /// Find a table index by tag (first match).
    pub fn find_by_tag(&self, tag: &str) -> Option<usize> {
        self.tables
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|t| t.tag == tag))
    }

    /// Find a table index by tag and unit_type.
    pub fn find_by_tag_and_unit(&self, tag: &str, unit_type: &str) -> Option<usize> {
        self.tables.iter().position(|slot| {
            slot.as_ref()
                .is_some_and(|t| t.tag == tag && t.unit_type == unit_type)
        })
    }

    /// Get a table by index.
    pub fn get(&self, index: usize) -> Option<&TableItem> {
        self.tables.get(index).and_then(|s| s.as_ref())
    }

    /// Number of loaded tables.
    pub fn count(&self) -> usize {
        self.tables.iter().filter(|s| s.is_some()).count()
    }

    /// Remove all tables. Used during config reload.
    pub fn clear(&mut self) {
        for slot in self.tables.iter_mut() {
            *slot = None;
        }
    }

    /// Perform binary search interpolation lookup.
    ///
    /// Given a raw value, find the corresponding converted value using
    /// binary search in the table followed by linear interpolation.
    ///
    /// `min` and `max` define the output range (engineering units).
    /// The table maps raw values to indices, which are linearly mapped to [min, max].
    pub fn lookup(&self, index: usize, raw: f64, min: f64, max: f64) -> Option<f64> {
        let item = self.get(index)?;
        let n_values = item.values.len();
        if n_values < 2 {
            return None;
        }

        let unit_step = (max - min) / (n_values as f64 - 1.0);

        match item.direction {
            TableDirection::Increasing => {
                // Check boundaries
                if raw <= item.values[0] {
                    return Some(min);
                }
                if raw >= item.values[n_values - 1] {
                    return Some(max);
                }

                // Binary search for the interval containing raw
                let pos = self.binary_search_increasing(&item.values, raw);
                let r1 = item.values[pos];
                let r2 = item.values[pos + 1];
                let c1 = min + pos as f64 * unit_step;
                let c2 = c1 + unit_step;
                let frac = (raw - r1) / (r2 - r1);
                Some(lerp(c1, c2, frac))
            }
            TableDirection::Decreasing => {
                // Check boundaries (reversed: first value is largest)
                if raw >= item.values[0] {
                    return Some(min);
                }
                if raw <= item.values[n_values - 1] {
                    return Some(max);
                }

                // Binary search for the interval containing raw (decreasing)
                let pos = self.binary_search_decreasing(&item.values, raw);
                let r1 = item.values[pos];
                let r2 = item.values[pos + 1];
                let c1 = min + pos as f64 * unit_step;
                let c2 = c1 + unit_step;
                let frac = (raw - r2) / (r1 - r2);
                Some(lerp(c2, c1, frac))
            }
        }
    }

    /// Reverse lookup: given a converted value, find the raw value.
    ///
    /// Uses linear scan (matches C behavior which does O(N) for reverse).
    pub fn reverse_lookup(&self, index: usize, cur: f64, min: f64, max: f64) -> Option<f64> {
        let item = self.get(index)?;
        let n_values = item.values.len();
        if n_values < 2 {
            return None;
        }

        let unit_step = (max - min) / (n_values as f64 - 1.0);

        match item.direction {
            TableDirection::Increasing => {
                // Boundary checks
                if cur <= min {
                    return Some(item.values[0]);
                }
                if cur >= max {
                    return Some(item.values[n_values - 1]);
                }

                // Linear scan to find the interval
                for n in 0..n_values - 1 {
                    let c1 = min + n as f64 * unit_step;
                    let c2 = c1 + unit_step;
                    if cur >= c1 && cur <= c2 {
                        let frac = (cur - c1) / (c2 - c1);
                        let raw = lerp(item.values[n], item.values[n + 1], frac);
                        return Some(raw as i64 as f64); // Truncate (matches C cast to int)
                    }
                }
                None
            }
            TableDirection::Decreasing => {
                if cur <= min {
                    return Some(item.values[0]);
                }
                if cur >= max {
                    return Some(item.values[n_values - 1]);
                }

                for n in 0..n_values - 1 {
                    let c1 = min + n as f64 * unit_step;
                    let c2 = c1 + unit_step;
                    if cur >= c1 && cur <= c2 {
                        let frac = (cur - c1) / (c2 - c1);
                        let raw = lerp(item.values[n], item.values[n + 1], frac);
                        return Some(raw as i64 as f64);
                    }
                }
                None
            }
        }
    }

    /// Binary search in an increasing sequence.
    /// Returns the index `n` such that `values[n] <= raw <= values[n+1]`.
    fn binary_search_increasing(&self, values: &[f64], raw: f64) -> usize {
        let mut lo = 0usize;
        let mut hi = values.len() - 1;

        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if values[mid] <= raw {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Binary search in a decreasing sequence.
    /// Returns the index `n` such that `values[n] >= raw >= values[n+1]`.
    fn binary_search_decreasing(&self, values: &[f64], raw: f64) -> usize {
        let mut lo = 0usize;
        let mut hi = values.len() - 1;

        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if values[mid] >= raw {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Load values from a data file (one float per line).
    fn load_values(path: &str) -> Result<Vec<f64>> {
        let content = fs::read_to_string(path)
            .map_err(|e| EngineError::InvalidTableFile(format!("cannot read {}: {}", path, e)))?;

        let values: Vec<f64> = content
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    trimmed.parse::<f64>().ok()
                }
            })
            .collect();

        if values.is_empty() {
            return Err(EngineError::InvalidTableFile(format!(
                "no numeric values found in {}",
                path
            )));
        }

        Ok(values)
    }
}

impl Default for TableStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Linear interpolation: `a * (1 - f) + b * f`.
pub fn lerp(a: f64, b: f64, f: f64) -> f64 {
    a * (1.0 - f) + b * f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_increasing_table() -> TableStore {
        let mut store = TableStore::new();
        // Simple increasing table: raw ADC values mapping to temperature
        // 5 values mapping [0, 100] raw to [-40, 120] degrees
        store
            .add_with_values(
                "test_inc",
                "temp",
                vec![100.0, 500.0, 1000.0, 2000.0, 4000.0],
                TableRanges {
                    fahrenheit: (-40.0, 120.0),
                    celsius: (-40.0, 49.0),
                    kelvin: (233.0, 322.0),
                },
            )
            .unwrap();
        store
    }

    fn make_decreasing_table() -> TableStore {
        let mut store = TableStore::new();
        // Decreasing table: NTC thermistor (resistance drops with temperature)
        store
            .add_with_values(
                "test_dec",
                "temp",
                vec![4000.0, 2000.0, 1000.0, 500.0, 100.0],
                TableRanges {
                    fahrenheit: (-40.0, 120.0),
                    celsius: (-40.0, 49.0),
                    kelvin: (233.0, 322.0),
                },
            )
            .unwrap();
        store
    }

    #[test]
    fn test_lerp() {
        assert_eq!(lerp(0.0, 10.0, 0.0), 0.0);
        assert_eq!(lerp(0.0, 10.0, 1.0), 10.0);
        assert_eq!(lerp(0.0, 10.0, 0.5), 5.0);
        assert_eq!(lerp(10.0, 20.0, 0.25), 12.5);
    }

    #[test]
    fn test_increasing_lookup_boundaries() {
        let store = make_increasing_table();
        let idx = store.find_by_tag("test_inc").unwrap();

        // At or below first value -> min
        assert_eq!(store.lookup(idx, 50.0, -40.0, 120.0), Some(-40.0));
        assert_eq!(store.lookup(idx, 100.0, -40.0, 120.0), Some(-40.0));

        // At or above last value -> max
        assert_eq!(store.lookup(idx, 4000.0, -40.0, 120.0), Some(120.0));
        assert_eq!(store.lookup(idx, 5000.0, -40.0, 120.0), Some(120.0));
    }

    #[test]
    fn test_increasing_lookup_interpolation() {
        let store = make_increasing_table();
        let idx = store.find_by_tag("test_inc").unwrap();

        // Midpoint of first interval [100, 500] -> [-40, 0]
        let result = store.lookup(idx, 300.0, -40.0, 120.0).unwrap();
        assert!((result - (-20.0)).abs() < 0.01);
    }

    #[test]
    fn test_decreasing_lookup_boundaries() {
        let store = make_decreasing_table();
        let idx = store.find_by_tag("test_dec").unwrap();

        // At or above first value (4000) -> min
        assert_eq!(store.lookup(idx, 5000.0, -40.0, 120.0), Some(-40.0));
        assert_eq!(store.lookup(idx, 4000.0, -40.0, 120.0), Some(-40.0));

        // At or below last value (100) -> max
        assert_eq!(store.lookup(idx, 100.0, -40.0, 120.0), Some(120.0));
        assert_eq!(store.lookup(idx, 50.0, -40.0, 120.0), Some(120.0));
    }

    #[test]
    fn test_decreasing_lookup_interpolation() {
        let store = make_decreasing_table();
        let idx = store.find_by_tag("test_dec").unwrap();

        // Midpoint of first interval [4000, 2000] (decreasing) -> [-40, 0]
        let result = store.lookup(idx, 3000.0, -40.0, 120.0).unwrap();
        assert!((result - (-20.0)).abs() < 0.01);
    }

    #[test]
    fn test_find_by_tag() {
        let store = make_increasing_table();
        assert!(store.find_by_tag("test_inc").is_some());
        assert!(store.find_by_tag("nonexistent").is_none());
    }

    #[test]
    fn test_duplicate_tag_skipped() {
        let mut store = TableStore::new();
        store
            .add_with_values("dup", "temp", vec![1.0, 2.0, 3.0], TableRanges::default())
            .unwrap();
        assert_eq!(store.count(), 1);

        // Adding same tag+unit again is silently skipped by add()
        // (add_with_values doesn't check, but add() does)
    }

    #[test]
    fn test_empty_values_rejected() {
        let mut store = TableStore::new();
        let result = store.add_with_values("empty", "temp", vec![], TableRanges::default());
        assert!(result.is_err());
    }

    #[test]
    fn test_reverse_lookup() {
        let store = make_increasing_table();
        let idx = store.find_by_tag("test_inc").unwrap();

        // Boundary: cur = min -> first raw value
        assert_eq!(store.reverse_lookup(idx, -40.0, -40.0, 120.0), Some(100.0));

        // Boundary: cur = max -> last raw value
        assert_eq!(store.reverse_lookup(idx, 120.0, -40.0, 120.0), Some(4000.0));
    }

    #[test]
    fn test_table_direction() {
        let store = make_increasing_table();
        let item = store.get(0).unwrap();
        assert_eq!(item.direction, TableDirection::Increasing);

        let store = make_decreasing_table();
        let item = store.get(0).unwrap();
        assert_eq!(item.direction, TableDirection::Decreasing);
    }

    #[test]
    fn test_table_clear() {
        let mut store = TableStore::new();
        store
            .add_with_values("t1", "temp", vec![1.0, 2.0, 3.0], TableRanges::default())
            .unwrap();
        store
            .add_with_values("t2", "temp", vec![4.0, 5.0, 6.0], TableRanges::default())
            .unwrap();
        assert_eq!(store.count(), 2);

        store.clear();
        assert_eq!(store.count(), 0);
        assert!(store.find_by_tag("t1").is_none());
        assert!(store.find_by_tag("t2").is_none());
    }
}
