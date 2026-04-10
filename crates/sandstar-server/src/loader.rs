//! Configuration loader: reads points.csv, tables.csv, table data files,
//! and database.zinc.
//!
//! Builds `ChannelStore` and `TableStore` from the real configuration files
//! used by the production Sandstar system.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use sandstar_engine::channel::{Channel, ChannelDirection, ChannelType};
use sandstar_engine::conversion::{RateLimitConfig, SmoothMethod, SmoothingConfig, SpikeConfig};
use sandstar_engine::error::EngineError;
use sandstar_engine::table::{TableRanges, TableStore};
use sandstar_engine::value::{ConversionFn, FilterConfig, FlowConfig, ValueConv};
use sandstar_engine::Engine;
use sandstar_hal::{HalDiagnostics, HalRead, HalWrite};
use tracing::{info, warn};

use crate::zinc::ZincGrid;

/// Load channels from points.csv into the engine.
///
/// CSV format: `channel,label,jumper,type,direction,device,address,trigger`
///
/// Returns the number of channels loaded.
pub fn load_points<H: HalRead + HalWrite + HalDiagnostics>(
    engine: &mut Engine<H>,
    path: &Path,
) -> Result<usize, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    let mut count = 0;

    for (line_num, line) in content.lines().enumerate() {
        if line_num == 0 || line.trim().is_empty() {
            continue; // skip header
        }

        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < 7 {
            warn!(line = line_num + 1, "skipping malformed line in points.csv");
            continue;
        }

        let id: u32 = match fields[0].trim().parse() {
            Ok(v) => v,
            Err(_) => {
                warn!(
                    line = line_num + 1,
                    field = fields[0],
                    "invalid channel number"
                );
                continue;
            }
        };

        let label = fields[1].trim();
        let channel_type = parse_channel_type(fields[3].trim());
        let direction = parse_direction(fields[4].trim());
        let device: u32 = fields[5].trim().parse().unwrap_or(0);
        let address: u32 = fields[6].trim().parse().unwrap_or(0);
        let trigger = fields.get(7).is_some_and(|s| !s.trim().is_empty());

        let mut conv = ValueConv::default();
        // Auto-assign conversion function for I2C channels
        if channel_type == ChannelType::I2c {
            conv.conv_func = ConversionFn::from_channel_id(id);
        }

        let ch = Channel::new(
            id,
            channel_type,
            direction,
            device,
            address,
            trigger,
            conv,
            label,
        );

        match engine.channels.add(ch) {
            Ok(()) => count += 1,
            Err(EngineError::DuplicateChannel(_)) => {
                // Points.csv has multiple sensor types per physical input;
                // only the first one is loaded (the rest are selected via database.zinc)
            }
            Err(e) => warn!(channel = id, err = %e, "failed to add channel"),
        }
    }

    info!(count, path = %path.display(), "loaded channels from points.csv");
    Ok(count)
}

/// Load lookup tables from tables.csv and their data files.
///
/// CSV format: `name,description,path,unit_type,range_F_min,range_F_max,...,tag`
///
/// The `path` column points to the table data file on the target system.
/// We search for the file locally using `table_dir` as the base directory.
pub fn load_tables(
    tables: &mut TableStore,
    csv_path: &Path,
    table_dir: &Path,
) -> Result<usize, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(csv_path)?;
    let mut count = 0;

    for (line_num, line) in content.lines().enumerate() {
        if line_num == 0 || line.trim().is_empty() {
            continue; // skip header
        }

        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < 11 {
            warn!(line = line_num + 1, "skipping malformed line in tables.csv");
            continue;
        }

        let name = fields[0].trim().to_string();
        let remote_path = fields[2].trim();
        let unit_type = fields[3].trim().to_string();
        let tag = fields[10].trim().to_string();

        // Parse ranges
        let f_min: f64 = fields[4].trim().parse().unwrap_or(0.0);
        let f_max: f64 = fields[5].trim().parse().unwrap_or(0.0);
        let c_min: f64 = fields[6].trim().parse().unwrap_or(0.0);
        let c_max: f64 = fields[7].trim().parse().unwrap_or(0.0);
        let k_min: f64 = fields[8].trim().parse().unwrap_or(0.0);
        let k_max: f64 = fields[9].trim().parse().unwrap_or(0.0);

        // Find the data file locally: extract filename from remote path
        let filename = Path::new(remote_path)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");

        let local_path = table_dir.join(filename);
        if !local_path.exists() {
            warn!(table = %name, path = %local_path.display(), "table data file not found");
            continue;
        }

        // Load table values (one f64 per line)
        let values = load_table_values(&local_path)?;
        if values.len() < 2 {
            warn!(table = %name, count = values.len(), "table has too few values");
            continue;
        }

        let ranges = TableRanges {
            fahrenheit: (f_min, f_max),
            celsius: (c_min, c_max),
            kelvin: (k_min, k_max),
        };

        match tables.add_with_values(&tag, &unit_type, values, ranges) {
            Ok(()) => {
                count += 1;
            }
            Err(e) => warn!(table = %name, err = %e, "failed to add table"),
        }
    }

    info!(count, path = %csv_path.display(), "loaded tables");
    Ok(count)
}

/// Load table data values from a text file (one f64 per line).
fn load_table_values(path: &Path) -> Result<Vec<f64>, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    let mut values = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match trimmed.parse::<f64>() {
            Ok(v) => values.push(v),
            Err(_) => {
                warn!(path = %path.display(), line = trimmed, "non-numeric value in table file");
            }
        }
    }

    Ok(values)
}

/// Set up poll list: add all input channels to the poll list.
pub fn setup_polls<H: HalRead + HalWrite + HalDiagnostics>(engine: &mut Engine<H>) -> usize {
    let input_ids: Vec<u32> = engine
        .channels
        .iter()
        .filter(|(_, ch)| !ch.direction.is_output() && ch.enabled)
        .map(|(&id, _)| id)
        .collect();

    let mut count = 0;
    for id in input_ids {
        if engine.polls.add(id).is_ok() {
            count += 1;
        }
    }

    info!(count, "set up poll list");
    count
}

/// Build a tag→table_index mapping for auto-detection.
///
/// Maps sensor type tags (e.g., "thermistor10K2") to the table index
/// in the TableStore, so channels can find their lookup table.
pub fn build_tag_table_map(tables: &TableStore) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    for i in 0..1000 {
        if let Some(item) = tables.get(i) {
            map.insert(item.tag.clone(), i);
        }
    }
    map
}

/// Load channel configuration from database.zinc into the engine.
///
/// For physical channels: calls `update_metadata()` on existing channels
/// that were previously loaded from points.csv.
/// For virtual channels: creates new `Channel` entries directly.
///
/// Returns the number of channels successfully configured.
pub fn load_database<H: HalRead + HalWrite + HalDiagnostics>(
    engine: &mut Engine<H>,
    path: &Path,
) -> Result<usize, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    let grid = ZincGrid::parse(&content)?;
    let mut count = 0;

    for row in 0..grid.row_count() {
        // 1. Read channel number — skip rows without a valid channel (device header rows)
        let channel_id = grid.integer(row, "channel", -1);
        if channel_id < 0 {
            continue;
        }
        let channel_id = channel_id as u32;

        // 2. Determine enabled state
        let enabled = grid.marker(row, "enabled") && !grid.marker(row, "disabled");

        // 3. Build ValueConv from zinc tags
        let conv = build_value_conv(&grid, row, engine);

        // 4. Get display label
        let label = grid.string(row, "navName", "");

        // 5. Collect all non-null tags from this row for SVM bridge
        let mut tags = std::collections::HashMap::new();
        for col_name in grid.column_names() {
            if !grid.is_null(row, col_name) {
                let val = if grid.marker(row, col_name) {
                    "M".to_string()
                } else {
                    grid.string(row, col_name, "")
                };
                if !val.is_empty() {
                    tags.insert(col_name.clone(), val);
                }
            }
        }

        // 6. Branch: virtual vs physical channel
        if grid.marker(row, "virtualChannel") {
            // --- Virtual channel ---
            let channel_type = if grid.marker(row, "analog") {
                ChannelType::VirtualAnalog
            } else {
                ChannelType::VirtualDigital
            };

            let ch = Channel::new(
                channel_id,
                channel_type,
                ChannelDirection::None,
                u32::MAX,
                u32::MAX,
                false,
                conv,
                &label,
            );

            match engine.channels.add(ch) {
                Ok(()) => {
                    // Virtual channels always get polled
                    let _ = engine.polls.add(channel_id);
                    if let Some(ch) = engine.channels.get_mut(channel_id) {
                        if !enabled {
                            ch.enabled = false;
                        }
                        ch.tags = tags.clone();
                    }
                    count += 1;
                }
                Err(e) => {
                    warn!(channel = channel_id, err = %e, "failed to add virtual channel");
                }
            }
        } else {
            // --- Physical channel (must already exist from points.csv) ---
            if !engine.channels.contains(channel_id) {
                warn!(
                    channel = channel_id,
                    row, "channel in database.zinc not found in points.csv — skipping"
                );
                continue;
            }

            match engine
                .channels
                .update_metadata(channel_id, enabled, conv, &label)
            {
                Ok(()) => {
                    if let Some(ch) = engine.channels.get_mut(channel_id) {
                        ch.tags = tags.clone();
                    }
                    count += 1;
                }
                Err(e) => {
                    warn!(channel = channel_id, err = %e, "failed to update channel metadata");
                    continue;
                }
            }

            // Add to poll list if cur or raw marker is present
            if grid.marker(row, "cur") || grid.marker(row, "raw") {
                let _ = engine.polls.add(channel_id);
            }
        }
    }

    info!(count, path = %path.display(), "loaded channel config from database.zinc");
    Ok(count)
}

/// Result of a granular database.zinc reload.
///
/// Tracks which channels were added, removed, modified, or unchanged
/// so the caller can apply targeted poll list updates instead of
/// clearing and rebuilding the entire poll list.
pub struct GranularLoadResult {
    /// Number of channels whose config was updated (label, conv, tags changed).
    pub modified: usize,
    /// Number of channels that were already up-to-date.
    pub unchanged: usize,
    /// Channel IDs of newly added virtual channels.
    pub added: Vec<u32>,
    /// Channel IDs of virtual channels removed (present in engine but
    /// absent from the new database.zinc).
    pub removed: Vec<u32>,
    /// The set of channel IDs that should be polled according to the new config.
    pub expected_polls: HashSet<u32>,
    /// Per-channel errors (non-fatal).
    pub warnings: Vec<String>,
}

/// Granular reload of database.zinc — diff-based update.
///
/// Unlike [`load_database`], this function:
/// - Updates existing virtual channels in-place instead of failing on duplicates
/// - Tracks which virtual channels were added or removed
/// - Returns the set of expected poll IDs so the caller can diff the poll list
///   without clearing it (preserving runtime state like `last_value` and
///   `unchanged_count`)
/// - Compares channel config fields to distinguish "modified" from "unchanged"
///
/// Physical channels are still updated via `update_metadata()` (which already
/// preserves runtime state like priority_array and current value).
pub fn load_database_granular<H: HalRead + HalWrite + HalDiagnostics>(
    engine: &mut Engine<H>,
    path: &Path,
) -> Result<GranularLoadResult, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    let grid = ZincGrid::parse(&content)?;

    let mut result = GranularLoadResult {
        modified: 0,
        unchanged: 0,
        added: Vec::new(),
        removed: Vec::new(),
        expected_polls: HashSet::new(),
        warnings: Vec::new(),
    };

    // Track which virtual channels appear in the new config so we can
    // detect removals afterwards.
    let mut new_virtual_ids: HashSet<u32> = HashSet::new();

    // Collect existing virtual channel IDs before we start mutating
    let old_virtual_ids: HashSet<u32> = engine
        .channels
        .iter()
        .filter(|(_, ch)| ch.channel_type.is_virtual())
        .map(|(&id, _)| id)
        .collect();

    for row in 0..grid.row_count() {
        // 1. Read channel number — skip rows without a valid channel (device header rows)
        let channel_id = grid.integer(row, "channel", -1);
        if channel_id < 0 {
            continue;
        }
        let channel_id = channel_id as u32;

        // 2. Determine enabled state
        let enabled = grid.marker(row, "enabled") && !grid.marker(row, "disabled");

        // 3. Build ValueConv from zinc tags
        let conv = build_value_conv(&grid, row, engine);

        // 4. Get display label
        let label = grid.string(row, "navName", "");

        // 5. Collect all non-null tags from this row for SVM bridge
        let mut tags = std::collections::HashMap::new();
        for col_name in grid.column_names() {
            if !grid.is_null(row, col_name) {
                let val = if grid.marker(row, col_name) {
                    "M".to_string()
                } else {
                    grid.string(row, col_name, "")
                };
                if !val.is_empty() {
                    tags.insert(col_name.clone(), val);
                }
            }
        }

        // 6. Branch: virtual vs physical channel
        if grid.marker(row, "virtualChannel") {
            // --- Virtual channel ---
            new_virtual_ids.insert(channel_id);
            result.expected_polls.insert(channel_id); // virtual channels always polled

            let channel_type = if grid.marker(row, "analog") {
                ChannelType::VirtualAnalog
            } else {
                ChannelType::VirtualDigital
            };

            if engine.channels.contains(channel_id) {
                // Virtual channel already exists — update in-place
                let changed = if let Some(ch) = engine.channels.get(channel_id) {
                    channel_config_changed(ch, &conv, &label, &tags, enabled)
                } else {
                    false
                };

                if changed {
                    if let Some(ch) = engine.channels.get_mut(channel_id) {
                        ch.conv = conv;
                        ch.label = label.clone();
                        ch.tags = tags;
                        ch.enabled = enabled;
                        ch.channel_type = channel_type;
                    }
                    result.modified += 1;
                } else {
                    result.unchanged += 1;
                }
            } else {
                // New virtual channel — add it
                let ch = Channel::new(
                    channel_id,
                    channel_type,
                    ChannelDirection::None,
                    u32::MAX,
                    u32::MAX,
                    false,
                    conv,
                    &label,
                );

                match engine.channels.add(ch) {
                    Ok(()) => {
                        if let Some(ch) = engine.channels.get_mut(channel_id) {
                            if !enabled {
                                ch.enabled = false;
                            }
                            ch.tags = tags;
                        }
                        result.added.push(channel_id);
                    }
                    Err(e) => {
                        result.warnings.push(format!(
                            "failed to add virtual channel {}: {}",
                            channel_id, e
                        ));
                    }
                }
            }
        } else {
            // --- Physical channel (must already exist from points.csv) ---
            if !engine.channels.contains(channel_id) {
                result.warnings.push(format!(
                    "channel {} in database.zinc not found in points.csv — skipping",
                    channel_id
                ));
                continue;
            }

            // Check if config actually changed before updating
            let changed = if let Some(ch) = engine.channels.get(channel_id) {
                channel_config_changed(ch, &conv, &label, &tags, enabled)
            } else {
                false
            };

            match engine
                .channels
                .update_metadata(channel_id, enabled, conv, &label)
            {
                Ok(()) => {
                    if let Some(ch) = engine.channels.get_mut(channel_id) {
                        ch.tags = tags;
                    }
                    if changed {
                        result.modified += 1;
                    } else {
                        result.unchanged += 1;
                    }
                }
                Err(e) => {
                    result.warnings.push(format!(
                        "failed to update channel {} metadata: {}",
                        channel_id, e
                    ));
                    continue;
                }
            }

            // Add to expected poll set if cur or raw marker is present
            if grid.marker(row, "cur") || grid.marker(row, "raw") {
                result.expected_polls.insert(channel_id);
            }
        }
    }

    // Detect removed virtual channels: existed before but absent in new config
    for id in &old_virtual_ids {
        if !new_virtual_ids.contains(id) {
            engine.channels.remove(*id).ok();
            result.removed.push(*id);
        }
    }

    info!(
        added = result.added.len(),
        removed = result.removed.len(),
        modified = result.modified,
        unchanged = result.unchanged,
        expected_polls = result.expected_polls.len(),
        path = %path.display(),
        "granular database.zinc reload"
    );

    Ok(result)
}

/// Compare channel config fields to determine if anything changed.
///
/// Compares label, enabled state, conversion parameters (unit, table_index,
/// min/max, low/high, offset/scale, adc_mode), and tags. Does NOT compare
/// runtime state (priority_array, value, filter states, etc.).
fn channel_config_changed(
    ch: &Channel,
    new_conv: &ValueConv,
    new_label: &str,
    new_tags: &HashMap<String, String>,
    new_enabled: bool,
) -> bool {
    if ch.label != new_label {
        return true;
    }
    if ch.enabled != new_enabled {
        return true;
    }
    if ch.tags != *new_tags {
        return true;
    }
    // Compare conversion parameters (the parts that affect behavior)
    if ch.conv.unit != new_conv.unit {
        return true;
    }
    if ch.conv.table_index != new_conv.table_index {
        return true;
    }
    if ch.conv.low != new_conv.low || ch.conv.high != new_conv.high {
        return true;
    }
    if ch.conv.offset != new_conv.offset || ch.conv.scale != new_conv.scale {
        return true;
    }
    if ch.conv.min != new_conv.min || ch.conv.max != new_conv.max {
        return true;
    }
    if ch.conv.adc_mode != new_conv.adc_mode {
        return true;
    }
    // Compare flow config presence and key fields
    match (&ch.conv.flow_config, &new_conv.flow_config) {
        (None, None) => {}
        (Some(_), None) | (None, Some(_)) => return true,
        (Some(old), Some(new)) => {
            if (old.k_factor - new.k_factor).abs() > f64::EPSILON
                || (old.dead_band - new.dead_band).abs() > f64::EPSILON
                || (old.scale_factor - new.scale_factor).abs() > f64::EPSILON
                || old.hysteresis_enabled != new.hysteresis_enabled
                || old.allow_reverse != new.allow_reverse
            {
                return true;
            }
        }
    }
    // Compare filter config
    match (&ch.conv.filter_config, &new_conv.filter_config) {
        (None, None) => {}
        (Some(_), None) | (None, Some(_)) => return true,
        (Some(old), Some(new)) => {
            if old.spike_filter != new.spike_filter
                || old.smoothing != new.smoothing
                || old.rate_limit != new.rate_limit
            {
                return true;
            }
        }
    }
    false
}

/// Build a `ValueConv` from zinc grid tags for a given row.
fn build_value_conv<H: HalRead + HalWrite + HalDiagnostics>(
    grid: &ZincGrid,
    row: usize,
    engine: &Engine<H>,
) -> ValueConv {
    let mut conv = ValueConv::default();

    // --- Core conversion parameters ---
    if !grid.is_null(row, "low") {
        conv.low = Some(grid.number(row, "low", 0.0));
    }
    if !grid.is_null(row, "high") {
        conv.high = Some(grid.number(row, "high", 1.0));
    }
    if !grid.is_null(row, "offset") {
        conv.offset = Some(grid.number(row, "offset", 0.0));
    }
    if !grid.is_null(row, "scale") {
        conv.scale = Some(grid.number(row, "scale", 1.0));
    }

    // min/max: MARKER presence gates the number from minVal/maxVal
    if grid.marker(row, "min") {
        conv.min = Some(grid.number(row, "minVal", 0.0));
    }
    if grid.marker(row, "max") {
        conv.max = Some(grid.number(row, "maxVal", 1.0));
    }

    // Unit (critical for auto-detect)
    conv.unit = grid.string(row, "unit", "");

    // ADC mode: analog + binary markers both present
    if grid.marker(row, "analog") && grid.marker(row, "binary") {
        conv.adc_mode = true;
    }

    // --- Table lookup via legacy marker tags ---
    let known_table_tags = [
        "thermistor2200",
        "thermistor3000",
        "thermistor10K1",
        "thermistor10K2",
        "thermistor10K3",
        "thermistor20K",
        "thermistor100K",
        "pt100",
        "pt500",
        "pt1000",
        "ni1000",
        "range0to10V",
        "range0to5V",
        "range0to20mA",
        "range4to20mA",
    ];
    for tag in &known_table_tags {
        if grid.marker(row, tag) {
            if let Some(idx) = engine.tables.find_by_tag(tag) {
                conv.table_index = Some(idx);
                break;
            }
        }
    }

    // --- Flow config (SDP610/SDP810) ---
    let has_flow_tags = !grid.is_null(row, "kFactor")
        || !grid.is_null(row, "deadBand")
        || grid.marker(row, "hysteresis")
        || !grid.is_null(row, "scaleFactor");

    if has_flow_tags {
        let mut flow = FlowConfig::default();

        if !grid.is_null(row, "kFactor") {
            flow.k_factor = grid.number(row, "kFactor", flow.k_factor);
        }
        if !grid.is_null(row, "deadBand") {
            flow.dead_band = grid.number(row, "deadBand", flow.dead_band);
        }
        if !grid.is_null(row, "scaleFactor") {
            flow.scale_factor = grid.number(row, "scaleFactor", flow.scale_factor);
        }
        if grid.marker(row, "hysteresis") {
            flow.hysteresis_enabled = true;
            if !grid.is_null(row, "hystOn") {
                flow.hyst_on = grid.number(row, "hystOn", flow.hyst_on);
            }
            if !grid.is_null(row, "hystOff") {
                flow.hyst_off = grid.number(row, "hystOff", flow.hyst_off);
            }
        }
        if grid.marker(row, "allowReverse") {
            flow.allow_reverse = true;
            if !grid.is_null(row, "reverseThreshold") {
                flow.reverse_threshold =
                    grid.number(row, "reverseThreshold", flow.reverse_threshold);
            }
        }

        conv.flow_config = Some(flow);
    }

    // --- Filter config ---
    let mut filter = FilterConfig::default();
    let mut has_filter = false;

    if grid.marker(row, "spikeFilter") {
        has_filter = true;
        filter.spike_filter = Some(SpikeConfig {
            threshold: grid.number(row, "spikeThreshold", 5.0),
            startup_discard: grid.number(row, "startupDiscard", 8.0) as u32,
            reverse_threshold: grid.number(row, "reverseThreshold", 10.0),
        });
    }

    if grid.marker(row, "smoothing") {
        has_filter = true;
        let method_str = grid.string(row, "smoothMethod", "median");
        let method = match method_str.to_lowercase().as_str() {
            "mean" | "average" => SmoothMethod::Mean,
            "ewma" | "exponential" => SmoothMethod::Ewma,
            _ => SmoothMethod::Median,
        };
        filter.smoothing = Some(SmoothingConfig {
            window: grid.number(row, "smoothWindow", 5.0) as usize,
            method,
        });
    }

    if grid.marker(row, "rateLimit") {
        has_filter = true;
        filter.rate_limit = Some(RateLimitConfig {
            max_rise: grid.number(row, "maxRiseRate", 100.0),
            max_fall: grid.number(row, "maxFallRate", 150.0),
        });
    }

    if has_filter {
        conv.filter_config = Some(filter);
    }

    conv
}

/// Cross-check loaded channels against available hardware subsystems.
///
/// Returns warnings for channels whose hardware is unavailable.
pub fn validate_channels_vs_hardware<H: HalRead + HalWrite + HalDiagnostics>(
    engine: &Engine<H>,
    validation: &sandstar_hal::HalValidation,
) -> Vec<String> {
    let checks: &[(ChannelType, &str)] = &[
        (ChannelType::Analog, "adc"),
        (ChannelType::I2c, "i2c"),
        (ChannelType::Digital, "gpio"),
        (ChannelType::Triac, "gpio"),
        (ChannelType::Pwm, "pwm"),
        (ChannelType::Uart, "uart"),
    ];

    let mut warnings = Vec::new();
    for &(ct, subsystem) in checks {
        if validation.is_available(subsystem) {
            continue;
        }
        let ids: Vec<u32> = engine
            .channels
            .iter()
            .filter(|(_, ch)| ch.enabled && ch.channel_type == ct)
            .map(|(&id, _)| id)
            .collect();
        if ids.is_empty() {
            continue;
        }
        let sample: Vec<String> = ids.iter().take(5).map(|id: &u32| id.to_string()).collect();
        let extra = if ids.len() > 5 {
            format!(" +{} more", ids.len() - 5)
        } else {
            String::new()
        };
        warnings.push(format!(
            "{} {:?} channels loaded but {} hardware not detected (channels: [{}]{})",
            ids.len(),
            ct,
            subsystem,
            sample.join(", "),
            extra
        ));
    }

    warnings
}

fn parse_channel_type(s: &str) -> ChannelType {
    match s.to_lowercase().as_str() {
        "analog" => ChannelType::Analog,
        "digital" => ChannelType::Digital,
        "pwm" => ChannelType::Pwm,
        "triac" => ChannelType::Triac,
        "i2c" => ChannelType::I2c,
        "uart" => ChannelType::Uart,
        "virtual_analog" | "virtualanalog" => ChannelType::VirtualAnalog,
        "virtual_digital" | "virtualdigital" => ChannelType::VirtualDigital,
        _ => {
            warn!(type_str = s, "unknown channel type, defaulting to Analog");
            ChannelType::Analog
        }
    }
}

fn parse_direction(s: &str) -> ChannelDirection {
    match s.to_lowercase().as_str() {
        "in" => ChannelDirection::In,
        "out" => ChannelDirection::Out,
        "high" => ChannelDirection::High,
        "low" => ChannelDirection::Low,
        _ => ChannelDirection::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandstar_hal::mock::MockHal;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_load_table_values() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "3927\n3921\n3915\n3909").unwrap();
        let values = load_table_values(f.path()).unwrap();
        assert_eq!(values.len(), 4);
        assert_eq!(values[0], 3927.0);
        assert_eq!(values[3], 3909.0);
    }

    #[test]
    fn test_parse_channel_type() {
        assert_eq!(parse_channel_type("analog"), ChannelType::Analog);
        assert_eq!(parse_channel_type("digital"), ChannelType::Digital);
        assert_eq!(parse_channel_type("pwm"), ChannelType::Pwm);
        assert_eq!(parse_channel_type("i2c"), ChannelType::I2c);
        assert_eq!(parse_channel_type("ANALOG"), ChannelType::Analog);
    }

    #[test]
    fn test_parse_direction() {
        assert_eq!(parse_direction("in"), ChannelDirection::In);
        assert_eq!(parse_direction("out"), ChannelDirection::Out);
        assert_eq!(parse_direction("IN"), ChannelDirection::In);
    }

    // --- database.zinc loader tests ---

    fn make_engine_with_channel(id: u32, ct: ChannelType) -> Engine<MockHal> {
        let mut engine = Engine::new(MockHal::new());
        let ch = Channel::new(
            id,
            ct,
            ChannelDirection::In,
            0,
            0,
            false,
            ValueConv::default(),
            "test",
        );
        engine.channels.add(ch).unwrap();
        engine
    }

    #[test]
    fn test_load_database_physical_channel() {
        let mut engine = make_engine_with_channel(1113, ChannelType::Analog);

        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            "ver:\"3.0\"\nchannel,enabled,unit,analog,min,minVal,max,maxVal,cur\n\
             1113,M,\"°F\",M,M,-40,M,303,M"
        )
        .unwrap();

        let count = load_database(&mut engine, f.path()).unwrap();
        assert_eq!(count, 1);

        let ch = engine.channels.get(1113).unwrap();
        assert!(ch.enabled);
        assert_eq!(ch.conv.unit, "°F");
        assert_eq!(ch.conv.min, Some(-40.0));
        assert_eq!(ch.conv.max, Some(303.0));
        assert!(engine.polls.contains(1113));
    }

    #[test]
    fn test_load_database_virtual_channel() {
        let mut engine = Engine::<MockHal>::new(MockHal::new());

        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            "ver:\"3.0\"\nchannel,enabled,virtualChannel,analog,navName\n\
             102,M,M,M,\"Test Virtual\""
        )
        .unwrap();

        let count = load_database(&mut engine, f.path()).unwrap();
        assert_eq!(count, 1);
        assert!(engine.channels.contains(102));

        let ch = engine.channels.get(102).unwrap();
        assert_eq!(ch.channel_type, ChannelType::VirtualAnalog);
        assert_eq!(ch.label, "Test Virtual");
        assert!(engine.polls.contains(102));
    }

    #[test]
    fn test_load_database_flow_config() {
        let mut engine = make_engine_with_channel(612, ChannelType::I2c);

        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            "ver:\"3.0\"\nchannel,enabled,analog,kFactor,deadBand,hysteresis,hystOn,hystOff,scaleFactor,cur,spikeFilter,spikeThreshold,startupDiscard\n\
             612,M,M,3200,1,M,16,8,60,M,M,5,5"
        )
        .unwrap();

        let count = load_database(&mut engine, f.path()).unwrap();
        assert_eq!(count, 1);

        let ch = engine.channels.get(612).unwrap();
        let flow = ch.conv.flow_config.as_ref().unwrap();
        assert_eq!(flow.k_factor, 3200.0);
        assert_eq!(flow.dead_band, 1.0);
        assert!(flow.hysteresis_enabled);
        assert_eq!(flow.hyst_on, 16.0);
        assert_eq!(flow.hyst_off, 8.0);
        assert_eq!(flow.scale_factor, 60.0);

        // Spike filter
        let spike = ch
            .conv
            .filter_config
            .as_ref()
            .unwrap()
            .spike_filter
            .as_ref()
            .unwrap();
        assert_eq!(spike.threshold, 5.0);
        assert_eq!(spike.startup_discard, 5);
    }

    #[test]
    fn test_load_database_skips_missing_physical() {
        let mut engine = Engine::<MockHal>::new(MockHal::new());
        // No channels loaded from points.csv

        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "ver:\"3.0\"\nchannel,enabled,analog\n1113,M,M").unwrap();

        let count = load_database(&mut engine, f.path()).unwrap();
        assert_eq!(count, 0); // skipped: 1113 not in ChannelStore
    }

    #[test]
    fn test_load_database_skips_device_row() {
        let mut engine = make_engine_with_channel(612, ChannelType::I2c);

        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "ver:\"3.0\"\nchannel,enabled,device,cur\n,,M,\n612,M,,M").unwrap();

        let count = load_database(&mut engine, f.path()).unwrap();
        assert_eq!(count, 1); // only row with channel=612
    }

    #[test]
    fn test_load_database_offset_scale() {
        let mut engine = make_engine_with_channel(615, ChannelType::I2c);

        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            "ver:\"3.0\"\nchannel,enabled,offset,scale,cur\n\
             615,M,32,0.009,M"
        )
        .unwrap();

        let count = load_database(&mut engine, f.path()).unwrap();
        assert_eq!(count, 1);

        let ch = engine.channels.get(615).unwrap();
        assert_eq!(ch.conv.offset, Some(32.0));
        assert_eq!(ch.conv.scale, Some(0.009));
    }
}
