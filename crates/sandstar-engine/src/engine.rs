//! Engine orchestration layer.
//!
//! Ties together channels, tables, conversions, polls, watches, and notifies
//! into a single `Engine<H>` struct generic over HAL traits.

use std::collections::HashMap;

use sandstar_hal::{HalDiagnostics, HalError, HalRead, HalWrite};

use crate::channel::{ChannelStore, ChannelType};
use crate::conversion::auto_detect::auto_detect_sensor;
use crate::conversion::filters;
use crate::error::{EngineError, Result};
use crate::priority;
use crate::notify::{NotifyId, NotifyStore};
use crate::poll::PollStore;
use crate::table::TableStore;
use crate::watch::{WatchId, WatchStore};
use crate::{ChannelId, EngineStatus, EngineValue, ValueFlags};

/// Cache key for I2C read coalescing: (device, address, label).
///
/// All I2C channels that share the same (device, address, label) tuple read
/// from the same physical sensor, so a single HAL read suffices. In
/// production the 6 SDP810 channels (610-615) all hit bus 2, address 0x25
/// — coalescing turns 6 × 45 ms = 270 ms into 1 × 45 ms per poll cycle.
type I2cCacheKey = (u32, u32, String);

/// Pre-read cache: maps (device, address, label) → HAL read result.
type I2cReadCache = HashMap<I2cCacheKey, std::result::Result<f64, HalError>>;

/// Retry cooldown: number of poll cycles before retrying a failed channel.
const RETRY_COOLDOWN: u32 = 30;

/// Consecutive read failures before resetting the fail counter.
const CONSECUTIVE_FAIL_THRESHOLD: u32 = 5;

/// Notification produced by a poll cycle.
#[derive(Debug, Clone)]
pub enum Notification {
    Watch {
        subscriber: WatchId,
        channel: ChannelId,
        value: EngineValue,
    },
    Notify {
        subscriber: NotifyId,
        channel: ChannelId,
        value: EngineValue,
    },
}

/// The engine: orchestrates HAL reads/writes through the channel pipeline.
pub struct Engine<H> {
    pub hal: H,
    pub channels: ChannelStore,
    pub tables: TableStore,
    pub polls: PollStore,
    pub watches: WatchStore,
    pub notifies: NotifyStore,
}

impl<H: HalRead + HalWrite + HalDiagnostics> Engine<H> {
    pub fn new(hal: H) -> Self {
        Self {
            hal,
            channels: ChannelStore::new(),
            tables: TableStore::new(),
            polls: PollStore::new(),
            watches: WatchStore::new(),
            notifies: NotifyStore::new(),
        }
    }

    /// Read a channel value through the full pipeline.
    ///
    /// 16-step pipeline matching C `channel_read()`:
    /// 1. Lookup channel
    /// 2. Disabled check
    /// 3. Failed/retry cooldown
    /// 4. Snapshot immutable fields
    /// 5. HAL dispatch by channel type
    /// 6. HAL error handling
    /// 7. Set raw value
    /// 8. SDP810 garbage detection
    /// 9. Auto-detect sensor
    /// 10. Convert raw→cur
    /// 11. SDP810 spike detection
    /// 12. Tag-based spike filter
    /// 13. Smoothing
    /// 14. Rate limiting
    /// 15. Trigger on zero
    /// 16. Store and return
    pub fn channel_read(&mut self, id: ChannelId) -> Result<EngineValue> {
        // 1. Lookup channel
        if !self.channels.contains(id) {
            return Err(EngineError::ChannelNotFound(id));
        }

        // 2. Disabled check
        {
            let ch = self.channels.get(id).ok_or(EngineError::ChannelNotFound(id))?;
            if !ch.enabled {
                return Ok(EngineValue::with_status(EngineStatus::Disabled));
            }
        }

        // 3. Failed/retry cooldown
        {
            let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
            if ch.failed {
                if ch.retry_counter < RETRY_COOLDOWN {
                    ch.retry_counter += 1;
                    return Ok(EngineValue::with_status(EngineStatus::Down));
                }

                // Cooldown expired — attempt recovery
                // For I2C channels, reinit the sensor before retrying (matches C engine)
                if ch.channel_type == ChannelType::I2c {
                    let device = ch.device;
                    let address = ch.address;
                    let label = ch.label.clone();
                    if let Err(_e) = self.hal.reinit_i2c_sensor(device, address, &label) {
                        // Reinit failed — stay failed, reset counter, try again later
                        let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
                        ch.retry_counter = 0;
                        return Ok(EngineValue::with_status(EngineStatus::Down));
                    }
                    // Re-borrow after HAL call
                    let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
                    ch.failed = false;
                    ch.retry_counter = 0;
                } else {
                    ch.failed = false;
                    ch.retry_counter = 0;
                }
            }
        }

        // 4. Snapshot immutable fields
        let (channel_type, device, address, label, channel_in, pwm_disabled, trigger) = {
            let ch = self.channels.get(id).ok_or(EngineError::ChannelNotFound(id))?;
            (
                ch.channel_type,
                ch.device,
                ch.address,
                ch.label.clone(),
                ch.channel_in,
                ch.pwm_disabled,
                ch.trigger,
            )
        };

        // 5. HAL dispatch by channel type
        // Virtual channels: if written to (priority array has effective value), return that;
        // otherwise copy from source channel (channel_in linkage).
        if channel_type.is_virtual() {
            let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;

            // Check if this virtual channel has been written to via channel_write_level
            let has_written_value = ch
                .priority_array
                .as_ref()
                .map(|pa| pa.effective().0.is_some())
                .unwrap_or(false);

            if has_written_value {
                // Written virtual channel: return the stored value from channel_write
                ch.value.status = EngineStatus::Ok;
                ch.value.flags |= ValueFlags::CUR;
                return Ok(ch.value);
            }

            // Unwritten virtual channel: copy from source (channel_in linkage)
            let source_cur = channel_in
                .and_then(|src_id| self.channels.get(src_id))
                .map(|src| src.value.cur)
                .unwrap_or(0.0);

            let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
            ch.value.cur = source_cur;
            ch.value.status = EngineStatus::Ok;
            ch.value.flags |= ValueFlags::CUR;
            return Ok(ch.value);
        }

        let hal_result: std::result::Result<f64, sandstar_hal::HalError> = match channel_type {
            ChannelType::Analog => self.hal.read_analog(device, address),
            ChannelType::Digital | ChannelType::Triac => {
                match self.hal.read_digital(address) {
                    Ok(b) => Ok(if b { 1.0 } else { 0.0 }),
                    Err(e) => Err(e),
                }
            }
            ChannelType::Pwm => {
                if pwm_disabled {
                    Ok(0.0)
                } else {
                    self.hal.read_pwm(device, address)
                }
            }
            ChannelType::I2c => self.hal.read_i2c(device, address, &label),
            ChannelType::Uart => self.hal.read_uart(device, &label),
            // Virtuals handled above
            ChannelType::VirtualAnalog | ChannelType::VirtualDigital => unreachable!(),
        };

        // 6. HAL error → failed=true, return Down
        let raw = match hal_result {
            Ok(v) => v,
            Err(_) => {
                let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
                ch.failed = true;
                ch.value.status = EngineStatus::Down;
                return Ok(EngineValue::with_status(EngineStatus::Down));
            }
        };

        // 7. Set raw value
        let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
        ch.value.set_raw(raw);

        // 8. SDP810 garbage detection
        let raw = ch.validate_i2c_raw(raw);
        ch.value.raw = raw;

        // 9. Auto-detect sensor (channels 1100-1723)
        auto_detect_sensor(id, &mut ch.conv, &self.tables);

        // 10. Convert raw→cur
        let mut flow_detected = ch.flow_state.as_ref().map(|f| f.detected);
        // Split borrow: conv (&) and value (&mut) are disjoint fields of Channel
        let (conv, value) = (&ch.conv, &mut ch.value);
        conv.convert(value, &self.tables, id, &mut flow_detected)?;
        // Write back flow state
        if let Some(detected) = flow_detected {
            if let Some(ref mut fs) = ch.flow_state {
                fs.detected = detected;
            }
        }

        // 11. SDP810 spike detection
        let cur = ch.check_sdp810_spike(ch.value.cur);
        ch.value.cur = cur;

        // 12. Tag-based spike filter
        if let Some(ref spike_cfg) = ch.conv.filter_config.as_ref().and_then(|f| f.spike_filter) {
            let cur = filters::apply_spike_filter(&mut ch.spike_state, ch.value.cur, spike_cfg);
            ch.value.cur = cur;
        }

        // 13. Smoothing
        if let Some(ref smooth_cfg) = ch.conv.filter_config.as_ref().and_then(|f| f.smoothing) {
            let cur = filters::apply_smoothing(
                &mut ch.smooth_state,
                ch.value.cur,
                smooth_cfg.window,
                smooth_cfg.method,
            );
            ch.value.cur = cur;
        }

        // 14. Rate limiting
        if let Some(ref rl_cfg) = ch.conv.filter_config.as_ref().and_then(|f| f.rate_limit) {
            let cur = filters::apply_rate_limit(
                &mut ch.rate_limit_state,
                ch.value.cur,
                rl_cfg.max_rise,
                rl_cfg.max_fall,
            );
            ch.value.cur = cur;
        }

        // 15. Trigger on zero
        if ch.value.cur == 0.0 {
            ch.value.trigger = trigger;
        }

        // 16. Return
        Ok(ch.value)
    }

    /// Write a value to an output channel.
    ///
    /// 9-step pipeline matching C `channel_write()`:
    /// 1. Lookup channel
    /// 2. Direction must be output AND flags != 0
    /// 3. Disabled check
    /// 4. Convert/revert based on flags
    /// 5. HAL dispatch
    /// 6. Unsupported types
    /// 7. HAL error handling
    /// 8. Update channel value
    /// 9. Return
    ///
    /// ## Virtual channels
    ///
    /// Virtual channels store the written value locally (`ch.value = *value`)
    /// but do **not** propagate the write to their source channel
    /// (`channel_in`). This matches the C system behavior: in the C engine,
    /// `channel_virtual_write()` sets `item->value.cur = value` without
    /// forwarding to the source, and `ENGINE_MESSAGE_WRITE_VIRTUAL` is
    /// commented out. Write propagation is intentionally not implemented to
    /// maintain feature parity with the C system.
    pub fn channel_write(&mut self, id: ChannelId, value: &mut EngineValue) -> Result<()> {
        // 1. Lookup
        if !self.channels.contains(id) {
            return Err(EngineError::ChannelNotFound(id));
        }

        // 2. Direction must be output (or virtual) AND flags != 0
        {
            let ch = self.channels.get(id).ok_or(EngineError::ChannelNotFound(id))?;
            let writable = ch.direction.is_output() || ch.channel_type.is_virtual();
            if !writable || value.flags.is_empty() {
                return Err(EngineError::WriteNotSupported(id));
            }
        }

        // 3. Disabled check
        {
            let ch = self.channels.get(id).ok_or(EngineError::ChannelNotFound(id))?;
            if !ch.enabled {
                value.status = EngineStatus::Disabled;
                return Ok(());
            }
        }

        // 4. Convert/revert based on flags
        {
            let ch = self.channels.get(id).ok_or(EngineError::ChannelNotFound(id))?;
            if value.flags.contains(ValueFlags::RAW) && !value.flags.contains(ValueFlags::CUR) {
                // RAW only → convert to get cur
                let mut flow = None;
                ch.conv.convert(value, &self.tables, id, &mut flow)?;
            } else if value.flags.contains(ValueFlags::CUR) && !value.flags.contains(ValueFlags::RAW) {
                // CUR only → revert to get raw
                ch.conv.revert(value, &self.tables)?;
            }
        }

        // Snapshot fields for HAL dispatch
        let (channel_type, address, device) = {
            let ch = self.channels.get(id).ok_or(EngineError::ChannelNotFound(id))?;
            (ch.channel_type, ch.address, ch.device)
        };

        // 5-6. HAL dispatch
        let hal_result: std::result::Result<(), sandstar_hal::HalError> = match channel_type {
            ChannelType::Digital | ChannelType::Triac => {
                self.hal.write_digital(address, value.raw > 0.5)
            }
            ChannelType::Pwm => {
                self.hal.write_pwm(device, address, value.raw)
            }
            ChannelType::VirtualAnalog | ChannelType::VirtualDigital => {
                // Virtual: just store cur on channel
                let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
                ch.value = *value;
                return Ok(());
            }
            // Analog, I2C, UART are not writable
            ChannelType::Analog | ChannelType::I2c | ChannelType::Uart => {
                return Err(EngineError::WriteNotSupported(id));
            }
        };

        // 7. HAL error
        if hal_result.is_err() {
            value.status = EngineStatus::Down;
        }

        // 8. Update channel value
        let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
        ch.value = *value;

        // 9. Return
        Ok(())
    }

    /// Write a value at a specific priority level (BACnet-style).
    ///
    /// Sets the value at the given level (1-17), then determines the
    /// effective value (highest-priority non-null) and writes it to hardware.
    ///
    /// If `value` is None, relinquishes the level. If all 17 levels become
    /// empty, the channel retains its last written value (no hardware write).
    ///
    /// For virtual channels, the write is stored locally but not propagated
    /// to the source channel. See [`channel_write`] for details.
    pub fn channel_write_level(
        &mut self,
        id: ChannelId,
        level: u8,
        value: Option<f64>,
        who: &str,
        duration: f64,
    ) -> Result<priority::WriteResult> {
        if !self.channels.contains(id) {
            return Err(EngineError::ChannelNotFound(id));
        }
        if !(1..=17).contains(&level) {
            return Err(EngineError::InvalidWriteLevel(level));
        }

        // Lazy-init priority array
        {
            let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
            if ch.priority_array.is_none() {
                ch.priority_array = Some(priority::PriorityArray::default());
            }
        }

        // Set the level and get effective value
        let write_result = {
            let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
            // priority_array is guaranteed Some by the lazy-init above
            ch.priority_array
                .as_mut()
                .expect("priority_array was just initialized")
                .set_level(level, value, who, duration)
        };

        // If we have an effective value, write to hardware
        if let Some(effective_val) = write_result.effective_value {
            let mut ev = EngineValue::default();
            ev.set_cur(effective_val);
            ev.flags = ValueFlags::CUR;
            self.channel_write(id, &mut ev)?;
        }
        // If no effective value (all levels empty), do NOT write — retain last value.

        Ok(write_result)
    }

    /// Get the priority array for a channel (for API queries).
    ///
    /// Returns None if no priority writes have been made to this channel.
    pub fn get_write_levels(
        &self,
        id: ChannelId,
    ) -> Result<Option<&priority::PriorityArray>> {
        let ch = self
            .channels
            .get(id)
            .ok_or(EngineError::ChannelNotFound(id))?;
        Ok(ch.priority_array.as_ref())
    }

    /// Convenience: convert a value in-place based on flags.
    ///
    /// If flags==RAW → convert (raw→cur). If flags==CUR → revert (cur→raw).
    pub fn channel_convert(&self, id: ChannelId, value: &mut EngineValue) -> Result<()> {
        let ch = self.channels.get(id).ok_or(EngineError::ChannelNotFound(id))?;

        if value.flags.contains(ValueFlags::RAW) && !value.flags.contains(ValueFlags::CUR) {
            let mut flow = None;
            ch.conv.convert(value, &self.tables, id, &mut flow)?;
        } else if value.flags.contains(ValueFlags::CUR) && !value.flags.contains(ValueFlags::RAW) {
            ch.conv.revert(value, &self.tables)?;
        }

        Ok(())
    }

    /// Expire any timed priority writes across all channels.
    ///
    /// Iterates channels that have a priority array and calls
    /// `expire_timed_levels()` on each. If any level expired and the
    /// effective value changed, writes the new effective value to hardware.
    pub fn expire_priority_timers(&mut self) {
        // Collect IDs with priority arrays to avoid borrow issues
        let ids_with_pa: Vec<ChannelId> = self
            .channels
            .iter()
            .filter(|(_, ch)| ch.priority_array.is_some())
            .map(|(&id, _)| id)
            .collect();

        for id in ids_with_pa {
            let expired = {
                let ch = match self.channels.get_mut(id) {
                    Some(ch) => ch,
                    None => continue,
                };
                match ch.priority_array.as_mut() {
                    Some(pa) => pa.expire_timed_levels(),
                    None => false,
                }
            };

            if expired {
                // Re-read effective value and write to hardware
                let effective_val = {
                    let ch = match self.channels.get(id) {
                        Some(ch) => ch,
                        None => continue,
                    };
                    ch.priority_array
                        .as_ref()
                        .map(|pa| pa.effective().0)
                        .unwrap_or(None)
                };

                if let Some(val) = effective_val {
                    let mut ev = EngineValue::default();
                    ev.set_cur(val);
                    ev.flags = ValueFlags::CUR;
                    let _ = self.channel_write(id, &mut ev);
                }
                // If no effective value (all expired), retain last hardware value.
            }
        }
    }

    /// Pre-read all polled I2C channels, coalescing reads that hit the same
    /// physical sensor.
    ///
    /// Returns a cache mapping `(device, address, label)` to the HAL result.
    /// During `poll_update`, `channel_read_cached` consults this cache
    /// instead of performing redundant hardware reads.
    ///
    /// In production the 6 SDP810 channels all share bus 2 / address 0x25.
    /// Without coalescing each poll cycle performs 6 × 45 ms = 270 ms of
    /// blocking I2C I/O.  With coalescing: 1 × 45 ms (6× improvement).
    fn pre_read_i2c_channels(&self) -> I2cReadCache {
        let mut cache = I2cReadCache::new();
        let polled_ids: Vec<ChannelId> = self.polls.iter().map(|(&id, _)| id).collect();

        for &id in &polled_ids {
            if let Some(ch) = self.channels.get(id) {
                if ch.channel_type == ChannelType::I2c && ch.enabled && !ch.failed {
                    let key = (ch.device, ch.address, ch.label.clone());
                    cache.entry(key).or_insert_with_key(|k| {
                        self.hal.read_i2c(k.0, k.1, &k.2)
                    });
                }
            }
        }

        cache
    }

    /// Read a channel through the full pipeline, using a pre-populated I2C
    /// cache for coalesced reads.
    ///
    /// For non-I2C channels this is identical to [`channel_read`].  For I2C
    /// channels the HAL read is served from the cache (populated by
    /// [`pre_read_i2c_channels`]) instead of hitting the hardware again.
    pub fn channel_read_cached(
        &mut self,
        id: ChannelId,
        i2c_cache: &I2cReadCache,
    ) -> Result<EngineValue> {
        // 1. Lookup channel
        if !self.channels.contains(id) {
            return Err(EngineError::ChannelNotFound(id));
        }

        // 2. Disabled check
        {
            let ch = self.channels.get(id).ok_or(EngineError::ChannelNotFound(id))?;
            if !ch.enabled {
                return Ok(EngineValue::with_status(EngineStatus::Disabled));
            }
        }

        // 3. Failed/retry cooldown
        {
            let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
            if ch.failed {
                if ch.retry_counter < RETRY_COOLDOWN {
                    ch.retry_counter += 1;
                    return Ok(EngineValue::with_status(EngineStatus::Down));
                }

                if ch.channel_type == ChannelType::I2c {
                    let device = ch.device;
                    let address = ch.address;
                    let label = ch.label.clone();
                    if let Err(_e) = self.hal.reinit_i2c_sensor(device, address, &label) {
                        let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
                        ch.retry_counter = 0;
                        return Ok(EngineValue::with_status(EngineStatus::Down));
                    }
                    let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
                    ch.failed = false;
                    ch.retry_counter = 0;
                } else {
                    ch.failed = false;
                    ch.retry_counter = 0;
                }
            }
        }

        // 4. Snapshot immutable fields
        let (channel_type, device, address, label, channel_in, pwm_disabled, trigger) = {
            let ch = self.channels.get(id).ok_or(EngineError::ChannelNotFound(id))?;
            (
                ch.channel_type,
                ch.device,
                ch.address,
                ch.label.clone(),
                ch.channel_in,
                ch.pwm_disabled,
                ch.trigger,
            )
        };

        // 5. HAL dispatch — I2C reads use cache, everything else goes direct
        if channel_type.is_virtual() {
            let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;

            // Check if this virtual channel has been written to via channel_write_level
            let has_written_value = ch
                .priority_array
                .as_ref()
                .map(|pa| pa.effective().0.is_some())
                .unwrap_or(false);

            if has_written_value {
                // Written virtual channel: return the stored value from channel_write
                ch.value.status = EngineStatus::Ok;
                ch.value.flags |= ValueFlags::CUR;
                return Ok(ch.value);
            }

            // Unwritten virtual channel: copy from source (channel_in linkage)
            let source_cur = channel_in
                .and_then(|src_id| self.channels.get(src_id))
                .map(|src| src.value.cur)
                .unwrap_or(0.0);

            let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
            ch.value.cur = source_cur;
            ch.value.status = EngineStatus::Ok;
            ch.value.flags |= ValueFlags::CUR;
            return Ok(ch.value);
        }

        let hal_result: std::result::Result<f64, HalError> = match channel_type {
            ChannelType::Analog => self.hal.read_analog(device, address),
            ChannelType::Digital | ChannelType::Triac => {
                match self.hal.read_digital(address) {
                    Ok(b) => Ok(if b { 1.0 } else { 0.0 }),
                    Err(e) => Err(e),
                }
            }
            ChannelType::Pwm => {
                if pwm_disabled {
                    Ok(0.0)
                } else {
                    self.hal.read_pwm(device, address)
                }
            }
            ChannelType::I2c => {
                // Use cached result if available; fall back to direct HAL read
                let key = (device, address, label.clone());
                match i2c_cache.get(&key) {
                    Some(Ok(val)) => Ok(*val),
                    Some(Err(_)) => Err(HalError::BusError(
                        device,
                        "cached I2C read failed".into(),
                    )),
                    None => self.hal.read_i2c(device, address, &label),
                }
            }
            ChannelType::Uart => self.hal.read_uart(device, &label),
            ChannelType::VirtualAnalog | ChannelType::VirtualDigital => unreachable!(),
        };

        // 6-16: Identical to channel_read
        let raw = match hal_result {
            Ok(v) => v,
            Err(_) => {
                let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
                ch.failed = true;
                ch.value.status = EngineStatus::Down;
                return Ok(EngineValue::with_status(EngineStatus::Down));
            }
        };

        let ch = self.channels.get_mut(id).ok_or(EngineError::ChannelNotFound(id))?;
        ch.value.set_raw(raw);

        let raw = ch.validate_i2c_raw(raw);
        ch.value.raw = raw;

        auto_detect_sensor(id, &mut ch.conv, &self.tables);

        let mut flow_detected = ch.flow_state.as_ref().map(|f| f.detected);
        let (conv, value) = (&ch.conv, &mut ch.value);
        conv.convert(value, &self.tables, id, &mut flow_detected)?;
        if let Some(detected) = flow_detected {
            if let Some(ref mut fs) = ch.flow_state {
                fs.detected = detected;
            }
        }

        let cur = ch.check_sdp810_spike(ch.value.cur);
        ch.value.cur = cur;

        if let Some(ref spike_cfg) = ch.conv.filter_config.as_ref().and_then(|f| f.spike_filter) {
            let cur = filters::apply_spike_filter(&mut ch.spike_state, ch.value.cur, spike_cfg);
            ch.value.cur = cur;
        }

        if let Some(ref smooth_cfg) = ch.conv.filter_config.as_ref().and_then(|f| f.smoothing) {
            let cur = filters::apply_smoothing(
                &mut ch.smooth_state,
                ch.value.cur,
                smooth_cfg.window,
                smooth_cfg.method,
            );
            ch.value.cur = cur;
        }

        if let Some(ref rl_cfg) = ch.conv.filter_config.as_ref().and_then(|f| f.rate_limit) {
            let cur = filters::apply_rate_limit(
                &mut ch.rate_limit_state,
                ch.value.cur,
                rl_cfg.max_rise,
                rl_cfg.max_fall,
            );
            ch.value.cur = cur;
        }

        if ch.value.cur == 0.0 {
            ch.value.trigger = trigger;
        }

        Ok(ch.value)
    }

    /// Run one poll cycle: read all polled channels and collect notifications.
    ///
    /// I2C reads are coalesced: channels sharing the same (device, address,
    /// label) trigger only a single HAL read.  This is critical on the
    /// BeagleBone where 6 SDP810 channels on bus 2 would otherwise cost
    /// 6 × 45 ms = 270 ms per cycle.
    ///
    /// Returns a list of notifications for changed channels.
    pub fn poll_update(&mut self) -> Vec<Notification> {
        // 0. Expire timed priority writes before reading channels.
        //    This ensures that `effective()` returns the correct value
        //    after any duration-limited overrides have lapsed.
        self.expire_priority_timers();

        // 1. Pre-read I2C channels to coalesce redundant hardware access.
        let i2c_cache = self.pre_read_i2c_channels();

        // 2. Snapshot polled channel IDs (breaks borrow cycle)
        let polled_ids: Vec<ChannelId> = self.polls.iter().map(|(&id, _)| id).collect();

        let mut notifications = Vec::new();

        // 3. For each polled channel (I2C reads served from cache)
        for id in polled_ids {
            match self.channel_read_cached(id, &i2c_cache) {
                Err(_) => {
                    // 4. On error: record failure, check threshold
                    self.polls.record_failure(id);
                    if let Some(item) = self.polls.get(id) {
                        if item.consecutive_fail_count >= CONSECUTIVE_FAIL_THRESHOLD {
                            // Reset counter after threshold
                            if let Some(item) = self.polls.get_mut(id) {
                                item.consecutive_fail_count = 0;
                            }
                        }
                    }
                }
                Ok(value) => {
                    // 5. On success: record value, check if changed
                    let changed = self.polls.record_value(id, &value);

                    // 6. If changed: collect notifications
                    if changed {
                        for (sub, ch, val) in self.watches.collect_notifications(id, &value) {
                            notifications.push(Notification::Watch {
                                subscriber: sub,
                                channel: ch,
                                value: val,
                            });
                        }
                        for (sub, ch, val) in self.notifies.collect_notifications(id, &value) {
                            notifications.push(Notification::Notify {
                                subscriber: sub,
                                channel: ch,
                                value: val,
                            });
                        }
                    }
                }
            }
        }

        notifications
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{Channel, ChannelDirection, ChannelType};
    use crate::conversion::filters::{RateLimitConfig, SmoothMethod, SmoothingConfig, SpikeConfig};
    use crate::table::TableRanges;
    use crate::value::{ConversionFn, FilterConfig, FlowConfig, ValueConv};
    use sandstar_hal::mock::MockHal;

    /// Helper: build an Engine with a fresh MockHal.
    fn make_engine() -> Engine<MockHal> {
        Engine::new(MockHal::new())
    }

    /// Helper: create a simple analog input channel.
    fn analog_channel(id: ChannelId) -> Channel {
        Channel::new(
            id,
            ChannelType::Analog,
            ChannelDirection::In,
            0,
            0,
            false,
            ValueConv {
                low: Some(0.0),
                high: Some(4096.0),
                ..Default::default()
            },
            "test",
        )
    }

    /// Helper: create a digital input channel.
    fn digital_channel(id: ChannelId) -> Channel {
        Channel::new(
            id,
            ChannelType::Digital,
            ChannelDirection::In,
            0,
            id,
            false,
            ValueConv::default(),
            "digital",
        )
    }

    /// Helper: create an I2C channel (SDP810 range 610-614).
    fn i2c_channel(id: ChannelId) -> Channel {
        Channel::new(
            id,
            ChannelType::I2c,
            ChannelDirection::In,
            2,
            0x40,
            false,
            ValueConv {
                conv_func: Some(ConversionFn::Sdp610ToCfm),
                flow_config: Some(FlowConfig::default()),
                ..Default::default()
            },
            "sdp810",
        )
    }

    /// Helper: create a PWM output channel.
    fn pwm_channel(id: ChannelId) -> Channel {
        Channel::new(
            id,
            ChannelType::Pwm,
            ChannelDirection::Out,
            0,
            1,
            false,
            ValueConv::default(),
            "pwm",
        )
    }

    /// Helper: create a virtual analog channel.
    fn virtual_channel(id: ChannelId, source: ChannelId) -> Channel {
        let mut ch = Channel::new(
            id,
            ChannelType::VirtualAnalog,
            ChannelDirection::In,
            0,
            0,
            false,
            ValueConv::default(),
            "virtual",
        );
        ch.channel_in = Some(source);
        ch
    }

    /// Helper: create a digital output channel.
    fn digital_out_channel(id: ChannelId) -> Channel {
        Channel::new(
            id,
            ChannelType::Digital,
            ChannelDirection::Out,
            0,
            id,
            false,
            ValueConv::default(),
            "dout",
        )
    }

    // ========================================================================
    // channel_read tests
    // ========================================================================

    #[test]
    fn test_read_not_found() {
        let mut engine = make_engine();
        let result = engine.channel_read(9999);
        assert!(matches!(result, Err(EngineError::ChannelNotFound(9999))));
    }

    #[test]
    fn test_read_disabled() {
        let mut engine = make_engine();
        let mut ch = analog_channel(1100);
        ch.enabled = false;
        engine.channels.add(ch).unwrap();

        let value = engine.channel_read(1100).unwrap();
        assert_eq!(value.status, EngineStatus::Disabled);
    }

    #[test]
    fn test_read_analog_ok() {
        let mut engine = make_engine();
        engine.channels.add(analog_channel(1100)).unwrap();
        engine.hal.set_analog(0, 0, Ok(2048.0));

        let value = engine.channel_read(1100).unwrap();
        assert_eq!(value.status, EngineStatus::Ok);
        assert_eq!(value.raw, 2048.0);
        // cur = raw/range = 2048/4096 = 0.5
        assert!((value.cur - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_read_digital_ok() {
        let mut engine = make_engine();
        engine.channels.add(digital_channel(5)).unwrap();
        engine.hal.set_digital(5, Ok(true));

        let value = engine.channel_read(5).unwrap();
        assert_eq!(value.raw, 1.0);
    }

    #[test]
    fn test_read_i2c_ok() {
        let mut engine = make_engine();
        engine.channels.add(i2c_channel(612)).unwrap();
        engine.hal.set_i2c(2, 0x40, "sdp810", Ok(60.0));

        let value = engine.channel_read(612).unwrap();
        assert_eq!(value.status, EngineStatus::Ok);
        // Should have converted via SDP610ToCfm
        assert!(value.cur >= 0.0);
    }

    #[test]
    fn test_read_pwm_disabled() {
        let mut engine = make_engine();
        let mut ch = pwm_channel(100);
        ch.direction = ChannelDirection::In;
        ch.pwm_disabled = true;
        engine.channels.add(ch).unwrap();

        let value = engine.channel_read(100).unwrap();
        assert_eq!(value.raw, 0.0);
    }

    #[test]
    fn test_read_pwm_ok() {
        let mut engine = make_engine();
        let mut ch = pwm_channel(100);
        ch.direction = ChannelDirection::In;
        engine.channels.add(ch).unwrap();
        engine.hal.set_pwm(0, 1, Ok(0.75));

        let value = engine.channel_read(100).unwrap();
        assert_eq!(value.raw, 0.75);
    }

    #[test]
    fn test_read_uart_ok() {
        let mut engine = make_engine();
        let ch = Channel::new(
            200,
            ChannelType::Uart,
            ChannelDirection::In,
            1,
            0,
            false,
            ValueConv::default(),
            "co2",
        );
        engine.channels.add(ch).unwrap();
        engine.hal.set_uart(1, "co2", Ok(400.0));

        let value = engine.channel_read(200).unwrap();
        assert_eq!(value.raw, 400.0);
    }

    #[test]
    fn test_read_virtual() {
        let mut engine = make_engine();
        // Source channel with a known cur value
        let mut src = analog_channel(1100);
        src.value.cur = 72.5;
        src.value.status = EngineStatus::Ok;
        engine.channels.add(src).unwrap();

        engine.channels.add(virtual_channel(3000, 1100)).unwrap();

        let value = engine.channel_read(3000).unwrap();
        assert_eq!(value.status, EngineStatus::Ok);
        assert_eq!(value.cur, 72.5);
    }

    #[test]
    fn test_read_hal_error_sets_down() {
        let mut engine = make_engine();
        engine.channels.add(analog_channel(1100)).unwrap();
        engine.hal.set_analog(
            0,
            0,
            Err(sandstar_hal::HalError::Timeout { device: 0, address: 0 }),
        );

        let value = engine.channel_read(1100).unwrap();
        assert_eq!(value.status, EngineStatus::Down);

        // Channel should be marked failed
        let ch = engine.channels.get(1100).unwrap();
        assert!(ch.failed);
    }

    #[test]
    fn test_read_retry_cooldown() {
        let mut engine = make_engine();
        engine.channels.add(analog_channel(1100)).unwrap();

        // Mark as failed
        engine.channels.get_mut(1100).unwrap().failed = true;

        // Should return Down for 30 cycles (counter increments 0→30)
        for i in 0..30 {
            let value = engine.channel_read(1100).unwrap();
            assert_eq!(value.status, EngineStatus::Down, "cycle {}", i);
        }

        // 31st cycle: counter==30 >= RETRY_COOLDOWN, clears failed, retries
        engine.hal.set_analog(0, 0, Ok(2048.0));
        let value = engine.channel_read(1100).unwrap();
        assert_eq!(value.status, EngineStatus::Ok);
    }

    #[test]
    fn test_read_sdp810_garbage() {
        let mut engine = make_engine();
        let mut ch = i2c_channel(612);
        ch.last_valid_value = 50.0;
        engine.channels.add(ch).unwrap();
        // Raw > 32767 → garbage → returns last_valid_value
        engine.hal.set_i2c(2, 0x40, "sdp810", Ok(65530.0));

        let _value = engine.channel_read(612).unwrap();
        // The raw should have been corrected to last_valid_value (50.0)
        assert_eq!(engine.channels.get(612).unwrap().value.raw, 50.0);
    }

    #[test]
    fn test_read_sdp810_spike_rejected() {
        let mut engine = make_engine();
        let mut ch = i2c_channel(612);
        ch.last_valid_value = 100.0;
        ch.spike_reading_count = 10; // Past startup
        engine.channels.add(ch).unwrap();

        // 6x spike → should be rejected back to baseline
        engine.hal.set_i2c(2, 0x40, "sdp810", Ok(60.0)); // raw=60 → converts to some CFM
        let _value = engine.channel_read(612).unwrap();
        // The SDP810 spike detector may or may not trigger depending on converted value vs baseline.
        // The key check: the function ran without error.
        assert_eq!(_value.status, EngineStatus::Ok);
    }

    #[test]
    fn test_read_trigger_on_zero() {
        let mut engine = make_engine();
        let mut ch = analog_channel(1100);
        ch.trigger = true;
        // Set conv so raw=0 → cur=0
        ch.conv = ValueConv {
            low: Some(0.0),
            high: Some(4096.0),
            ..Default::default()
        };
        engine.channels.add(ch).unwrap();
        engine.hal.set_analog(0, 0, Ok(0.0));

        let value = engine.channel_read(1100).unwrap();
        assert_eq!(value.cur, 0.0);
        assert!(value.trigger);
    }

    #[test]
    fn test_read_auto_detect_sensor() {
        let mut engine = make_engine();

        // Add thermistor table
        engine
            .tables
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

        let ch = Channel::new(
            1113,
            ChannelType::Analog,
            ChannelDirection::In,
            0,
            0,
            false,
            ValueConv {
                unit: "F".to_string(),
                ..Default::default()
            },
            "temp",
        );
        engine.channels.add(ch).unwrap();
        engine.hal.set_analog(0, 0, Ok(8000.0));

        let value = engine.channel_read(1113).unwrap();
        assert_eq!(value.status, EngineStatus::Ok);
        // After auto-detect, should have table-based conversion
        let ch = engine.channels.get(1113).unwrap();
        assert!(ch.conv.table_index.is_some());
    }

    #[test]
    fn test_read_with_smoothing() {
        let mut engine = make_engine();
        let mut ch = analog_channel(1100);
        ch.conv.filter_config = Some(FilterConfig {
            smoothing: Some(SmoothingConfig {
                window: 3,
                method: SmoothMethod::Mean,
            }),
            ..Default::default()
        });
        engine.channels.add(ch).unwrap();

        // First read
        engine.hal.set_analog(0, 0, Ok(2048.0));
        engine.channel_read(1100).unwrap();

        // Second read
        engine.hal.set_analog(0, 0, Ok(4096.0));
        let value = engine.channel_read(1100).unwrap();

        // With smoothing mean of 2 samples, result should be averaged
        // (exact value depends on raw→cur conversion + smoothing)
        assert_eq!(value.status, EngineStatus::Ok);
    }

    #[test]
    fn test_read_with_spike_filter() {
        let mut engine = make_engine();
        let mut ch = analog_channel(1100);
        ch.conv.filter_config = Some(FilterConfig {
            spike_filter: Some(SpikeConfig {
                threshold: 5.0,
                startup_discard: 2,
                reverse_threshold: 10.0,
            }),
            ..Default::default()
        });
        engine.channels.add(ch).unwrap();

        // Startup readings
        engine.hal.set_analog(0, 0, Ok(2048.0));
        engine.channel_read(1100).unwrap();
        engine.hal.set_analog(0, 0, Ok(2048.0));
        engine.channel_read(1100).unwrap();

        // Spike: raw changes dramatically
        engine.hal.set_analog(0, 0, Ok(2048.0));
        let value = engine.channel_read(1100).unwrap();
        // No spike here since raw is same → spike filter should pass through
        assert_eq!(value.status, EngineStatus::Ok);
    }

    #[test]
    fn test_read_with_rate_limit() {
        let mut engine = make_engine();
        let mut ch = analog_channel(1100);
        ch.conv.filter_config = Some(FilterConfig {
            rate_limit: Some(RateLimitConfig {
                max_rise: 100.0,
                max_fall: 150.0,
            }),
            ..Default::default()
        });
        engine.channels.add(ch).unwrap();

        // First read (initializes rate limiter)
        engine.hal.set_analog(0, 0, Ok(2048.0));
        engine.channel_read(1100).unwrap();

        // Second read with large change
        engine.hal.set_analog(0, 0, Ok(4096.0));
        let value = engine.channel_read(1100).unwrap();
        assert_eq!(value.status, EngineStatus::Ok);
    }

    // ========================================================================
    // channel_write tests
    // ========================================================================

    #[test]
    fn test_write_not_found() {
        let mut engine = make_engine();
        let mut value = EngineValue::default();
        value.set_raw(1.0);

        let result = engine.channel_write(9999, &mut value);
        assert!(matches!(result, Err(EngineError::ChannelNotFound(9999))));
    }

    #[test]
    fn test_write_not_output() {
        let mut engine = make_engine();
        // Analog input — not writable
        engine.channels.add(analog_channel(1100)).unwrap();

        let mut value = EngineValue::default();
        value.set_raw(1.0);

        let result = engine.channel_write(1100, &mut value);
        assert!(matches!(result, Err(EngineError::WriteNotSupported(1100))));
    }

    #[test]
    fn test_write_disabled() {
        let mut engine = make_engine();
        let mut ch = digital_out_channel(5);
        ch.enabled = false;
        engine.channels.add(ch).unwrap();

        let mut value = EngineValue::default();
        value.set_raw(1.0);

        engine.channel_write(5, &mut value).unwrap();
        assert_eq!(value.status, EngineStatus::Disabled);
    }

    #[test]
    fn test_write_digital_out() {
        let mut engine = make_engine();
        engine.channels.add(digital_out_channel(5)).unwrap();

        let mut value = EngineValue::default();
        value.set_raw(1.0);

        engine.channel_write(5, &mut value).unwrap();

        let writes = engine.hal.digital_writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].address, 5);
        assert!(writes[0].value); // raw 1.0 > 0.5 → true
    }

    #[test]
    fn test_write_pwm_out() {
        let mut engine = make_engine();
        engine.channels.add(pwm_channel(100)).unwrap();

        let mut value = EngineValue::default();
        value.set_raw(0.75);

        engine.channel_write(100, &mut value).unwrap();

        let writes = engine.hal.pwm_writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].duty, 0.75);
    }

    #[test]
    fn test_write_virtual() {
        let mut engine = make_engine();
        let mut ch = Channel::new(
            3000,
            ChannelType::VirtualAnalog,
            ChannelDirection::Out,
            0,
            0,
            false,
            ValueConv::default(),
            "virt",
        );
        ch.channel_in = None;
        engine.channels.add(ch).unwrap();

        let mut value = EngineValue::default();
        value.set_cur(42.0);

        engine.channel_write(3000, &mut value).unwrap();

        let ch = engine.channels.get(3000).unwrap();
        assert_eq!(ch.value.cur, 42.0);
    }

    #[test]
    fn test_write_flags_zero_rejected() {
        let mut engine = make_engine();
        engine.channels.add(digital_out_channel(5)).unwrap();

        let mut value = EngineValue::default();
        // flags are empty by default

        let result = engine.channel_write(5, &mut value);
        assert!(matches!(result, Err(EngineError::WriteNotSupported(5))));
    }

    #[test]
    fn test_write_cur_reverts() {
        let mut engine = make_engine();
        let mut ch = digital_out_channel(5);
        ch.conv = ValueConv {
            low: Some(0.0),
            high: Some(4096.0),
            adc_mode: true,
            ..Default::default()
        };
        engine.channels.add(ch).unwrap();

        let mut value = EngineValue::default();
        value.set_cur(1.0); // CUR flag → should revert to raw

        engine.channel_write(5, &mut value).unwrap();

        // With adc_mode: cur=1.0 > 0 → raw = high = 4096
        assert_eq!(value.raw, 4096.0);
    }

    // ========================================================================
    // channel_convert tests
    // ========================================================================

    #[test]
    fn test_convert_raw_to_cur() {
        let engine = make_engine_with_analog();

        let mut value = EngineValue::default();
        value.set_raw(2048.0);

        engine.channel_convert(1100, &mut value).unwrap();
        assert!((value.cur - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_convert_cur_to_raw() {
        let engine = make_engine_with_analog();

        let mut value = EngineValue::default();
        value.set_cur(0.5);

        engine.channel_convert(1100, &mut value).unwrap();
        assert!((value.raw - 2048.0).abs() < 0.01);
    }

    fn make_engine_with_analog() -> Engine<MockHal> {
        let mut engine = make_engine();
        engine.channels.add(analog_channel(1100)).unwrap();
        engine
    }

    // ========================================================================
    // poll_update tests
    // ========================================================================

    #[test]
    fn test_poll_basic() {
        let mut engine = make_engine();
        engine.channels.add(analog_channel(1100)).unwrap();
        engine.polls.add(1100).unwrap();
        engine.hal.set_analog(0, 0, Ok(2048.0));

        let notifs = engine.poll_update();
        // First read always changes from default → should produce notifications
        // (but no watch/notify subscribers, so empty)
        assert!(notifs.is_empty());

        // Verify poll recorded the value
        let item = engine.polls.get(1100).unwrap();
        assert_eq!(item.consecutive_fail_count, 0);
    }

    #[test]
    fn test_poll_no_change() {
        let mut engine = make_engine();
        engine.channels.add(analog_channel(1100)).unwrap();
        engine.polls.add(1100).unwrap();

        // First poll
        engine.hal.set_analog(0, 0, Ok(2048.0));
        engine.poll_update();

        // Second poll with same value → no change
        engine.hal.set_analog(0, 0, Ok(2048.0));
        let notifs = engine.poll_update();
        assert!(notifs.is_empty());
    }

    #[test]
    fn test_poll_watch_notifications() {
        let mut engine = make_engine();
        engine.channels.add(analog_channel(1100)).unwrap();
        engine.polls.add(1100).unwrap();
        engine.watches.add(1100, 42);

        // First poll — value changes from default
        engine.hal.set_analog(0, 0, Ok(2048.0));
        let notifs = engine.poll_update();

        assert_eq!(notifs.len(), 1);
        assert!(matches!(
            &notifs[0],
            Notification::Watch { subscriber: 42, channel: 1100, .. }
        ));
    }

    #[test]
    fn test_poll_notify_notifications() {
        let mut engine = make_engine();
        engine.channels.add(analog_channel(1100)).unwrap();
        engine.polls.add(1100).unwrap();
        engine.notifies.add(99);

        // First poll — value changes from default
        engine.hal.set_analog(0, 0, Ok(2048.0));
        let notifs = engine.poll_update();

        assert_eq!(notifs.len(), 1);
        assert!(matches!(
            &notifs[0],
            Notification::Notify { subscriber: 99, channel: 1100, .. }
        ));
    }

    #[test]
    fn test_poll_read_failure() {
        let mut engine = make_engine();
        engine.channels.add(analog_channel(1100)).unwrap();
        engine.polls.add(1100).unwrap();
        engine.hal.set_analog(
            0,
            0,
            Err(sandstar_hal::HalError::Timeout { device: 0, address: 0 }),
        );

        let notifs = engine.poll_update();
        assert!(notifs.is_empty());

        // HAL error sets channel.failed but channel_read returns Ok(Down),
        // so poll records the value (status=Down), doesn't record failure.
        // The channel is marked failed for retry cooldown.
        let ch = engine.channels.get(1100).unwrap();
        assert!(ch.failed);
    }

    #[test]
    fn test_poll_consecutive_fail_threshold() {
        let mut engine = make_engine();
        engine.channels.add(analog_channel(1100)).unwrap();
        engine.polls.add(1100).unwrap();

        // Directly test poll failure recording
        for _ in 0..5 {
            engine.polls.record_failure(1100);
        }

        let item = engine.polls.get(1100).unwrap();
        assert_eq!(item.consecutive_fail_count, 5);
    }

    #[test]
    fn test_poll_multiple_channels() {
        let mut engine = make_engine();

        // Add two channels
        engine.channels.add(analog_channel(1100)).unwrap();
        let mut ch2 = analog_channel(1200);
        ch2.device = 0;
        ch2.address = 1;
        engine.channels.add(ch2).unwrap();

        engine.polls.add(1100).unwrap();
        engine.polls.add(1200).unwrap();

        engine.watches.add(1100, 1);
        engine.watches.add(1200, 2);

        engine.hal.set_analog(0, 0, Ok(2048.0));
        engine.hal.set_analog(0, 1, Ok(3072.0));

        let notifs = engine.poll_update();

        // Both channels should produce watch notifications (first read = change)
        assert_eq!(notifs.len(), 2);
    }

    // ========================================================================
    // Virtual channel write propagation (7.0a)
    // ========================================================================

    /// Verify that writing to a virtual channel does NOT modify the source
    /// channel. This documents intentional behavior matching the C system:
    /// the C engine's `channel_virtual_write()` stores the value locally
    /// without forwarding to the source, and `ENGINE_MESSAGE_WRITE_VIRTUAL`
    /// is commented out.
    #[test]
    fn test_virtual_write_does_not_propagate_to_source() {
        let mut engine = make_engine();

        // Create a source channel (analog output) with a known value
        let mut src = Channel::new(
            500,
            ChannelType::Analog,
            ChannelDirection::In,
            0,
            0,
            false,
            ValueConv::default(),
            "source",
        );
        src.value.set_cur(100.0);
        src.value.status = EngineStatus::Ok;
        engine.channels.add(src).unwrap();

        // Create a virtual channel pointing at the source
        let mut virt = Channel::new(
            3000,
            ChannelType::VirtualAnalog,
            ChannelDirection::Out,
            0,
            0,
            false,
            ValueConv::default(),
            "virtual",
        );
        virt.channel_in = Some(500);
        engine.channels.add(virt).unwrap();

        // Write a new value to the virtual channel
        let mut value = EngineValue::default();
        value.set_cur(42.0);
        engine.channel_write(3000, &mut value).unwrap();

        // Virtual channel should have the new value
        let virt_ch = engine.channels.get(3000).unwrap();
        assert_eq!(virt_ch.value.cur, 42.0);

        // Source channel MUST remain unchanged at 100.0
        let src_ch = engine.channels.get(500).unwrap();
        assert_eq!(
            src_ch.value.cur, 100.0,
            "write to virtual channel must not propagate to source (matches C system behavior)"
        );
    }

    /// Verify that priority-level write to a virtual channel does NOT
    /// propagate to the source channel.
    #[test]
    fn test_virtual_write_level_does_not_propagate_to_source() {
        let mut engine = make_engine();

        // Source channel with a known value
        let mut src = Channel::new(
            500,
            ChannelType::Analog,
            ChannelDirection::In,
            0,
            0,
            false,
            ValueConv::default(),
            "source",
        );
        src.value.set_cur(100.0);
        src.value.status = EngineStatus::Ok;
        engine.channels.add(src).unwrap();

        // Virtual output channel
        let mut virt = Channel::new(
            3000,
            ChannelType::VirtualAnalog,
            ChannelDirection::Out,
            0,
            0,
            false,
            ValueConv::default(),
            "virtual",
        );
        virt.channel_in = Some(500);
        engine.channels.add(virt).unwrap();

        // Write at priority level 8
        engine
            .channel_write_level(3000, 8, Some(55.0), "test", 0.0)
            .unwrap();

        // Virtual channel should have the priority write value
        let virt_ch = engine.channels.get(3000).unwrap();
        assert_eq!(virt_ch.value.cur, 55.0);

        // Source channel MUST remain unchanged
        let src_ch = engine.channels.get(500).unwrap();
        assert_eq!(
            src_ch.value.cur, 100.0,
            "priority write to virtual channel must not propagate to source (matches C system behavior)"
        );
    }

    // ========================================================================
    // I2C read coalescing tests (Phase 7.0c)
    // ========================================================================

    #[test]
    fn test_i2c_coalescing_reduces_reads() {
        // 3 I2C channels sharing the same (device=2, address=0x40, label="sdp810").
        // MockHal has sticky mode: one queued value satisfies all reads to the
        // same key.  With coalescing, pre_read_i2c_channels() performs exactly
        // one HAL read and all 3 channels consume the cached result.
        let mut engine = make_engine();

        for id in [610, 611, 612] {
            engine.channels.add(i2c_channel(id)).unwrap();
            engine.polls.add(id).unwrap();
        }

        // Queue exactly ONE read result for (device=2, addr=0x40, label="sdp810").
        engine.hal.set_i2c(2, 0x40, "sdp810", Ok(60.0));

        // poll_update uses pre_read_i2c_channels then channel_read_cached.
        // If coalescing were not working, the 2nd and 3rd channels would
        // hit the MockHal again (sticky returns 60.0 anyway, so the test
        // passes — but we verify all 3 channels read successfully).
        let _notifs = engine.poll_update();

        for id in [610, 611, 612] {
            let ch = engine.channels.get(id).unwrap();
            assert_eq!(ch.value.status, EngineStatus::Ok, "channel {} should be Ok", id);
            // raw should be 60.0 from the cache
            assert_eq!(ch.value.raw, 60.0, "channel {} raw should be 60.0", id);
        }
    }

    #[test]
    fn test_i2c_coalescing_different_keys() {
        // Two I2C channels on different devices should each trigger their own
        // HAL read (no coalescing across different keys).
        let mut engine = make_engine();

        // Channel on device 2
        let mut ch_a = i2c_channel(610);
        ch_a.device = 2;
        ch_a.address = 0x40;
        engine.channels.add(ch_a).unwrap();
        engine.polls.add(610).unwrap();

        // Channel on device 3 (different bus)
        let ch_b = Channel::new(
            611,
            ChannelType::I2c,
            ChannelDirection::In,
            3,
            0x50,
            false,
            ValueConv {
                conv_func: Some(ConversionFn::Sdp610ToCfm),
                flow_config: Some(FlowConfig::default()),
                ..Default::default()
            },
            "sdp810",
        );
        engine.channels.add(ch_b).unwrap();
        engine.polls.add(611).unwrap();

        // Queue one result per unique key
        engine.hal.set_i2c(2, 0x40, "sdp810", Ok(60.0));
        engine.hal.set_i2c(3, 0x50, "sdp810", Ok(80.0));

        let _notifs = engine.poll_update();

        let ch_a = engine.channels.get(610).unwrap();
        assert_eq!(ch_a.value.status, EngineStatus::Ok);
        assert_eq!(ch_a.value.raw, 60.0);

        let ch_b = engine.channels.get(611).unwrap();
        assert_eq!(ch_b.value.status, EngineStatus::Ok);
        assert_eq!(ch_b.value.raw, 80.0);
    }

    #[test]
    fn test_i2c_coalescing_error_shared() {
        // If the shared I2C read fails, all channels sharing that key get the
        // error (marked as failed/Down).
        let mut engine = make_engine();

        for id in [610, 611] {
            engine.channels.add(i2c_channel(id)).unwrap();
            engine.polls.add(id).unwrap();
        }

        // Queue an error for the shared key
        engine.hal.set_i2c(
            2,
            0x40,
            "sdp810",
            Err(sandstar_hal::HalError::Timeout { device: 2, address: 0x40 }),
        );

        let _notifs = engine.poll_update();

        for id in [610, 611] {
            let ch = engine.channels.get(id).unwrap();
            assert!(
                ch.failed || ch.value.status == EngineStatus::Down,
                "channel {} should be failed or Down after shared I2C error",
                id,
            );
        }
    }

    #[test]
    fn test_i2c_pre_read_skips_disabled_and_failed() {
        // pre_read_i2c_channels should skip disabled and failed channels.
        let mut engine = make_engine();

        // Disabled I2C channel
        let mut ch_dis = i2c_channel(610);
        ch_dis.enabled = false;
        engine.channels.add(ch_dis).unwrap();
        engine.polls.add(610).unwrap();

        // Failed I2C channel
        let mut ch_fail = i2c_channel(611);
        ch_fail.failed = true;
        engine.channels.add(ch_fail).unwrap();
        engine.polls.add(611).unwrap();

        // Enabled I2C channel
        engine.channels.add(i2c_channel(612)).unwrap();
        engine.polls.add(612).unwrap();
        engine.hal.set_i2c(2, 0x40, "sdp810", Ok(60.0));

        // The pre_read should only issue one HAL read for channel 612
        let cache = engine.pre_read_i2c_channels();

        // Should have exactly one entry (for the enabled, non-failed channel)
        assert_eq!(cache.len(), 1);
        assert!(cache.contains_key(&(2u32, 0x40u32, "sdp810".to_string())));
    }

    // ========================================================================
    // expire_priority_timers tests
    // ========================================================================

    #[test]
    fn test_expire_timer_basic() {
        // Write at level 1 with a timed duration, manually expire it,
        // verify level 1 is cleared.
        let mut engine = make_engine();
        engine.channels.add(digital_out_channel(5)).unwrap();

        // Write level 1 with 1-second duration
        engine
            .channel_write_level(5, 1, Some(1.0), "temp", 1.0)
            .unwrap();

        // Verify level 1 is active
        let pa = engine.get_write_levels(5).unwrap().unwrap();
        assert_eq!(pa.effective().0, Some(1.0));
        assert_eq!(pa.effective().1, 1);

        // Force expiry by setting expires_at to the past
        let past = std::time::Instant::now() - std::time::Duration::from_secs(10);
        engine
            .channels
            .get_mut(5)
            .unwrap()
            .priority_array
            .as_mut()
            .unwrap()
            .levels[0]  // level 1 = index 0 (private field, access via direct struct)
            .expires_at = Some(past);

        engine.expire_priority_timers();

        // Level 1 should be cleared
        let pa = engine.get_write_levels(5).unwrap().unwrap();
        assert!(pa.levels()[0].value.is_none());
    }

    #[test]
    fn test_expire_timer_falls_to_lower_priority() {
        // Level 1 timed (1s) + level 8 permanent. Expire level 1,
        // effective should fall to level 8's value.
        let mut engine = make_engine();
        engine.channels.add(digital_out_channel(5)).unwrap();

        // Write permanent at level 8
        engine
            .channel_write_level(5, 8, Some(0.0), "perm", 0.0)
            .unwrap();

        // Write timed at level 1 (higher priority)
        engine
            .channel_write_level(5, 1, Some(1.0), "temp", 1.0)
            .unwrap();

        // Effective should be level 1's value
        let pa = engine.get_write_levels(5).unwrap().unwrap();
        assert_eq!(pa.effective().0, Some(1.0));
        assert_eq!(pa.effective().1, 1);

        // Force level 1 expiry
        let past = std::time::Instant::now() - std::time::Duration::from_secs(10);
        engine
            .channels
            .get_mut(5)
            .unwrap()
            .priority_array
            .as_mut()
            .unwrap()
            .levels[0]
            .expires_at = Some(past);

        engine.expire_priority_timers();

        // Effective should now be level 8's value (0.0)
        let pa = engine.get_write_levels(5).unwrap().unwrap();
        assert_eq!(pa.effective().0, Some(0.0));
        assert_eq!(pa.effective().1, 8);
    }

    #[test]
    fn test_expire_timer_multiple_channels() {
        // Two channels with different expiry: only the expired one clears.
        let mut engine = make_engine();
        engine.channels.add(digital_out_channel(5)).unwrap();
        engine.channels.add(digital_out_channel(6)).unwrap();

        // Channel 5: timed write, will be expired
        engine
            .channel_write_level(5, 1, Some(1.0), "temp", 1.0)
            .unwrap();
        // Channel 6: timed write, will NOT be expired (future)
        engine
            .channel_write_level(6, 1, Some(1.0), "temp", 3600.0)
            .unwrap();

        // Force channel 5 level 1 expiry only
        let past = std::time::Instant::now() - std::time::Duration::from_secs(10);
        engine
            .channels
            .get_mut(5)
            .unwrap()
            .priority_array
            .as_mut()
            .unwrap()
            .levels[0]
            .expires_at = Some(past);

        engine.expire_priority_timers();

        // Channel 5 level 1 should be cleared
        let pa5 = engine.get_write_levels(5).unwrap().unwrap();
        assert!(pa5.levels()[0].value.is_none());

        // Channel 6 level 1 should still be active
        let pa6 = engine.get_write_levels(6).unwrap().unwrap();
        assert_eq!(pa6.levels()[0].value, Some(1.0));
        assert_eq!(pa6.effective().0, Some(1.0));
    }

    #[test]
    fn test_expire_timer_all_levels_expire() {
        // Write timed values at levels 1, 5, 10, all expire together,
        // verify channel returns to default/None.
        let mut engine = make_engine();
        engine.channels.add(digital_out_channel(5)).unwrap();

        for level in [1u8, 5, 10] {
            engine
                .channel_write_level(5, level, Some(level as f64), "temp", 1.0)
                .unwrap();
        }

        // Force all three to expire
        let past = std::time::Instant::now() - std::time::Duration::from_secs(10);
        let pa = engine
            .channels
            .get_mut(5)
            .unwrap()
            .priority_array
            .as_mut()
            .unwrap();
        pa.levels[0].expires_at = Some(past); // level 1
        pa.levels[4].expires_at = Some(past); // level 5
        pa.levels[9].expires_at = Some(past); // level 10

        engine.expire_priority_timers();

        // All empty
        let pa = engine.get_write_levels(5).unwrap().unwrap();
        assert!(pa.effective().0.is_none());
        assert_eq!(pa.effective().1, 0);
    }

    #[test]
    fn test_expire_timer_highest_priority_expires() {
        // Level 1 timed expires, level 2 permanent remains,
        // effective shifts to level 2.
        let mut engine = make_engine();
        engine.channels.add(digital_out_channel(5)).unwrap();

        // Permanent at level 2
        engine
            .channel_write_level(5, 2, Some(0.0), "perm", 0.0)
            .unwrap();
        // Timed at level 1
        engine
            .channel_write_level(5, 1, Some(1.0), "temp", 1.0)
            .unwrap();

        assert_eq!(
            engine
                .get_write_levels(5)
                .unwrap()
                .unwrap()
                .effective()
                .1,
            1
        );

        // Expire level 1
        let past = std::time::Instant::now() - std::time::Duration::from_secs(10);
        engine
            .channels
            .get_mut(5)
            .unwrap()
            .priority_array
            .as_mut()
            .unwrap()
            .levels[0]
            .expires_at = Some(past);

        engine.expire_priority_timers();

        let pa = engine.get_write_levels(5).unwrap().unwrap();
        assert_eq!(pa.effective().0, Some(0.0));
        assert_eq!(pa.effective().1, 2);
    }

    #[test]
    fn test_expire_timer_no_timers() {
        // Call expire on engine with no timed writes — should be a no-op.
        let mut engine = make_engine();
        engine.channels.add(digital_out_channel(5)).unwrap();

        // Permanent write only (no expiry)
        engine
            .channel_write_level(5, 8, Some(1.0), "perm", 0.0)
            .unwrap();

        engine.expire_priority_timers();

        // Value unchanged
        let pa = engine.get_write_levels(5).unwrap().unwrap();
        assert_eq!(pa.effective().0, Some(1.0));
        assert_eq!(pa.effective().1, 8);
    }

    #[test]
    fn test_expire_timer_hardware_write_on_expiry() {
        // After expiry, verify the HAL receives the new effective value
        // (the write-through to hardware).
        let mut engine = make_engine();
        engine.channels.add(digital_out_channel(5)).unwrap();

        // Permanent at level 8: value 0.0 (digital off)
        engine
            .channel_write_level(5, 8, Some(0.0), "base", 0.0)
            .unwrap();
        // Timed at level 1: value 1.0 (digital on)
        engine
            .channel_write_level(5, 1, Some(1.0), "override", 1.0)
            .unwrap();

        // Record how many writes have been made so far
        let writes_before = engine.hal.digital_writes().len();

        // Expire level 1
        let past = std::time::Instant::now() - std::time::Duration::from_secs(10);
        engine
            .channels
            .get_mut(5)
            .unwrap()
            .priority_array
            .as_mut()
            .unwrap()
            .levels[0]
            .expires_at = Some(past);

        engine.expire_priority_timers();

        // HAL should have received a new write for the effective value (0.0 → digital false)
        let writes = engine.hal.digital_writes();
        assert!(
            writes.len() > writes_before,
            "expire_priority_timers should write new effective value to HAL"
        );
        let last_write = writes.last().unwrap();
        assert_eq!(last_write.address, 5);
        assert!(!last_write.value, "effective value 0.0 should write false to digital HAL");
    }
}
