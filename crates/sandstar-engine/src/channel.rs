use std::collections::HashMap;

use crate::conversion::filters::{RateLimitState, SmoothState, SpikeState};
use crate::error::{EngineError, Result};
use crate::priority::PriorityArray;
use crate::value::ValueConv;
use crate::{ChannelId, EngineValue};

/// Maximum valid raw I2C reading (signed 16-bit).
const I2C_RAW_MAX: f64 = 32767.0;

/// Baseline threshold for zero-drop detection.
const ZERO_DROP_BASELINE: f64 = 50.0;

/// Default spike ratio for SDP810 channels.
const SDP810_SPIKE_RATIO: f64 = 5.0;

/// Channel type (maps C `CHANNEL_TYPE` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelType {
    Analog,
    Digital,
    Pwm,
    Triac,
    I2c,
    Uart,
    VirtualAnalog,
    VirtualDigital,
}

impl ChannelType {
    pub fn is_virtual(&self) -> bool {
        matches!(self, Self::VirtualAnalog | Self::VirtualDigital)
    }
}

/// Channel direction (maps C `CHANNEL_DIRECTION` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelDirection {
    In,
    Out,
    High,
    Low,
    None,
}

impl ChannelDirection {
    pub fn is_output(&self) -> bool {
        matches!(self, Self::Out | Self::High | Self::Low)
    }
}

/// Channel state for flow detection (SDP810 I2C sensors).
#[derive(Debug, Clone, Default)]
pub struct FlowState {
    pub detected: bool,
}

/// A single channel (maps C `CHANNEL_ITEM`).
#[derive(Debug, Clone)]
pub struct Channel {
    pub id: ChannelId,
    pub channel_in: Option<ChannelId>,
    pub enabled: bool,
    pub channel_type: ChannelType,
    pub direction: ChannelDirection,
    pub device: u32,
    pub address: u32,
    pub trigger: bool,
    pub failed: bool,
    pub retry_counter: u32,
    pub exported: bool,
    pub conv: ValueConv,
    pub value: EngineValue,
    pub label: String,
    // Filter states (mutable per-channel)
    pub flow_state: Option<FlowState>,
    pub spike_state: SpikeState,
    pub smooth_state: SmoothState,
    pub rate_limit_state: RateLimitState,
    pub pwm_disabled: bool,
    // BACnet 17-level write priority array (lazy-allocated on first write)
    pub priority_array: Option<PriorityArray>,
    // SDP810 I2C garbage/spike tracking
    pub last_valid_value: f64,
    pub spike_reading_count: u32,
    /// Zinc tags from database.zinc (tag_name → raw cell value).
    /// Populated during load_database() for SVM bridge tag resolution.
    pub tags: HashMap<String, String>,
}

impl Channel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: ChannelId,
        channel_type: ChannelType,
        direction: ChannelDirection,
        device: u32,
        address: u32,
        trigger: bool,
        conv: ValueConv,
        label: &str,
    ) -> Self {
        let flow_state = if channel_type == ChannelType::I2c {
            Some(FlowState::default())
        } else {
            None
        };

        Self {
            id,
            channel_in: None,
            enabled: true,
            channel_type,
            direction,
            device,
            address,
            trigger,
            failed: false,
            retry_counter: 0,
            exported: false,
            conv,
            value: EngineValue::default(),
            label: label.to_string(),
            flow_state,
            spike_state: SpikeState::default(),
            smooth_state: SmoothState::default(),
            rate_limit_state: RateLimitState::default(),
            pwm_disabled: false,
            priority_array: None,
            last_valid_value: 0.0,
            spike_reading_count: 0,
            tags: HashMap::new(),
        }
    }

    /// Check if this is an SDP810 I2C flow sensor channel (610-614).
    pub fn is_sdp810(&self) -> bool {
        self.channel_type == ChannelType::I2c && (610..=614).contains(&self.id)
    }

    /// Validate raw I2C reading for SDP810 channels.
    ///
    /// Rejects garbage values: raw > 32767 or raw < 0 (I2C bus errors).
    /// Returns the validated raw value, or the last valid value if garbage.
    pub fn validate_i2c_raw(&mut self, raw: f64) -> f64 {
        if !self.is_sdp810() {
            return raw;
        }

        if !(0.0..=I2C_RAW_MAX).contains(&raw) {
            // Garbage reading — return last valid or 0
            if self.last_valid_value != 0.0 {
                self.last_valid_value
            } else {
                0.0
            }
        } else {
            raw
        }
    }

    /// Apply SDP810 spike detection after conversion.
    ///
    /// Rejects values that changed >5x from baseline.
    /// Also rejects sudden drops to ~0 when baseline >50.
    pub fn check_sdp810_spike(&mut self, cur: f64) -> f64 {
        if !self.is_sdp810() {
            return cur;
        }

        self.spike_reading_count += 1;

        // Need a baseline
        if self.last_valid_value == 0.0 || self.spike_reading_count < 3 {
            self.last_valid_value = cur;
            return cur;
        }

        let baseline = self.last_valid_value;

        // Check for sudden drop to near-zero
        if baseline.abs() > ZERO_DROP_BASELINE && cur.abs() < 1.0 {
            return baseline;
        }

        // Check for spike (>5x change)
        if baseline.abs() > 1e-10 {
            let ratio = (cur / baseline).abs();
            if ratio > SDP810_SPIKE_RATIO {
                return baseline;
            }
        }

        self.last_valid_value = cur;
        cur
    }
}

/// Collection of channels (maps C `CHANNEL` struct).
///
/// Uses HashMap for O(1) lookup instead of C's sparse array + index.
pub struct ChannelStore {
    channels: HashMap<ChannelId, Channel>,
}

impl ChannelStore {
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            channels: HashMap::with_capacity(capacity),
        }
    }

    /// Add a channel. Returns error if channel ID already exists.
    pub fn add(&mut self, channel: Channel) -> Result<()> {
        let id = channel.id;
        if self.channels.contains_key(&id) {
            return Err(EngineError::DuplicateChannel(id));
        }
        self.channels.insert(id, channel);
        Ok(())
    }

    /// Remove a channel by ID.
    pub fn remove(&mut self, id: ChannelId) -> Result<()> {
        self.channels
            .remove(&id)
            .map(|_| ())
            .ok_or(EngineError::ChannelNotFound(id))
    }

    /// Get a channel by ID (immutable).
    pub fn get(&self, id: ChannelId) -> Option<&Channel> {
        self.channels.get(&id)
    }

    /// Get a channel by ID (mutable).
    pub fn get_mut(&mut self, id: ChannelId) -> Option<&mut Channel> {
        self.channels.get_mut(&id)
    }

    /// Update channel metadata without touching hardware state.
    ///
    /// Preserves existing conv_func if the new conv doesn't have one
    /// (matches C behavior for Haystack metadata sync).
    pub fn update_metadata(
        &mut self,
        id: ChannelId,
        enabled: bool,
        mut conv: ValueConv,
        label: &str,
    ) -> Result<()> {
        let channel = self
            .channels
            .get_mut(&id)
            .ok_or(EngineError::ChannelNotFound(id))?;

        // Preserve existing conv_func if incoming has None
        if conv.conv_func.is_none() {
            conv.conv_func = channel.conv.conv_func;
        }

        // For SDP810 channels without a conv_func, auto-assign
        if conv.conv_func.is_none() && (610..=614).contains(&id) {
            conv.conv_func = crate::value::ConversionFn::from_channel_id(id);
        }

        channel.enabled = enabled;
        channel.conv = conv;
        channel.label = label.to_string();

        Ok(())
    }

    /// Update a virtual channel's input link.
    pub fn update_virtual(&mut self, id: ChannelId, channel_in: ChannelId) -> Result<()> {
        let channel = self
            .channels
            .get_mut(&id)
            .ok_or(EngineError::ChannelNotFound(id))?;
        channel.channel_in = Some(channel_in);
        Ok(())
    }

    /// Number of channels.
    pub fn count(&self) -> usize {
        self.channels.len()
    }

    /// Iterate over all channels.
    pub fn iter(&self) -> impl Iterator<Item = (&ChannelId, &Channel)> {
        self.channels.iter()
    }

    /// Iterate over all channels (mutable).
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&ChannelId, &mut Channel)> {
        self.channels.iter_mut()
    }

    /// Check if a channel exists.
    pub fn contains(&self, id: ChannelId) -> bool {
        self.channels.contains_key(&id)
    }

    /// Remove all channels. Used during config reload.
    pub fn clear(&mut self) {
        self.channels.clear();
    }
}

impl Default for ChannelStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::ValueConv;

    fn make_channel(id: ChannelId, ct: ChannelType) -> Channel {
        Channel::new(
            id,
            ct,
            ChannelDirection::In,
            0,
            0,
            false,
            ValueConv::default(),
            "test",
        )
    }

    #[test]
    fn test_channel_store_add_remove() {
        let mut store = ChannelStore::new();
        let ch = make_channel(1100, ChannelType::Analog);
        store.add(ch).unwrap();

        assert_eq!(store.count(), 1);
        assert!(store.contains(1100));

        store.remove(1100).unwrap();
        assert_eq!(store.count(), 0);
        assert!(!store.contains(1100));
    }

    #[test]
    fn test_channel_store_duplicate() {
        let mut store = ChannelStore::new();
        store.add(make_channel(1100, ChannelType::Analog)).unwrap();

        let result = store.add(make_channel(1100, ChannelType::Analog));
        assert!(result.is_err());
    }

    #[test]
    fn test_channel_store_not_found() {
        let mut store = ChannelStore::new();
        assert!(store.remove(999).is_err());
    }

    #[test]
    fn test_channel_store_get() {
        let mut store = ChannelStore::new();
        store.add(make_channel(1100, ChannelType::Analog)).unwrap();

        let ch = store.get(1100).unwrap();
        assert_eq!(ch.id, 1100);
        assert_eq!(ch.channel_type, ChannelType::Analog);
    }

    #[test]
    fn test_channel_store_update_metadata() {
        let mut store = ChannelStore::new();
        store.add(make_channel(1100, ChannelType::Analog)).unwrap();

        let conv = ValueConv {
            unit: "F".to_string(),
            ..Default::default()
        };
        store
            .update_metadata(1100, false, conv, "updated")
            .unwrap();

        let ch = store.get(1100).unwrap();
        assert!(!ch.enabled);
        assert_eq!(ch.label, "updated");
    }

    #[test]
    fn test_channel_store_update_preserves_conv_func() {
        let mut store = ChannelStore::new();
        let mut ch = make_channel(612, ChannelType::I2c);
        ch.conv.conv_func = Some(crate::value::ConversionFn::Sdp610ToCfm);
        store.add(ch).unwrap();

        // Update with conv_func = None -> should preserve existing
        let conv = ValueConv::default();
        store.update_metadata(612, true, conv, "flow").unwrap();

        let ch = store.get(612).unwrap();
        assert_eq!(
            ch.conv.conv_func,
            Some(crate::value::ConversionFn::Sdp610ToCfm)
        );
    }

    #[test]
    fn test_channel_store_auto_assign_sdp610() {
        let mut store = ChannelStore::new();
        store.add(make_channel(612, ChannelType::I2c)).unwrap();

        // Update without conv_func -> auto-assigns for SDP810 channel
        let conv = ValueConv::default();
        store.update_metadata(612, true, conv, "flow").unwrap();

        let ch = store.get(612).unwrap();
        assert_eq!(
            ch.conv.conv_func,
            Some(crate::value::ConversionFn::Sdp610ToCfm)
        );
    }

    #[test]
    fn test_virtual_channel() {
        let mut store = ChannelStore::new();
        store
            .add(make_channel(3000, ChannelType::VirtualAnalog))
            .unwrap();

        store.update_virtual(3000, 1100).unwrap();

        let ch = store.get(3000).unwrap();
        assert_eq!(ch.channel_in, Some(1100));
        assert!(ch.channel_type.is_virtual());
    }

    #[test]
    fn test_channel_type_virtual() {
        assert!(ChannelType::VirtualAnalog.is_virtual());
        assert!(ChannelType::VirtualDigital.is_virtual());
        assert!(!ChannelType::Analog.is_virtual());
        assert!(!ChannelType::I2c.is_virtual());
    }

    #[test]
    fn test_channel_direction_output() {
        assert!(ChannelDirection::Out.is_output());
        assert!(ChannelDirection::High.is_output());
        assert!(ChannelDirection::Low.is_output());
        assert!(!ChannelDirection::In.is_output());
        assert!(!ChannelDirection::None.is_output());
    }

    #[test]
    fn test_is_sdp810() {
        let ch = make_channel(612, ChannelType::I2c);
        assert!(ch.is_sdp810());

        let ch = make_channel(1100, ChannelType::Analog);
        assert!(!ch.is_sdp810());

        let ch = make_channel(612, ChannelType::Analog);
        assert!(!ch.is_sdp810());
    }

    #[test]
    fn test_validate_i2c_raw_good() {
        let mut ch = make_channel(612, ChannelType::I2c);
        assert_eq!(ch.validate_i2c_raw(100.0), 100.0);
    }

    #[test]
    fn test_validate_i2c_raw_garbage() {
        let mut ch = make_channel(612, ChannelType::I2c);
        ch.last_valid_value = 50.0;

        // Raw > 32767 (unsigned overflow from I2C bus error)
        assert_eq!(ch.validate_i2c_raw(65530.0), 50.0);

        // Raw < 0
        assert_eq!(ch.validate_i2c_raw(-1.0), 50.0);
    }

    #[test]
    fn test_validate_i2c_raw_non_sdp810() {
        let mut ch = make_channel(1100, ChannelType::Analog);
        // Non-SDP810 channels pass through unchanged
        assert_eq!(ch.validate_i2c_raw(65530.0), 65530.0);
    }

    #[test]
    fn test_sdp810_spike_detection() {
        let mut ch = make_channel(612, ChannelType::I2c);

        // Build up baseline
        ch.check_sdp810_spike(100.0);
        ch.check_sdp810_spike(100.0);
        ch.check_sdp810_spike(100.0);

        // 6x spike -> rejected
        assert_eq!(ch.check_sdp810_spike(600.0), 100.0);

        // Normal change -> accepted
        assert_eq!(ch.check_sdp810_spike(110.0), 110.0);
    }

    #[test]
    fn test_sdp810_zero_drop() {
        let mut ch = make_channel(612, ChannelType::I2c);
        ch.last_valid_value = 100.0;
        ch.spike_reading_count = 10;

        // Sudden drop to near-zero when baseline > 50
        assert_eq!(ch.check_sdp810_spike(0.5), 100.0);
    }

    #[test]
    fn test_channel_store_iter() {
        let mut store = ChannelStore::new();
        store.add(make_channel(100, ChannelType::Analog)).unwrap();
        store.add(make_channel(200, ChannelType::Digital)).unwrap();

        let ids: Vec<_> = store.iter().map(|(&id, _)| id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&100));
        assert!(ids.contains(&200));
    }

    #[test]
    fn test_channel_store_clear() {
        let mut store = ChannelStore::new();
        store.add(make_channel(100, ChannelType::Analog)).unwrap();
        store.add(make_channel(200, ChannelType::Digital)).unwrap();
        assert_eq!(store.count(), 2);

        store.clear();
        assert_eq!(store.count(), 0);
        assert!(!store.contains(100));
        assert!(!store.contains(200));
    }
}
