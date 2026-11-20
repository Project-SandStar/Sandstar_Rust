use crate::conversion::filters::{RateLimitConfig, SmoothingConfig, SpikeConfig};
use crate::conversion::sdp610;
use crate::table::TableStore;
use crate::{ChannelId, EngineError, EngineStatus, EngineValue, Result, ValueFlags};

/// Conversion function tag (replaces C function pointer dispatch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionFn {
    Sdp610ToPa,
    Sdp610ToInH2O,
    Sdp610ToPsi,
    Sdp610ToCfm,
    Sdp610ToLps,
}

impl ConversionFn {
    /// Parse from a string tag (matches C `value_add_conv_func`).
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "SDP610ToPa" => Some(Self::Sdp610ToPa),
            "SDP610ToInH2O" => Some(Self::Sdp610ToInH2O),
            "SDP610ToPSI" | "SDP610ToPsi" => Some(Self::Sdp610ToPsi),
            "SDP610ToCFM" | "SDP610ToCfm" => Some(Self::Sdp610ToCfm),
            "SDP610ToLPS" | "SDP610ToLps" => Some(Self::Sdp610ToLps),
            _ => None,
        }
    }

    /// Auto-detect conversion function from I2C channel ID (610-614).
    pub fn from_channel_id(channel: ChannelId) -> Option<Self> {
        match channel {
            610 => Some(Self::Sdp610ToInH2O),
            611 => Some(Self::Sdp610ToPa),
            612 => Some(Self::Sdp610ToCfm),
            613 => Some(Self::Sdp610ToPsi),
            614 => Some(Self::Sdp610ToLps),
            _ => None,
        }
    }

    /// Whether this conversion function is an SDP610 type.
    pub fn is_sdp610(&self) -> bool {
        true // All current variants are SDP610
    }
}

/// Flow sensor configuration (SDP610/SDP810).
#[derive(Debug, Clone, Copy)]
pub struct FlowConfig {
    pub k_factor: f64,
    pub dead_band: f64,
    pub hyst_on: f64,
    pub hyst_off: f64,
    pub scale_factor: f64,
    pub hysteresis_enabled: bool,
    pub allow_reverse: bool,
    pub reverse_threshold: f64,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            k_factor: sdp610::DEFAULT_K_FACTOR,
            dead_band: sdp610::DEFAULT_DEAD_BAND,
            hyst_on: sdp610::DEFAULT_HYST_ON,
            hyst_off: sdp610::DEFAULT_HYST_OFF,
            scale_factor: sdp610::DEFAULT_SCALE_FACTOR,
            hysteresis_enabled: false,
            allow_reverse: false,
            reverse_threshold: 10.0,
        }
    }
}

/// Filter configuration (spike, smoothing, rate limiting).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FilterConfig {
    pub spike_filter: Option<SpikeConfig>,
    pub smoothing: Option<SmoothingConfig>,
    pub rate_limit: Option<RateLimitConfig>,
}

/// Value conversion configuration (maps C `VALUE_CONV` struct).
///
/// Replaces the 17-bit flag system with `Option<T>` fields.
#[derive(Debug, Clone, Default)]
pub struct ValueConv {
    pub table_index: Option<usize>,
    pub low: Option<f64>,
    pub high: Option<f64>,
    pub offset: Option<f64>,
    pub scale: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub adc_mode: bool,
    pub dac_mode: bool,
    pub conv_func: Option<ConversionFn>,
    pub unit: String,
    pub flow_config: Option<FlowConfig>,
    pub filter_config: Option<FilterConfig>,
}

impl ValueConv {
    /// Why this conversion cannot be applied, if it cannot (R-10).
    ///
    /// `f64::clamp` panics when its lower bound is above the upper one or
    /// either is NaN, and a non-finite scale or offset makes every output
    /// non-finite. The limits come from config (Zinc), so they are checked
    /// here instead of trusted. Only limits that are both set are compared;
    /// `convert` also checks its defaults for a half-set range, which
    /// `revert` does not clamp.
    pub fn limits_error(&self) -> Option<String> {
        for (name, v) in [
            ("low", self.low),
            ("high", self.high),
            ("min", self.min),
            ("max", self.max),
            ("offset", self.offset),
            ("scale", self.scale),
        ] {
            if let Some(v) = v {
                if !v.is_finite() {
                    return Some(format!("{name} is not finite"));
                }
            }
        }
        if let (Some(low), Some(high)) = (self.low, self.high) {
            if low > high {
                return Some(format!("low {low} is above high {high}"));
            }
        }
        if let (Some(min), Some(max)) = (self.min, self.max) {
            if min > max {
                return Some(format!("min {min} is above max {max}"));
            }
        }
        None
    }

    /// Convert raw value to engineering units (maps C `value_convert`).
    ///
    /// Three conversion paths:
    /// 1. conv_func: SDP610 sensor-specific conversion
    /// 2. Table lookup: binary search interpolation
    /// 3. Range scaling: linear raw-to-unit mapping
    ///
    /// After conversion, sets status=Ok and trigger=false (matches C behavior).
    /// Writes back the clamped raw value to `value.raw`.
    ///
    /// NOTE: C calls auto_detect_sensor() inline for channels 1100-1723.
    /// In Rust, auto-detect should be called by the orchestration layer
    /// (channel_read) before convert, since convert takes &self.
    pub fn convert(
        &self,
        value: &mut EngineValue,
        tables: &TableStore,
        channel: ChannelId,
        flow_detected: &mut Option<bool>,
    ) -> Result<()> {
        // Invalid limits would panic in clamp below; fault the channel instead.
        // The range path clamps against defaults for an unset bound, so a
        // half-set range is compared against them too.
        if self.limits_error().is_some() || self.low.unwrap_or(0.0) > self.high.unwrap_or(4096.0) {
            value.status = EngineStatus::Fault;
            value.trigger = false;
            return Ok(());
        }

        let mut raw = value.raw;

        if let Some(conv_func) = self.conv_func {
            // Path 1: Function-based conversion (SDP610/SDP810)
            raw = raw.max(0.0); // Clamp negative raw

            let flow_cfg = self.flow_config.as_ref().cloned().unwrap_or_default();

            // Apply hysteresis before conversion if enabled
            if flow_cfg.hysteresis_enabled {
                if let Some(detected) = flow_detected.as_mut() {
                    raw = sdp610::apply_hysteresis(
                        raw,
                        flow_cfg.hyst_on,
                        flow_cfg.hyst_off,
                        detected,
                    );
                }
            }

            let cur = match conv_func {
                ConversionFn::Sdp610ToPa => sdp610::raw_to_pa(raw, flow_cfg.scale_factor),
                ConversionFn::Sdp610ToInH2O => sdp610::raw_to_inh2o(raw, flow_cfg.scale_factor),
                ConversionFn::Sdp610ToPsi => sdp610::raw_to_psi(raw, flow_cfg.scale_factor),
                ConversionFn::Sdp610ToCfm => sdp610::raw_to_cfm(
                    raw,
                    flow_cfg.k_factor,
                    flow_cfg.dead_band,
                    flow_cfg.scale_factor,
                ),
                ConversionFn::Sdp610ToLps => sdp610::raw_to_lps(
                    raw,
                    flow_cfg.k_factor,
                    flow_cfg.dead_band,
                    flow_cfg.scale_factor,
                ),
            };

            // Apply combined formula if all parameters set
            let cur = self.apply_combined_transform(cur);

            // Apply min/max clamping
            let cur = self.clamp_output(cur);

            // Write back values (matches C: value->raw = raw; value->cur = cur;)
            value.status = EngineStatus::Ok;
            value.raw = raw;
            value.cur = cur;
            value.flags |= ValueFlags::CUR;
            value.trigger = false;
        } else {
            // Path 2/3: Table or range-based conversion
            self.convert_table_or_range(value, tables, channel)?;
        }

        Ok(())
    }

    /// Reverse convert: engineering units back to raw (maps C `value_revert`).
    ///
    /// After reversion, sets status=Ok and trigger=false (matches C behavior).
    /// Writes back both raw and (clamped) cur values.
    ///
    /// The result goes to hardware, so a non-finite value or invalid limits
    /// are an error and nothing is written back (numeric-output.md, R-9):
    /// `f64::min` would turn NaN into `max`.
    pub fn revert(
        &self,
        value: &mut EngineValue,
        tables: &TableStore,
        channel: ChannelId,
    ) -> Result<()> {
        if let Some(reason) = self.limits_error() {
            return Err(EngineError::ConversionFault(channel, reason));
        }
        if !value.cur.is_finite() {
            return Err(EngineError::NonFiniteValue(channel));
        }
        let mut cur = value.cur;

        // Clamp to [min, max]
        if let Some(max_val) = self.max {
            cur = cur.min(max_val);
        }
        if let Some(min_val) = self.min {
            cur = cur.max(min_val);
        }

        // Reverse scale
        if let Some(scale) = self.scale {
            if scale != 0.0 {
                cur /= scale;
            }
        }

        // Reverse offset
        if let Some(offset) = self.offset {
            cur -= offset;
        }
        // A huge cur over a tiny scale overflows; checked before any clamp
        // below can turn infinity into an end position (R-9).
        if !cur.is_finite() {
            return Err(EngineError::ConversionFault(
                channel,
                format!("reverted value {cur} is not finite"),
            ));
        }

        let raw = if self.adc_mode {
            // ADC: binary threshold
            let low = self.low.unwrap_or(0.0);
            let high = self.high.unwrap_or(4096.0);
            if cur <= 0.0 {
                low
            } else {
                high
            }
        } else if let Some(table_idx) = self.table_index {
            // Table reverse lookup
            let min = self.min.unwrap_or(0.0);
            let max = self.max.unwrap_or(100.0);
            tables
                .reverse_lookup(table_idx, cur, min, max)
                .unwrap_or(cur)
        } else {
            // Range un-scaling
            let low = self.low.unwrap_or(0.0);
            let high = self.high.unwrap_or(1.0);
            cur * (high - low)
        };

        // The range un-scaling can still overflow; check before the clamp.
        if !raw.is_finite() {
            return Err(EngineError::ConversionFault(
                channel,
                format!("reverted raw value {raw} is not finite"),
            ));
        }

        // Clamp to [low, high]; limits_error() above guarantees low <= high.
        let raw = if let (Some(low), Some(high)) = (self.low, self.high) {
            raw.clamp(low, high)
        } else {
            raw
        };

        // Write back values (matches C: status=Ok, both raw+cur written, flags |= RAW)
        value.status = EngineStatus::Ok;
        value.raw = raw;
        value.cur = cur;
        value.flags |= ValueFlags::RAW;
        value.trigger = false;
        Ok(())
    }

    /// Table or range-based conversion (non-function path).
    ///
    /// Returns FAULT status if table is set but no low/high range configured
    /// (matches C behavior when USERANGE flag is not set).
    fn convert_table_or_range(
        &self,
        value: &mut EngineValue,
        tables: &TableStore,
        _channel: ChannelId,
    ) -> Result<()> {
        // Issue #6: FAULT if table set but no low/high range (matches C USERANGE check)
        if self.table_index.is_some() && self.low.is_none() && self.high.is_none() {
            value.status = EngineStatus::Fault;
            value.trigger = false;
            return Ok(());
        }

        let low = self.low.unwrap_or(0.0);
        let high = self.high.unwrap_or(4096.0);
        let unclamped_raw = value.raw;
        let raw = unclamped_raw.clamp(low, high);

        let cur = if self.adc_mode {
            // ADC mode: threshold at midpoint -> binary
            let mid = low + (high - low) * 0.5;
            if raw <= mid {
                0.0
            } else {
                1.0
            }
        } else if let Some(table_idx) = self.table_index {
            // Table lookup with binary search interpolation
            let min = self.min.unwrap_or(0.0);
            let max = self.max.unwrap_or(100.0);

            // Out-of-range fault detection: if the unclamped raw is at or
            // beyond the table boundaries (with 2% margin), the sensor is
            // likely disconnected (open circuit → low raw) or shorted
            // (saturated → high raw). Report Fault with the boundary value
            // so consumers know the reading is unreliable.
            let range = high - low;
            let margin = range * 0.02;
            if unclamped_raw <= low + margin {
                value.status = EngineStatus::Fault;
                value.raw = unclamped_raw;
                value.cur = min;
                value.flags |= ValueFlags::CUR;
                value.trigger = false;
                return Ok(());
            } else if unclamped_raw >= high - margin {
                value.status = EngineStatus::Fault;
                value.raw = unclamped_raw;
                value.cur = max;
                value.flags |= ValueFlags::CUR;
                value.trigger = false;
                return Ok(());
            }

            // Normal table lookup (raw is within valid range)
            tables.lookup(table_idx, raw, min, max).unwrap_or_else(|| {
                // Fallback: range scaling
                let range = high - low;
                if range != 0.0 {
                    raw / range
                } else {
                    0.0
                }
            })
        } else if self.low.is_some() && self.high.is_some() {
            // Range scaling (no table)
            let range = high - low;
            if range != 0.0 {
                raw / range
            } else {
                0.0
            }
        } else {
            raw
        };

        // Apply offset
        let cur = if let Some(offset) = self.offset {
            cur + offset
        } else {
            cur
        };

        // Apply scale
        let cur = if let Some(scale) = self.scale {
            cur * scale
        } else {
            cur
        };

        // Clamp to [min, max]
        let cur = self.clamp_output(cur);

        // Write back values (matches C: status=Ok, raw written back clamped, flags |= CUR)
        value.status = EngineStatus::Ok;
        value.raw = raw;
        value.cur = cur;
        value.flags |= ValueFlags::CUR;
        value.trigger = false;
        Ok(())
    }

    /// Apply combined transform when all parameters are present.
    ///
    /// Formula: fmax(fmin(((cur / (high - low)) * scale) + offset, max), min)
    fn apply_combined_transform(&self, cur: f64) -> f64 {
        if let (Some(low), Some(high), Some(offset), Some(scale), Some(min), Some(max)) = (
            self.low,
            self.high,
            self.offset,
            self.scale,
            self.min,
            self.max,
        ) {
            let range = high - low;
            if range != 0.0 {
                let result = ((cur / range) * scale) + offset;
                return result.clamp(min, max);
            }
        }
        cur
    }

    /// Clamp output to [min, max] if set.
    fn clamp_output(&self, cur: f64) -> f64 {
        let cur = if let Some(min) = self.min {
            cur.max(min)
        } else {
            cur
        };
        if let Some(max) = self.max {
            cur.min(max)
        } else {
            cur
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{TableRanges, TableStore};
    use crate::EngineValue;

    #[allow(dead_code)]
    fn make_simple_conv() -> ValueConv {
        ValueConv {
            low: Some(0.0),
            high: Some(4096.0),
            min: Some(0.0),
            max: Some(100.0),
            ..Default::default()
        }
    }

    fn make_table_store() -> TableStore {
        let mut store = TableStore::new();
        // Simple linear table: 5 points from 0 to 4096
        store
            .add_with_values(
                "test_table",
                "temp",
                vec![0.0, 1024.0, 2048.0, 3072.0, 4096.0],
                TableRanges::default(),
            )
            .unwrap();
        store
    }

    #[test]
    fn test_convert_range_scaling() {
        let conv = ValueConv {
            low: Some(0.0),
            high: Some(4096.0),
            ..Default::default()
        };
        let mut value = EngineValue::default();
        value.set_raw(2048.0);

        let tables = TableStore::new();
        let mut flow = None;
        conv.convert(&mut value, &tables, 0, &mut flow).unwrap();

        assert!((value.cur - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_convert_with_offset_scale() {
        let conv = ValueConv {
            low: Some(0.0),
            high: Some(4096.0),
            offset: Some(10.0),
            scale: Some(2.0),
            ..Default::default()
        };
        let mut value = EngineValue::default();
        value.set_raw(2048.0);

        let tables = TableStore::new();
        let mut flow = None;
        conv.convert(&mut value, &tables, 0, &mut flow).unwrap();

        // raw/range = 2048/4096 = 0.5, + offset = 10.5, * scale = 21.0
        assert!((value.cur - 21.0).abs() < 0.01);
    }

    #[test]
    fn test_convert_table_lookup() {
        let tables = make_table_store();
        let idx = tables.find_by_tag("test_table").unwrap();

        let conv = ValueConv {
            table_index: Some(idx),
            low: Some(0.0),
            high: Some(4096.0),
            min: Some(-40.0),
            max: Some(120.0),
            ..Default::default()
        };

        let mut value = EngineValue::default();
        value.set_raw(2048.0); // Middle of table

        let mut flow = None;
        conv.convert(&mut value, &tables, 0, &mut flow).unwrap();

        // 2048 is at index 2 out of [0,1,2,3,4] -> maps to -40 + 2 * 40 = 40
        assert!((value.cur - 40.0).abs() < 0.01);
    }

    #[test]
    fn test_convert_adc_mode() {
        let conv = ValueConv {
            low: Some(0.0),
            high: Some(4096.0),
            adc_mode: true,
            ..Default::default()
        };

        let tables = TableStore::new();
        let mut flow = None;

        // Below midpoint -> 0
        let mut value = EngineValue::default();
        value.set_raw(1000.0);
        conv.convert(&mut value, &tables, 0, &mut flow).unwrap();
        assert_eq!(value.cur, 0.0);

        // Above midpoint -> 1
        let mut value = EngineValue::default();
        value.set_raw(3000.0);
        conv.convert(&mut value, &tables, 0, &mut flow).unwrap();
        assert_eq!(value.cur, 1.0);
    }

    #[test]
    fn test_convert_sdp610_cfm() {
        let conv = ValueConv {
            conv_func: Some(ConversionFn::Sdp610ToCfm),
            flow_config: Some(FlowConfig::default()),
            ..Default::default()
        };

        let mut value = EngineValue::default();
        value.set_raw(60.0);

        let tables = TableStore::new();
        let mut flow = None;
        conv.convert(&mut value, &tables, 612, &mut flow).unwrap();

        assert!(value.cur > 0.0);
    }

    #[test]
    fn test_convert_sdp610_dead_band() {
        let conv = ValueConv {
            conv_func: Some(ConversionFn::Sdp610ToCfm),
            flow_config: Some(FlowConfig::default()),
            ..Default::default()
        };

        let mut value = EngineValue::default();
        value.set_raw(3.0); // Below dead band of 5

        let tables = TableStore::new();
        let mut flow = None;
        conv.convert(&mut value, &tables, 612, &mut flow).unwrap();

        assert_eq!(value.cur, 0.0);
    }

    #[test]
    fn test_convert_sdp610_hysteresis() {
        let conv = ValueConv {
            conv_func: Some(ConversionFn::Sdp610ToPa),
            flow_config: Some(FlowConfig {
                hysteresis_enabled: true,
                ..FlowConfig::default()
            }),
            ..Default::default()
        };

        let tables = TableStore::new();
        let mut flow = Some(false);

        // Below hysteresis on threshold (16) -> 0
        let mut value = EngineValue::default();
        value.set_raw(10.0);
        conv.convert(&mut value, &tables, 611, &mut flow).unwrap();
        assert_eq!(value.cur, 0.0);

        // Above hysteresis on threshold -> converts
        let mut value = EngineValue::default();
        value.set_raw(20.0);
        conv.convert(&mut value, &tables, 611, &mut flow).unwrap();
        assert!(value.cur > 0.0);
    }

    #[test]
    fn test_revert_range() {
        let conv = ValueConv {
            low: Some(0.0),
            high: Some(4096.0),
            ..Default::default()
        };

        let mut value = EngineValue::default();
        value.set_cur(0.5);

        let tables = TableStore::new();
        conv.revert(&mut value, &tables, 0).unwrap();

        assert!((value.raw - 2048.0).abs() < 0.01);
    }

    #[test]
    fn test_revert_adc() {
        let conv = ValueConv {
            low: Some(0.0),
            high: Some(4096.0),
            adc_mode: true,
            ..Default::default()
        };

        let tables = TableStore::new();

        // cur <= 0 -> low
        let mut value = EngineValue::default();
        value.set_cur(0.0);
        conv.revert(&mut value, &tables, 0).unwrap();
        assert_eq!(value.raw, 0.0);

        // cur > 0 -> high
        let mut value = EngineValue::default();
        value.set_cur(1.0);
        conv.revert(&mut value, &tables, 0).unwrap();
        assert_eq!(value.raw, 4096.0);
    }

    #[test]
    fn test_conversion_fn_from_tag() {
        assert_eq!(
            ConversionFn::from_tag("SDP610ToPa"),
            Some(ConversionFn::Sdp610ToPa)
        );
        assert_eq!(
            ConversionFn::from_tag("SDP610ToCFM"),
            Some(ConversionFn::Sdp610ToCfm)
        );
        assert_eq!(ConversionFn::from_tag("unknown"), None);
    }

    #[test]
    fn test_conversion_fn_from_channel() {
        assert_eq!(
            ConversionFn::from_channel_id(610),
            Some(ConversionFn::Sdp610ToInH2O)
        );
        assert_eq!(
            ConversionFn::from_channel_id(612),
            Some(ConversionFn::Sdp610ToCfm)
        );
        assert_eq!(ConversionFn::from_channel_id(999), None);
    }

    #[test]
    fn test_clamp_output() {
        let conv = ValueConv {
            min: Some(0.0),
            max: Some(100.0),
            ..Default::default()
        };

        assert_eq!(conv.clamp_output(-10.0), 0.0);
        assert_eq!(conv.clamp_output(50.0), 50.0);
        assert_eq!(conv.clamp_output(150.0), 100.0);
    }

    // --- Behavioral fidelity tests (Phase 2 audit fixes) ---

    #[test]
    fn test_convert_sets_status_ok() {
        let conv = ValueConv {
            low: Some(0.0),
            high: Some(4096.0),
            ..Default::default()
        };
        let mut value = EngineValue::default();
        assert_eq!(value.status, crate::EngineStatus::Unknown);

        value.set_raw(2048.0);
        let tables = TableStore::new();
        let mut flow = None;
        conv.convert(&mut value, &tables, 0, &mut flow).unwrap();

        assert_eq!(value.status, crate::EngineStatus::Ok);
        assert!(!value.trigger);
    }

    #[test]
    fn test_convert_writes_back_clamped_raw() {
        let conv = ValueConv {
            low: Some(100.0),
            high: Some(3000.0),
            ..Default::default()
        };
        let mut value = EngineValue::default();
        value.set_raw(5000.0); // Exceeds high

        let tables = TableStore::new();
        let mut flow = None;
        conv.convert(&mut value, &tables, 0, &mut flow).unwrap();

        // Raw should be clamped to high
        assert_eq!(value.raw, 3000.0);
    }

    #[test]
    fn test_convert_flags_or_not_replace() {
        let conv = ValueConv {
            low: Some(0.0),
            high: Some(4096.0),
            ..Default::default()
        };
        let mut value = EngineValue::default();
        value.set_raw(2048.0);
        // After set_raw, flags = RAW (replaced)
        assert_eq!(value.flags, crate::ValueFlags::RAW);

        let tables = TableStore::new();
        let mut flow = None;
        conv.convert(&mut value, &tables, 0, &mut flow).unwrap();

        // After convert, flags should be RAW | CUR (OR'd)
        assert!(value.flags.contains(crate::ValueFlags::RAW));
        assert!(value.flags.contains(crate::ValueFlags::CUR));
    }

    #[test]
    fn test_revert_sets_status_ok() {
        let conv = ValueConv {
            low: Some(0.0),
            high: Some(4096.0),
            ..Default::default()
        };
        let mut value = EngineValue::default();
        assert_eq!(value.status, crate::EngineStatus::Unknown);

        value.set_cur(0.5);
        let tables = TableStore::new();
        conv.revert(&mut value, &tables, 0).unwrap();

        assert_eq!(value.status, crate::EngineStatus::Ok);
        assert!(!value.trigger);
    }

    #[test]
    fn test_revert_flags_or_not_replace() {
        let conv = ValueConv {
            low: Some(0.0),
            high: Some(4096.0),
            ..Default::default()
        };
        let mut value = EngineValue::default();
        value.set_cur(0.5);
        // After set_cur, flags = CUR (replaced)
        assert_eq!(value.flags, crate::ValueFlags::CUR);

        let tables = TableStore::new();
        conv.revert(&mut value, &tables, 0).unwrap();

        // After revert, flags should be CUR | RAW (OR'd)
        assert!(value.flags.contains(crate::ValueFlags::RAW));
        assert!(value.flags.contains(crate::ValueFlags::CUR));
    }

    #[test]
    fn test_set_raw_replaces_flags() {
        let mut value = EngineValue::default();
        value.set_cur(1.0); // flags = CUR
        value.set_raw(2.0); // flags should replace to RAW (not RAW|CUR)
        assert_eq!(value.flags, crate::ValueFlags::RAW);
        assert!(!value.trigger);
    }

    #[test]
    fn test_set_cur_replaces_flags() {
        let mut value = EngineValue::default();
        value.set_raw(1.0); // flags = RAW
        value.set_cur(2.0); // flags should replace to CUR (not RAW|CUR)
        assert_eq!(value.flags, crate::ValueFlags::CUR);
        assert!(!value.trigger);
    }

    #[test]
    fn test_table_no_range_returns_fault() {
        let tables = make_table_store();
        let idx = tables.find_by_tag("test_table").unwrap();

        // Table set but NO low/high -> should FAULT
        let conv = ValueConv {
            table_index: Some(idx),
            // low: None, high: None  (no USERANGE equivalent)
            ..Default::default()
        };

        let mut value = EngineValue::default();
        value.set_raw(2048.0);

        let mut flow = None;
        conv.convert(&mut value, &tables, 0, &mut flow).unwrap();

        assert_eq!(value.status, crate::EngineStatus::Fault);
    }

    #[test]
    fn test_sdp610_convert_sets_status_ok() {
        let conv = ValueConv {
            conv_func: Some(ConversionFn::Sdp610ToPa),
            flow_config: Some(FlowConfig::default()),
            ..Default::default()
        };

        let mut value = EngineValue::default();
        value.set_raw(120.0);

        let tables = TableStore::new();
        let mut flow = None;
        conv.convert(&mut value, &tables, 611, &mut flow).unwrap();

        assert_eq!(value.status, crate::EngineStatus::Ok);
        assert!(!value.trigger);
        assert!(value.cur > 0.0);
    }

    // -- ADC out-of-range fault detection tests --------------------------------

    #[test]
    fn test_table_raw_below_low_is_fault() {
        let tables = make_table_store();
        let idx = tables.find_by_tag("test_table").unwrap();

        let conv = ValueConv {
            table_index: Some(idx),
            low: Some(500.0),
            high: Some(32000.0),
            min: Some(-40.0),
            max: Some(200.0),
            ..Default::default()
        };

        // raw=92 is well below low=500 (disconnected thermistor)
        let mut value = EngineValue::default();
        value.set_raw(92.0);

        let mut flow = None;
        conv.convert(&mut value, &tables, 1113, &mut flow).unwrap();

        assert_eq!(value.status, crate::EngineStatus::Fault);
        assert_eq!(value.raw, 92.0);
        assert_eq!(value.cur, -40.0); // min value
    }

    #[test]
    fn test_table_raw_above_high_is_fault() {
        let tables = make_table_store();
        let idx = tables.find_by_tag("test_table").unwrap();

        let conv = ValueConv {
            table_index: Some(idx),
            low: Some(500.0),
            high: Some(32000.0),
            min: Some(-40.0),
            max: Some(200.0),
            ..Default::default()
        };

        // raw=33000 is above high=32000 (shorted sensor)
        let mut value = EngineValue::default();
        value.set_raw(33000.0);

        let mut flow = None;
        conv.convert(&mut value, &tables, 1113, &mut flow).unwrap();

        assert_eq!(value.status, crate::EngineStatus::Fault);
        assert_eq!(value.raw, 33000.0);
        assert_eq!(value.cur, 200.0); // max value
    }

    #[test]
    fn test_table_raw_within_range_is_ok() {
        let tables = make_table_store();
        let idx = tables.find_by_tag("test_table").unwrap();

        let conv = ValueConv {
            table_index: Some(idx),
            low: Some(0.0),
            high: Some(4096.0),
            min: Some(-40.0),
            max: Some(120.0),
            ..Default::default()
        };

        // raw=2048 is safely within range
        let mut value = EngineValue::default();
        value.set_raw(2048.0);

        let mut flow = None;
        conv.convert(&mut value, &tables, 1113, &mut flow).unwrap();

        assert_eq!(value.status, crate::EngineStatus::Ok);
    }

    #[test]
    fn test_table_raw_near_boundary_is_fault() {
        let tables = make_table_store();
        let idx = tables.find_by_tag("test_table").unwrap();

        let conv = ValueConv {
            table_index: Some(idx),
            low: Some(500.0),
            high: Some(32000.0),
            min: Some(-40.0),
            max: Some(200.0),
            ..Default::default()
        };

        // 2% margin of 31500 range = 630. So 500 + 630 = 1130.
        // raw=510 is within margin of low → Fault
        let mut value = EngineValue::default();
        value.set_raw(510.0);

        let mut flow = None;
        conv.convert(&mut value, &tables, 1113, &mut flow).unwrap();
        assert_eq!(value.status, crate::EngineStatus::Fault);

        // raw=1200 is outside margin → Ok
        let mut value = EngineValue::default();
        value.set_raw(1200.0);
        conv.convert(&mut value, &tables, 1113, &mut flow).unwrap();
        assert_eq!(value.status, crate::EngineStatus::Ok);
    }

    #[test]
    fn test_range_scaling_no_fault_detection() {
        // Range-scaling (no table) should NOT trigger fault detection
        let conv = ValueConv {
            low: Some(0.0),
            high: Some(4096.0),
            ..Default::default()
        };

        let mut value = EngineValue::default();
        value.set_raw(5.0); // Near zero but range-scaling, not table

        let tables = TableStore::new();
        let mut flow = None;
        conv.convert(&mut value, &tables, 0, &mut flow).unwrap();

        assert_eq!(value.status, crate::EngineStatus::Ok);
    }

    // --- S-25: invalid limits and non-finite values (R-9, R-10) ---

    #[test]
    fn limits_error_reports_unusable_limits() {
        assert_eq!(ValueConv::default().limits_error(), None);
        let cases: [fn(&mut ValueConv); 5] = [
            |c| {
                c.low = Some(10.0);
                c.high = Some(0.0);
            },
            |c| {
                c.min = Some(5.0);
                c.max = Some(1.0);
            },
            |c| c.low = Some(f64::NAN),
            |c| c.max = Some(f64::INFINITY),
            |c| c.scale = Some(f64::NAN),
        ];
        for (i, set) in cases.iter().enumerate() {
            let mut conv = ValueConv::default();
            set(&mut conv);
            assert!(conv.limits_error().is_some(), "case {i}");
        }
    }

    #[test]
    fn convert_faults_on_inverted_range_instead_of_panicking() {
        let tables = TableStore::new();
        let conv = ValueConv {
            low: Some(4096.0),
            high: Some(0.0),
            ..Default::default()
        };
        let mut value = EngineValue::default();
        value.raw = 100.0;
        let mut flow = None;
        conv.convert(&mut value, &tables, 0, &mut flow).unwrap();
        assert_eq!(value.status, crate::EngineStatus::Fault);
    }

    #[test]
    fn revert_rejects_non_finite_cur_instead_of_returning_max() {
        let tables = TableStore::new();
        let conv = ValueConv {
            min: Some(0.0),
            max: Some(100.0),
            ..Default::default()
        };
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut value = EngineValue::default();
            value.cur = v;
            let raw_before = value.raw;
            let err = conv.revert(&mut value, &tables, 9).unwrap_err();
            assert!(
                matches!(err, EngineError::NonFiniteValue(9)),
                "{v}: got {err:?}"
            );
            assert_eq!(value.raw, raw_before, "{v} changed raw");
        }
    }

    #[test]
    fn revert_rejects_inverted_limits_and_overflow() {
        let tables = TableStore::new();
        let inverted = ValueConv {
            low: Some(10.0),
            high: Some(0.0),
            ..Default::default()
        };
        let mut value = EngineValue::default();
        value.cur = 5.0;
        assert!(matches!(
            inverted.revert(&mut value, &tables, 3),
            Err(EngineError::ConversionFault(3, _))
        ));

        // A tiny scale overflows a finite cur to infinity. With low/high set
        // the clamp used to turn that into high, the end position.
        for (low, high) in [(None, None), (Some(0.0), Some(4096.0))] {
            let tiny_scale = ValueConv {
                low,
                high,
                scale: Some(1e-300),
                ..Default::default()
            };
            let mut value = EngineValue::default();
            value.cur = 1e300;
            let raw_before = value.raw;
            assert!(
                matches!(
                    tiny_scale.revert(&mut value, &tables, 3),
                    Err(EngineError::ConversionFault(3, _))
                ),
                "low {low:?} high {high:?}"
            );
            assert_eq!(value.raw, raw_before);
        }
    }

    #[test]
    fn half_set_range_faults_reads_but_still_reverts() {
        // low above the convert default high (4096) with no high set: the
        // read path compares against the default and faults; writes, which
        // never clamped a half-set range, keep working.
        let tables = TableStore::new();
        let conv = ValueConv {
            low: Some(5000.0),
            ..Default::default()
        };
        assert_eq!(conv.limits_error(), None);
        let mut value = EngineValue::default();
        value.raw = 100.0;
        let mut flow = None;
        conv.convert(&mut value, &tables, 0, &mut flow).unwrap();
        assert_eq!(value.status, crate::EngineStatus::Fault);

        let mut value = EngineValue::default();
        value.cur = 0.5;
        conv.revert(&mut value, &tables, 0).unwrap();
        assert!(value.raw.is_finite());
    }
}
