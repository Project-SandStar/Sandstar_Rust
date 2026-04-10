//! Automatic sensor detection based on channel ID.
//!
//! Channel numbering format: XXYY where XX = physical input (11-71), YY = sensor variant.
//! The sensor variant maps to a lookup table tag.

use crate::table::TableStore;
use crate::value::ValueConv;
use crate::ChannelId;

/// Map sensor variant (last 2 digits of channel ID) to table tag.
fn variant_to_tag(variant: u32) -> Option<&'static str> {
    match variant {
        0 => Some("range0to10V"),
        1 => Some("range0to5V"),
        2 => Some("range0to20mA"),
        3 => Some("range4to20mA"),
        10 => Some("thermistor2200"),
        11 => Some("thermistor3000"),
        12 => Some("thermistor10K1"),
        13 => Some("thermistor10K2"),
        14 => Some("thermistor10K3"),
        15 => Some("thermistor20K"),
        16 => Some("thermistor100K"),
        20 => Some("pt100"),
        21 => Some("pt500"),
        22 => Some("pt1000"),
        23 => Some("ni1000"),
        _ => None,
    }
}

/// Auto-detect sensor type from channel ID and configure conversion.
///
/// For analog input channels (1100-1723):
/// 1. Parse channel: physical_input = channel / 100, sensor_variant = channel % 100
/// 2. Validate physical_input is in [11, 71]
/// 3. Map sensor_variant to table tag
/// 4. Find table by tag, set table_index + low/high/min/max from table data
///
/// Returns true if detection succeeded.
pub fn auto_detect_sensor(channel: ChannelId, conv: &mut ValueConv, tables: &TableStore) -> bool {
    // Skip if table already configured (e.g., from database.zinc loader
    // with explicit low/high/min/max for custom sensor calibration).
    if conv.table_index.is_some() && conv.low.is_some() && conv.high.is_some() {
        return false;
    }

    // Only auto-detect for analog input channels (1100-1723)
    if !(1100..=1723).contains(&channel) {
        return false;
    }

    let physical_input = channel / 100;
    let sensor_variant = channel % 100;

    // Validate physical input range
    if !(11..=71).contains(&physical_input) {
        return false;
    }

    // Need a unit string to be set
    if conv.unit.is_empty() {
        return false;
    }

    // Map variant to table tag
    let tag = match variant_to_tag(sensor_variant) {
        Some(t) => t,
        None => return false,
    };

    // Find table by tag
    let table_idx = match tables.find_by_tag(tag) {
        Some(idx) => idx,
        None => return false,
    };

    let table = match tables.get(table_idx) {
        Some(t) => t,
        None => return false,
    };

    // Set table index
    conv.table_index = Some(table_idx);

    // Set low/high from table boundary values
    if let (Some(low), Some(high)) = (table.low_value(), table.high_value()) {
        conv.low = Some(low);
        conv.high = Some(high);
    }

    // Set min/max from unit-appropriate range columns (if not already set)
    let unit = conv.unit.to_lowercase();
    if conv.min.is_none() {
        let (min, max) = match unit.as_str() {
            "f" | "°f" | "fahrenheit" => table.ranges.fahrenheit,
            "c" | "°c" | "celsius" => table.ranges.celsius,
            "k" | "kelvin" => table.ranges.kelvin,
            _ => table.ranges.fahrenheit, // Default to Fahrenheit
        };
        conv.min = Some(min);
        conv.max = Some(max);
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{TableRanges, TableStore};
    use crate::value::ValueConv;

    fn make_test_tables() -> TableStore {
        let mut store = TableStore::new();

        // Thermistor 10K Type 2 (decreasing — NTC characteristic)
        store
            .add_with_values(
                "thermistor10K2",
                "temp",
                vec![32000.0, 16000.0, 8000.0, 4000.0, 2000.0, 1000.0, 500.0],
                TableRanges {
                    fahrenheit: (-40.0, 200.0),
                    celsius: (-40.0, 93.3),
                    kelvin: (233.0, 366.5),
                },
            )
            .unwrap();

        // Range 0-10V
        store
            .add_with_values(
                "range0to10V",
                "voltage",
                vec![0.0, 1024.0, 2048.0, 3072.0, 4096.0],
                TableRanges {
                    fahrenheit: (0.0, 10.0),
                    celsius: (0.0, 10.0),
                    kelvin: (0.0, 10.0),
                },
            )
            .unwrap();

        store
    }

    #[test]
    fn test_auto_detect_thermistor() {
        let tables = make_test_tables();
        let mut conv = ValueConv {
            unit: "F".to_string(),
            ..Default::default()
        };

        // Channel 1113 = physical input 11, variant 13 (thermistor10K2)
        let result = auto_detect_sensor(1113, &mut conv, &tables);
        assert!(result);
        assert!(conv.table_index.is_some());
        assert_eq!(conv.min, Some(-40.0));
        assert_eq!(conv.max, Some(200.0));
    }

    #[test]
    fn test_auto_detect_voltage() {
        let tables = make_test_tables();
        let mut conv = ValueConv {
            unit: "V".to_string(),
            ..Default::default()
        };

        // Channel 1100 = physical input 11, variant 0 (range0to10V)
        let result = auto_detect_sensor(1100, &mut conv, &tables);
        assert!(result);
        assert!(conv.table_index.is_some());
    }

    #[test]
    fn test_auto_detect_out_of_range() {
        let tables = make_test_tables();
        let mut conv = ValueConv {
            unit: "F".to_string(),
            ..Default::default()
        };

        // Channel 900 — outside valid range
        assert!(!auto_detect_sensor(900, &mut conv, &tables));

        // Channel 2000 — above valid range
        assert!(!auto_detect_sensor(2000, &mut conv, &tables));
    }

    #[test]
    fn test_auto_detect_no_unit() {
        let tables = make_test_tables();
        let mut conv = ValueConv::default();

        // No unit string -> fails
        assert!(!auto_detect_sensor(1113, &mut conv, &tables));
    }

    #[test]
    fn test_auto_detect_unknown_variant() {
        let tables = make_test_tables();
        let mut conv = ValueConv {
            unit: "F".to_string(),
            ..Default::default()
        };

        // Variant 99 — not mapped
        assert!(!auto_detect_sensor(1199, &mut conv, &tables));
    }

    #[test]
    fn test_auto_detect_celsius_ranges() {
        let tables = make_test_tables();
        let mut conv = ValueConv {
            unit: "C".to_string(),
            ..Default::default()
        };

        auto_detect_sensor(1113, &mut conv, &tables);
        assert_eq!(conv.min, Some(-40.0));
        assert_eq!(conv.max, Some(93.3));
    }

    #[test]
    fn test_auto_detect_preserves_existing_min_max() {
        let tables = make_test_tables();
        let mut conv = ValueConv {
            unit: "F".to_string(),
            min: Some(0.0),
            max: Some(150.0),
            ..Default::default()
        };

        auto_detect_sensor(1113, &mut conv, &tables);

        // Existing min/max preserved
        assert_eq!(conv.min, Some(0.0));
        assert_eq!(conv.max, Some(150.0));
    }

    #[test]
    fn test_variant_to_tag_all() {
        assert_eq!(variant_to_tag(0), Some("range0to10V"));
        assert_eq!(variant_to_tag(1), Some("range0to5V"));
        assert_eq!(variant_to_tag(2), Some("range0to20mA"));
        assert_eq!(variant_to_tag(3), Some("range4to20mA"));
        assert_eq!(variant_to_tag(10), Some("thermistor2200"));
        assert_eq!(variant_to_tag(11), Some("thermistor3000"));
        assert_eq!(variant_to_tag(12), Some("thermistor10K1"));
        assert_eq!(variant_to_tag(13), Some("thermistor10K2"));
        assert_eq!(variant_to_tag(14), Some("thermistor10K3"));
        assert_eq!(variant_to_tag(15), Some("thermistor20K"));
        assert_eq!(variant_to_tag(16), Some("thermistor100K"));
        assert_eq!(variant_to_tag(20), Some("pt100"));
        assert_eq!(variant_to_tag(21), Some("pt500"));
        assert_eq!(variant_to_tag(22), Some("pt1000"));
        assert_eq!(variant_to_tag(23), Some("ni1000"));
        assert_eq!(variant_to_tag(99), None);
    }
}
