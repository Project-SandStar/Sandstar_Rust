//! Automatic sensor detection based on channel ID.
//!
//! Channel numbering format: XXYY where XX = physical input (11-71), YY = sensor variant.
//! The sensor variant maps to a lookup table tag.

use crate::table::{TableStore, MISSING_TABLE};
use crate::value::ValueConv;
use crate::ChannelId;
use tracing::warn;

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
    detect(channel, conv, tables, true)
}

/// What [`auto_detect_sensor`] would do to `conv`, without the warning.
///
/// For a caller that works on a copy to see what the engine will make of a
/// configuration (the reload compares channels this way); the warning is
/// for the channel that is really read.
pub fn auto_detect_preview(channel: ChannelId, conv: &mut ValueConv, tables: &TableStore) -> bool {
    detect(channel, conv, tables, false)
}

/// The table tag the channel number stands for, if auto-detection covers
/// this channel at all: an analog input 1100-1723 with a unit and a sensor
/// variant that has a table.
pub fn auto_detect_tag(channel: ChannelId, conv: &ValueConv) -> Option<&'static str> {
    // Only auto-detect for analog input channels (1100-1723)
    if !(1100..=1723).contains(&channel) {
        return None;
    }
    // Validate physical input range
    if !(11..=71).contains(&(channel / 100)) {
        return None;
    }
    // Need a unit string to be set
    if conv.unit.is_empty() {
        return None;
    }
    // Map variant to table tag
    variant_to_tag(channel % 100)
}

fn detect(channel: ChannelId, conv: &mut ValueConv, tables: &TableStore, log: bool) -> bool {
    // Skip if table already configured (e.g., from database.zinc loader
    // with explicit low/high/min/max for custom sensor calibration).
    if conv.table_index.is_some() && conv.low.is_some() && conv.high.is_some() {
        return false;
    }

    // The configuration named a table that is not loaded. Picking one by
    // channel number would convert with a table nobody asked for (R-63).
    if conv.table_index == Some(MISSING_TABLE) {
        return false;
    }

    let Some(tag) = auto_detect_tag(channel, conv) else {
        return false;
    };

    // Find table by tag
    let table_idx = match tables.find_by_tag(tag) {
        Some(idx) => idx,
        None => {
            // Tables are loaded, the one this channel number stands for is
            // not among them, and the configuration names no other table:
            // mark the channel. It reads as fault instead of converting
            // without a table and reporting a good value (R-63). The mark
            // also stops this function, so the warning comes once.
            //
            // Not marked:
            // - no table at all (demo, simulator, a set-up without
            //   tables.csv): conversion without tables is what it runs on;
            // - a row that gives low and high itself: that is a range
            //   scaling somebody configured, not a sensor waiting for its
            //   table.
            let own_range = conv.low.is_some() && conv.high.is_some();
            if tables.count() > 0 && conv.table_index.is_none() && !own_range {
                conv.table_index = Some(MISSING_TABLE);
                if log {
                    warn!(
                        channel,
                        table = tag,
                        "conversion table for this channel number is not loaded: \
                     channel reads as fault and rejects writes"
                    );
                }
            }
            return false;
        }
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

    #[test]
    fn a_channel_marked_as_missing_its_table_is_not_given_another_one() {
        let mut tables = TableStore::new();
        tables
            .add_with_values(
                "thermistor10K2",
                "temp",
                vec![100.0, 200.0, 300.0],
                crate::table::TableRanges::default(),
            )
            .unwrap();
        let mut conv = ValueConv {
            unit: "°C".to_string(),
            table_index: Some(MISSING_TABLE),
            ..Default::default()
        };
        // 1113 would auto-detect thermistor10K2.
        assert!(!auto_detect_sensor(1113, &mut conv, &tables));
        assert_eq!(conv.table_index, Some(MISSING_TABLE));
        assert!(conv.low.is_none() && conv.high.is_none());
    }

    // ── S-60: the table this channel number stands for is not loaded ──

    fn conv_with_unit() -> ValueConv {
        ValueConv {
            unit: "F".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_missing_table_marks_the_channel_when_tables_are_loaded() {
        // make_test_tables has thermistor10K2 and range0to10V, not pt1000.
        let tables = make_test_tables();
        let mut conv = conv_with_unit();

        // 1122 stands for pt1000.
        assert!(!auto_detect_sensor(1122, &mut conv, &tables));
        assert_eq!(conv.table_index, Some(MISSING_TABLE));
        assert!(conv.low.is_none() && conv.high.is_none());
        assert!(conv.min.is_none() && conv.max.is_none());

        // The second call changes nothing.
        assert!(!auto_detect_sensor(1122, &mut conv, &tables));
        assert_eq!(conv.table_index, Some(MISSING_TABLE));
    }

    #[test]
    fn a_missing_table_marks_nothing_when_no_table_is_loaded() {
        let tables = TableStore::new();
        let mut conv = conv_with_unit();
        assert!(!auto_detect_sensor(1122, &mut conv, &tables));
        assert_eq!(conv.table_index, None);
    }

    #[test]
    fn a_missing_table_does_not_mark_channels_auto_detection_does_not_cover() {
        let tables = make_test_tables();
        // Outside 1100-1723, no unit, a variant without a table.
        for (channel, unit) in [(900, "F"), (2022, "F"), (1122, ""), (1199, "F")] {
            let mut conv = ValueConv {
                unit: unit.to_string(),
                ..Default::default()
            };
            assert!(!auto_detect_sensor(channel, &mut conv, &tables));
            assert_eq!(conv.table_index, None, "channel {channel} unit {unit:?}");
        }
    }

    /// A table the configuration named stays; only a channel without any
    /// table is marked.
    #[test]
    fn a_missing_table_does_not_replace_a_table_the_configuration_named() {
        let tables = make_test_tables();
        let named = tables.find_by_tag("range0to10V").unwrap();
        let mut conv = conv_with_unit();
        conv.table_index = Some(named);
        assert!(!auto_detect_sensor(1122, &mut conv, &tables));
        assert_eq!(conv.table_index, Some(named));
    }

    /// A row with its own low and high is a configured range scaling; it
    /// keeps working when the table of its channel number is not loaded.
    #[test]
    fn a_missing_table_does_not_mark_a_channel_with_its_own_range() {
        let tables = make_test_tables();
        let mut conv = conv_with_unit();
        conv.low = Some(0.0);
        conv.high = Some(4096.0);
        assert!(!auto_detect_sensor(1122, &mut conv, &tables));
        assert_eq!(conv.table_index, None);
        // Half a range is not a range.
        let mut conv = conv_with_unit();
        conv.low = Some(0.0);
        assert!(!auto_detect_sensor(1122, &mut conv, &tables));
        assert_eq!(conv.table_index, Some(MISSING_TABLE));
    }

    #[test]
    fn preview_marks_like_the_real_call() {
        let tables = make_test_tables();
        let (mut a, mut b) = (conv_with_unit(), conv_with_unit());
        assert_eq!(
            auto_detect_sensor(1122, &mut a, &tables),
            auto_detect_preview(1122, &mut b, &tables)
        );
        assert_eq!(a.table_index, b.table_index);
        let (mut a, mut b) = (conv_with_unit(), conv_with_unit());
        assert!(auto_detect_sensor(1113, &mut a, &tables));
        assert!(auto_detect_preview(1113, &mut b, &tables));
        assert_eq!(
            (a.table_index, a.low, a.high),
            (b.table_index, b.low, b.high)
        );
    }

    #[test]
    fn auto_detect_tag_covers_only_sensor_channels_with_a_unit() {
        let conv = conv_with_unit();
        assert_eq!(auto_detect_tag(1113, &conv), Some("thermistor10K2"));
        assert_eq!(auto_detect_tag(1122, &conv), Some("pt1000"));
        assert_eq!(auto_detect_tag(1199, &conv), None);
        assert_eq!(auto_detect_tag(900, &conv), None);
        assert_eq!(auto_detect_tag(1113, &ValueConv::default()), None);
    }
}
