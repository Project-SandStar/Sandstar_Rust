//! Kit 4 (EacIo) native methods — bridge-aware implementations.
//!
//! Replaces the inline kit 4 stubs in `native_table.rs` with implementations
//! that read from the [`ChannelSnapshot`] and queue writes through the engine
//! bridge.  When the bridge is not initialized (e.g., in unit tests or
//! standalone VM mode), the methods return safe defaults identical to the
//! previous stubs.
//!
//! # Architecture
//!
//! ```text
//! Sedona scode → NativeTable dispatch → native_eacio.rs → bridge.rs → Engine
//! ```
//!
//! The bridge uses lock-free reads (RwLock) for the hot path (point gets)
//! and a mutex-protected queue for writes, minimizing contention with the
//! engine's poll loop.
//!
//! # Method ID mapping (from nativetable.c kit 4)
//!
//! | ID | Method                           | Return | Direction |
//! |----|----------------------------------|--------|-----------|
//! |  0 | (reserved / NULL in C)           | —      | —         |
//! |  1 | boolInPoint.get(channel)         | bool   | read      |
//! |  2 | boolOutPoint.set(channel, val)   | bool   | write     |
//! |  3 | binaryValuePoint.set(channel,val)| bool   | write     |
//! |  4 | analogInPoint.get(channel)       | float  | read      |
//! |  5 | analogOutPoint.set(channel, val) | bool   | write     |
//! |  6 | analogValuePoint.set(channel,val)| bool   | write     |
//! |  7 | eacio.resolveChannel(markers)    | int    | read      |
//! |  8 | eacio.getRecordCount(markers)    | int    | read      |
//! |  9 | eacio.getCurStatus(ch, buf)      | bool   | read      |
//! | 10 | eacio.getChannelName(ch, buf)    | bool   | read      |
//! | 11 | triacPoint.set(channel, val)     | bool   | write     |
//! | 12 | eacio.writeSedonaId(ch, id)      | int    | write     |
//! | 13 | eacio.writeSedonaType(ch,t1,t2)  | int    | write     |
//! | 14 | eacio.isChannelEnabled(ch)       | bool   | read      |
//! | 15 | eacio.getBoolTagValue(ch, tag)   | bool   | read      |
//! | 16 | eacio.getNumberTagValue(ch, tag) | float  | read      |
//! | 17 | eacio.getStringTagValue(ch,t,buf)| void   | read      |
//! | 18 | eacio.getTagType(ch, tag)        | int    | read      |
//! | 19 | eacio.getLevel(ch)               | int    | read      |
//! | 20 | eacio.getLevelValue(ch, level)    | float  | read      |
//! | 21 | eacio.getChannelIn(ch)           | int    | read      |
//! | 22 | analogValuePoint.get(ch)         | float  | read      |

use crate::native_table::{NativeContext, NativeTable};
use crate::vm_error::VmResult;

/// EacIo kit ID (matches nativetable.c).
pub const EACIO_KIT_ID: u8 = 4;

/// Number of native methods in the EacIo kit (0..22 inclusive).
pub const EACIO_METHOD_COUNT: u16 = 23;

// ════════════════════════════════════════════════════════════════
// Bridge helpers (read from ChannelSnapshot, queue writes)
// ════════════════════════════════════════════════════════════════

/// Read a channel's current float value from the bridge snapshot.
/// Returns `None` if the bridge is not initialized or channel not found.
fn bridge_read_float(channel: u32) -> Option<f64> {
    let snapshot = crate::bridge::ENGINE_BRIDGE.get()?;
    let guard = snapshot.read().ok()?;
    guard.get_cur(channel)
}

/// Read a channel's boolean status (status_ok) from the bridge snapshot.
fn bridge_read_bool(channel: u32) -> Option<bool> {
    let snapshot = crate::bridge::ENGINE_BRIDGE.get()?;
    let guard = snapshot.read().ok()?;
    guard.get(channel).map(|ch| ch.status_ok)
}

/// Check if a channel is enabled via the bridge snapshot.
fn bridge_is_enabled(channel: u32) -> Option<bool> {
    let snapshot = crate::bridge::ENGINE_BRIDGE.get()?;
    let guard = snapshot.read().ok()?;
    Some(guard.is_enabled(channel))
}

/// Get a channel's label from the bridge snapshot.
fn bridge_get_label(channel: u32) -> Option<String> {
    let snapshot = crate::bridge::ENGINE_BRIDGE.get()?;
    let guard = snapshot.read().ok()?;
    guard.get(channel).map(|ch| ch.label.clone())
}

/// Get a channel's virtual input source from the bridge snapshot.
fn bridge_get_channel_in(channel: u32) -> Option<i32> {
    let snapshot = crate::bridge::ENGINE_BRIDGE.get()?;
    let guard = snapshot.read().ok()?;
    guard.get(channel).map(|ch| ch.channel_in)
}

/// Get a channel's active write priority level from the bridge snapshot.
fn bridge_get_level(channel: u32) -> Option<u8> {
    let snapshot = crate::bridge::ENGINE_BRIDGE.get()?;
    let guard = snapshot.read().ok()?;
    guard.get(channel).map(|ch| ch.write_level)
}

/// Get a channel's write level value from the bridge snapshot.
fn bridge_get_level_value(channel: u32, level: usize) -> Option<f64> {
    let snapshot = crate::bridge::ENGINE_BRIDGE.get()?;
    let guard = snapshot.read().ok()?;
    guard
        .get(channel)
        .and_then(|ch| {
            if level >= 1 && level <= 17 {
                ch.write_levels[level - 1]
            } else {
                None
            }
        })
}

/// Get a tag value for a channel from the bridge snapshot.
#[allow(dead_code)] // Phase B: used when Str parameter reading is implemented
fn bridge_get_tag(channel: u32, _tag_name: &str) -> Option<crate::bridge::TagValue> {
    let snapshot = crate::bridge::ENGINE_BRIDGE.get()?;
    let guard = snapshot.read().ok()?;
    guard
        .get(channel)
        .and_then(|ch| ch.tags.get(_tag_name).cloned())
}

/// Resolve a channel by marker string via the bridge snapshot.
#[allow(dead_code)] // Phase B: used when Str parameter reading is implemented
fn bridge_resolve_channel(markers: &str) -> Option<u32> {
    let snapshot = crate::bridge::ENGINE_BRIDGE.get()?;
    let guard = snapshot.read().ok()?;
    guard.resolve_channel(markers)
}

/// Count channels matching markers via the bridge snapshot.
#[allow(dead_code)] // Phase B: used when Str parameter reading is implemented
fn bridge_count_matching(markers: &str) -> usize {
    crate::bridge::ENGINE_BRIDGE
        .get()
        .and_then(|s| s.read().ok())
        .map(|g| g.count_matching(markers))
        .unwrap_or(0)
}

/// Queue a write to the engine (float value to channel).
fn bridge_queue_write(channel: u32, value: f64) {
    if let Some(q) = crate::bridge::WRITE_QUEUE.get() {
        if let Ok(mut v) = q.lock() {
            v.push(crate::bridge::SvmWrite { channel, value });
        }
    }
}

/// Queue a tag write to the engine.
fn bridge_queue_tag_write(channel: u32, tag: String, value: String) {
    if let Some(q) = crate::bridge::TAG_WRITE_QUEUE.get() {
        if let Ok(mut v) = q.lock() {
            v.push(crate::bridge::SvmTagWrite { channel, tag, value });
        }
    }
}

// ════════════════════════════════════════════════════════════════
// Native method implementations
// ════════════════════════════════════════════════════════════════

/// `boolInPoint.get(channel) -> bool` (4::1)
///
/// Reads a boolean input point.  Returns the channel's status_ok flag,
/// or false (0) if the bridge is not available.
pub fn eacio_bool_in_point_get(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    let val = bridge_read_bool(channel).unwrap_or(false);
    Ok(val as i32)
}

/// `boolOutPoint.set(channel, value) -> bool` (4::2)
///
/// Writes a boolean output point.  Queues a write (1.0 or 0.0) to the engine.
pub fn eacio_bool_out_point_set(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    let value = params.get(1).copied().unwrap_or(0);
    bridge_queue_write(channel, if value != 0 { 1.0 } else { 0.0 });
    Ok(1) // success
}

/// `binaryValuePoint.set(channel, value) -> bool` (4::3)
///
/// Writes a binary value point (same semantics as bool output).
pub fn eacio_binary_value_point_set(
    _ctx: &mut NativeContext<'_>,
    params: &[i32],
) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    let value = params.get(1).copied().unwrap_or(0);
    bridge_queue_write(channel, if value != 0 { 1.0 } else { 0.0 });
    Ok(1)
}

/// `analogInPoint.get(channel) -> float` (4::4)
///
/// Reads an analog input point.  Returns the channel's current value
/// as float bits, or 0.0f if the bridge is not available.
pub fn eacio_analog_in_point_get(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    let val = bridge_read_float(channel).unwrap_or(0.0) as f32;
    Ok(val.to_bits() as i32)
}

/// `analogOutPoint.set(channel, value) -> bool` (4::5)
///
/// Writes an analog output point.  The value parameter is float bits.
pub fn eacio_analog_out_point_set(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    let value_bits = params.get(1).copied().unwrap_or(0) as u32;
    let value = f32::from_bits(value_bits) as f64;
    bridge_queue_write(channel, value);
    Ok(1)
}

/// `analogValuePoint.set(channel, value) -> bool` (4::6)
///
/// Writes an analog value point (same as analog output).
pub fn eacio_analog_value_point_set(
    _ctx: &mut NativeContext<'_>,
    params: &[i32],
) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    let value_bits = params.get(1).copied().unwrap_or(0) as u32;
    let value = f32::from_bits(value_bits) as f64;
    bridge_queue_write(channel, value);
    Ok(1)
}

/// `eacio.resolveChannel(markers) -> int` (4::7)
///
/// Resolves a channel by marker string (comma-separated tag names).
/// Returns the channel ID or -1 if not found.
pub fn eacio_resolve_channel(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    // In the pure Rust VM, the markers parameter is a Sedona Str pointer
    // which requires memory access to dereference.  Without bridge
    // initialized, return -1.  With bridge, we'd need to read the string
    // from VM memory.  For now, return -1 (not found).
    // TODO: Phase B — read Str from VM memory via ctx.memory
    Ok(-1)
}

/// `eacio.getRecordCount(markers) -> int` (4::8)
///
/// Returns the number of channels matching the marker string.
pub fn eacio_get_record_count(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    // TODO: Phase B — read Str from VM memory, call bridge_count_matching
    Ok(0)
}

/// `eacio.getCurStatus(channel, buf) -> bool` (4::9)
///
/// Writes the channel's current status string into `buf`.
/// Returns true if status was written, false otherwise.
pub fn eacio_get_cur_status(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    let _has_status = bridge_read_float(channel).is_some();
    // TODO: Phase B — write status string into Sedona Str buffer
    Ok(0) // false — no status string written yet
}

/// `eacio.getChannelName(channel, buf) -> bool` (4::10)
///
/// Writes the channel's label into `buf`.
/// Returns true if name was written, false otherwise.
pub fn eacio_get_channel_name(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    let _label = bridge_get_label(channel);
    // TODO: Phase B — write label into Sedona Str buffer
    Ok(0) // false — no name written yet
}

/// `triacPoint.set(channel, value) -> bool` (4::11)
///
/// Writes a triac output point (binary output for SSR/triac control).
pub fn eacio_triac_point_set(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    let value = params.get(1).copied().unwrap_or(0);
    bridge_queue_write(channel, if value != 0 { 1.0 } else { 0.0 });
    Ok(1)
}

/// `eacio.writeSedonaId(channel, sedonaId) -> int` (4::12)
///
/// Tags a channel with its Sedona component ID for cross-referencing.
pub fn eacio_write_sedona_id(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    let sedona_id = params.get(1).copied().unwrap_or(0);
    bridge_queue_tag_write(channel, "sedonaId".to_string(), sedona_id.to_string());
    Ok(0)
}

/// `eacio.writeSedonaType(channel, type1, type2) -> int` (4::13)
///
/// Tags a channel with its Sedona component type info.
pub fn eacio_write_sedona_type(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    // type1, type2 are Sedona Str pointers — need VM memory access to read
    // TODO: Phase B — read Str from VM memory
    let _ = channel;
    Ok(0)
}

/// `eacio.isChannelEnabled(channel) -> bool` (4::14)
///
/// Returns true if the channel is enabled in the engine configuration.
pub fn eacio_is_channel_enabled(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    let enabled = bridge_is_enabled(channel).unwrap_or(false);
    Ok(enabled as i32)
}

/// `eacio.getBoolTagValue(channel, tagName) -> bool` (4::15)
///
/// Returns the boolean value of a tag on the channel.
pub fn eacio_get_bool_tag_value(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0) as u32;
    // TODO: Phase B — read tag name Str from VM memory, call bridge_get_tag
    Ok(0) // false
}

/// `eacio.getNumberTagValue(channel, tagName) -> float` (4::16)
///
/// Returns the numeric value of a tag on the channel (as float bits).
pub fn eacio_get_number_tag_value(
    _ctx: &mut NativeContext<'_>,
    params: &[i32],
) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0) as u32;
    // TODO: Phase B — read tag name Str, return Number tag value
    Ok(0_f32.to_bits() as i32)
}

/// `eacio.getStringTagValue(channel, tagName, buf) -> void` (4::17)
///
/// Writes the string value of a tag into `buf`.
pub fn eacio_get_string_tag_value(
    _ctx: &mut NativeContext<'_>,
    _params: &[i32],
) -> VmResult<i32> {
    // TODO: Phase B — read tag name, write value into Sedona Str buffer
    Ok(0)
}

/// `eacio.getTagType(channel, tagName) -> int` (4::18)
///
/// Returns the type code of a tag: 0=unknown, 1=marker, 2=bool, 3=number, 4=str.
pub fn eacio_get_tag_type(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0) as u32;
    // TODO: Phase B — read tag name Str, call bridge_get_tag, return type_code
    Ok(0) // unknown
}

/// `eacio.getLevel(channel) -> int` (4::19)
///
/// Returns the active write priority level (1-17, 17 = default).
pub fn eacio_get_level(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    let level = bridge_get_level(channel).unwrap_or(17);
    Ok(level as i32)
}

/// `eacio.getLevelValue(channel, level) -> float` (4::20)
///
/// Returns the value at a specific write priority level (as float bits).
pub fn eacio_get_level_value(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    let level = params.get(1).copied().unwrap_or(0) as usize;
    let val = bridge_get_level_value(channel, level).unwrap_or(0.0) as f32;
    Ok(val.to_bits() as i32)
}

/// `eacio.getChannelIn(channel) -> int` (4::21)
///
/// Returns the virtual channel input source (-1 = not virtual).
pub fn eacio_get_channel_in(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    let channel_in = bridge_get_channel_in(channel).unwrap_or(-1);
    Ok(channel_in)
}

/// `analogValuePoint.get(channel) -> float` (4::22)
///
/// Reads an analog value point (same as analogInPoint.get for reads).
pub fn eacio_analog_value_point_get(
    _ctx: &mut NativeContext<'_>,
    params: &[i32],
) -> VmResult<i32> {
    let channel = params.first().copied().unwrap_or(0) as u32;
    let val = bridge_read_float(channel).unwrap_or(0.0) as f32;
    Ok(val.to_bits() as i32)
}

// ════════════════════════════════════════════════════════════════
// Registration
// ════════════════════════════════════════════════════════════════

/// Register all Kit 4 (EacIo) methods in a [`NativeTable`].
///
/// This replaces the inline kit 4 stubs in `native_table.rs` with
/// bridge-aware implementations.  Slot 0 remains a stub (NULL in C).
pub fn register_kit4_eacio(table: &mut NativeTable) {
    table.set_kit_name(EACIO_KIT_ID, "EacIo");

    // Slot 0 is reserved (NULL in C nativetable)
    table.register_stub(EACIO_KIT_ID, 0);

    // Point I/O methods
    table.register(EACIO_KIT_ID, 1, eacio_bool_in_point_get);
    table.register(EACIO_KIT_ID, 2, eacio_bool_out_point_set);
    table.register(EACIO_KIT_ID, 3, eacio_binary_value_point_set);
    table.register(EACIO_KIT_ID, 4, eacio_analog_in_point_get);
    table.register(EACIO_KIT_ID, 5, eacio_analog_out_point_set);
    table.register(EACIO_KIT_ID, 6, eacio_analog_value_point_set);

    // Channel query methods
    table.register(EACIO_KIT_ID, 7, eacio_resolve_channel);
    table.register(EACIO_KIT_ID, 8, eacio_get_record_count);
    table.register(EACIO_KIT_ID, 9, eacio_get_cur_status);
    table.register(EACIO_KIT_ID, 10, eacio_get_channel_name);

    // Triac output
    table.register(EACIO_KIT_ID, 11, eacio_triac_point_set);

    // Tag write methods
    table.register(EACIO_KIT_ID, 12, eacio_write_sedona_id);
    table.register(EACIO_KIT_ID, 13, eacio_write_sedona_type);

    // Channel property queries
    table.register(EACIO_KIT_ID, 14, eacio_is_channel_enabled);
    table.register(EACIO_KIT_ID, 15, eacio_get_bool_tag_value);
    table.register(EACIO_KIT_ID, 16, eacio_get_number_tag_value);
    table.register(EACIO_KIT_ID, 17, eacio_get_string_tag_value);
    table.register(EACIO_KIT_ID, 18, eacio_get_tag_type);

    // Priority level methods
    table.register(EACIO_KIT_ID, 19, eacio_get_level);
    table.register(EACIO_KIT_ID, 20, eacio_get_level_value);

    // Virtual channel methods
    table.register(EACIO_KIT_ID, 21, eacio_get_channel_in);
    table.register(EACIO_KIT_ID, 22, eacio_analog_value_point_get);
}

// ════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_table::{NativeContext, NativeEntry, NativeTable};

    fn test_ctx() -> (Vec<u8>, Vec<i32>) {
        (vec![0u8; 64], vec![])
    }

    // ── Point I/O: reads return defaults without bridge ─────────

    #[test]
    fn bool_in_point_get_returns_false_without_bridge() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let result = eacio_bool_in_point_get(&mut ctx, &[1113]).unwrap();
        assert_eq!(result, 0, "should return false without bridge");
    }

    #[test]
    fn analog_in_point_get_returns_zero_without_bridge() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let bits = eacio_analog_in_point_get(&mut ctx, &[1100]).unwrap();
        assert_eq!(f32::from_bits(bits as u32), 0.0);
    }

    #[test]
    fn analog_value_point_get_returns_zero_without_bridge() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let bits = eacio_analog_value_point_get(&mut ctx, &[1100]).unwrap();
        assert_eq!(f32::from_bits(bits as u32), 0.0);
    }

    // ── Point I/O: writes return success ────────────────────────

    #[test]
    fn bool_out_point_set_returns_success() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_bool_out_point_set(&mut ctx, &[1113, 1]).unwrap(), 1);
    }

    #[test]
    fn analog_out_point_set_returns_success() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let val_bits = 72.5_f32.to_bits() as i32;
        assert_eq!(eacio_analog_out_point_set(&mut ctx, &[1100, val_bits]).unwrap(), 1);
    }

    #[test]
    fn analog_value_point_set_returns_success() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_analog_value_point_set(&mut ctx, &[1100, 0]).unwrap(), 1);
    }

    #[test]
    fn binary_value_point_set_returns_success() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_binary_value_point_set(&mut ctx, &[1100, 1]).unwrap(), 1);
    }

    #[test]
    fn triac_point_set_returns_success() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_triac_point_set(&mut ctx, &[1100, 1]).unwrap(), 1);
    }

    // ── Channel queries: return defaults without bridge ─────────

    #[test]
    fn resolve_channel_returns_not_found() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_resolve_channel(&mut ctx, &[]).unwrap(), -1);
    }

    #[test]
    fn get_record_count_returns_zero() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_get_record_count(&mut ctx, &[]).unwrap(), 0);
    }

    #[test]
    fn get_cur_status_returns_false() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_get_cur_status(&mut ctx, &[1113]).unwrap(), 0);
    }

    #[test]
    fn get_channel_name_returns_false() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_get_channel_name(&mut ctx, &[1113]).unwrap(), 0);
    }

    #[test]
    fn is_channel_enabled_returns_false_without_bridge() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_is_channel_enabled(&mut ctx, &[1113]).unwrap(), 0);
    }

    // ── Tag queries: return defaults without bridge ─────────────

    #[test]
    fn get_bool_tag_value_returns_false() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_get_bool_tag_value(&mut ctx, &[1113]).unwrap(), 0);
    }

    #[test]
    fn get_number_tag_value_returns_zero() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let bits = eacio_get_number_tag_value(&mut ctx, &[1113]).unwrap();
        assert_eq!(f32::from_bits(bits as u32), 0.0);
    }

    #[test]
    fn get_string_tag_value_returns_zero() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_get_string_tag_value(&mut ctx, &[]).unwrap(), 0);
    }

    #[test]
    fn get_tag_type_returns_unknown() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_get_tag_type(&mut ctx, &[1113]).unwrap(), 0);
    }

    // ── Level queries: return defaults without bridge ───────────

    #[test]
    fn get_level_returns_default_without_bridge() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_get_level(&mut ctx, &[1113]).unwrap(), 17);
    }

    #[test]
    fn get_level_value_returns_zero_without_bridge() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let bits = eacio_get_level_value(&mut ctx, &[1113, 8]).unwrap();
        assert_eq!(f32::from_bits(bits as u32), 0.0);
    }

    #[test]
    fn get_channel_in_returns_not_virtual() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_get_channel_in(&mut ctx, &[1113]).unwrap(), -1);
    }

    // ── Tag writes succeed silently ─────────────────────────────

    #[test]
    fn write_sedona_id_returns_zero() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_write_sedona_id(&mut ctx, &[1113, 42]).unwrap(), 0);
    }

    #[test]
    fn write_sedona_type_returns_zero() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(eacio_write_sedona_type(&mut ctx, &[1113]).unwrap(), 0);
    }

    // ── Registration ────────────────────────────────────────────

    #[test]
    fn register_kit4_populates_table() {
        let mut table = NativeTable::new();
        register_kit4_eacio(&mut table);

        assert_eq!(table.kit_name(EACIO_KIT_ID), Some("EacIo"));
        assert_eq!(table.method_count(EACIO_KIT_ID), EACIO_METHOD_COUNT as usize);

        // Slot 0 is stub
        assert!(!table.is_implemented(EACIO_KIT_ID, 0));

        // Slots 1-22 are real
        for id in 1..EACIO_METHOD_COUNT {
            assert!(
                table.is_implemented(EACIO_KIT_ID, id),
                "EacIo method {id} should be implemented"
            );
        }
    }

    #[test]
    fn register_kit4_implemented_count() {
        let mut table = NativeTable::new();
        register_kit4_eacio(&mut table);
        assert_eq!(table.implemented_count(EACIO_KIT_ID), 22);
    }

    #[test]
    fn register_kit4_methods_callable_via_dispatch() {
        let mut table = NativeTable::new();
        register_kit4_eacio(&mut table);

        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);

        // boolInPoint.get
        let r = table.call(EACIO_KIT_ID, 1, &mut ctx, &[1113]).unwrap();
        assert_eq!(r, 0);

        // analogInPoint.get
        let r = table.call(EACIO_KIT_ID, 4, &mut ctx, &[1100]).unwrap();
        assert_eq!(f32::from_bits(r as u32), 0.0);

        // boolOutPoint.set
        let r = table.call(EACIO_KIT_ID, 2, &mut ctx, &[1113, 1]).unwrap();
        assert_eq!(r, 1);

        // getLevel
        let r = table.call(EACIO_KIT_ID, 19, &mut ctx, &[1113]).unwrap();
        assert_eq!(r, 17);

        // getChannelIn
        let r = table.call(EACIO_KIT_ID, 21, &mut ctx, &[1113]).unwrap();
        assert_eq!(r, -1);
    }

    // ── Edge cases ──────────────────────────────────────────────

    #[test]
    fn methods_handle_empty_params() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);

        // All methods should handle empty params gracefully
        assert!(eacio_bool_in_point_get(&mut ctx, &[]).is_ok());
        assert!(eacio_bool_out_point_set(&mut ctx, &[]).is_ok());
        assert!(eacio_analog_in_point_get(&mut ctx, &[]).is_ok());
        assert!(eacio_analog_out_point_set(&mut ctx, &[]).is_ok());
        assert!(eacio_resolve_channel(&mut ctx, &[]).is_ok());
        assert!(eacio_get_record_count(&mut ctx, &[]).is_ok());
        assert!(eacio_get_cur_status(&mut ctx, &[]).is_ok());
        assert!(eacio_get_channel_name(&mut ctx, &[]).is_ok());
        assert!(eacio_triac_point_set(&mut ctx, &[]).is_ok());
        assert!(eacio_write_sedona_id(&mut ctx, &[]).is_ok());
        assert!(eacio_write_sedona_type(&mut ctx, &[]).is_ok());
        assert!(eacio_is_channel_enabled(&mut ctx, &[]).is_ok());
        assert!(eacio_get_bool_tag_value(&mut ctx, &[]).is_ok());
        assert!(eacio_get_number_tag_value(&mut ctx, &[]).is_ok());
        assert!(eacio_get_string_tag_value(&mut ctx, &[]).is_ok());
        assert!(eacio_get_tag_type(&mut ctx, &[]).is_ok());
        assert!(eacio_get_level(&mut ctx, &[]).is_ok());
        assert!(eacio_get_level_value(&mut ctx, &[]).is_ok());
        assert!(eacio_get_channel_in(&mut ctx, &[]).is_ok());
        assert!(eacio_analog_value_point_get(&mut ctx, &[]).is_ok());
    }
}
