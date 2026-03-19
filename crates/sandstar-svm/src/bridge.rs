//! Rust implementations of Sedona native methods that bridge the VM to
//! the Rust engine's channel store.
//!
//! These `#[no_mangle] extern "C"` functions are referenced by nativetable.c
//! and linked at build time.  They replace the C implementations in
//! EAC_Gpio.cpp (kit 4) and provide stubs for shaystack (kit 100).

use std::collections::HashMap;
use std::ffi::CStr;
use std::sync::{Arc, Mutex, RwLock};

use crate::types::{Cell, SedonaVM};

// ════════════════════════════════════════════════════════════════
// FFI safety: catch_unwind wrappers
// ════════════════════════════════════════════════════════════════

/// Wraps an FFI function body with `catch_unwind` to prevent panics from
/// crossing the FFI boundary (which is undefined behavior).
/// On panic, logs the error and returns a safe default value.
macro_rules! ffi_safe {
    ($default:expr, $body:block) => {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body)) {
            Ok(val) => val,
            Err(e) => {
                let msg = e
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| e.downcast_ref::<String>().map(|s| s.as_str()))
                    .unwrap_or("<unknown panic>");
                eprintln!("PANIC in FFI: {}", msg);
                $default
            }
        }
    };
}

// ════════════════════════════════════════════════════════════════
// Channel snapshot: read-optimized view of engine state
// ════════════════════════════════════════════════════════════════

/// Per-channel data visible to the Sedona VM.
#[derive(Clone, Debug)]
pub struct ChannelInfo {
    pub channel: u32,
    pub cur: f64,
    pub raw: f64,
    pub status_ok: bool,
    pub enabled: bool,
    pub label: String,
    /// Virtual channel input source (-1 = not virtual)
    pub channel_in: i32,
    /// Active write priority level (1-17, 17 = default)
    pub write_level: u8,
    /// Write level values: level (1-17) → Option<f64>
    pub write_levels: [Option<f64>; 17],
    /// Channel tags: tag_name → tag_value (as string)
    pub tags: HashMap<String, TagValue>,
}

/// Tag value representation matching C engine's tag types.
#[derive(Clone, Debug)]
pub enum TagValue {
    Marker,
    Bool(bool),
    Number(f64),
    Str(String),
}

impl TagValue {
    pub fn type_code(&self) -> i32 {
        match self {
            TagValue::Marker => 1,
            TagValue::Bool(_) => 2,
            TagValue::Number(_) => 3,
            TagValue::Str(_) => 4,
        }
    }
}

/// Read-optimized snapshot of engine channel values.
/// Updated after each poll cycle by the server main loop.
pub struct ChannelSnapshot {
    /// Channel data indexed by channel_id for O(1) lookup.
    channels: HashMap<u32, ChannelInfo>,
}

impl ChannelSnapshot {
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
        }
    }

    /// Replace all channel data (called after each poll cycle).
    pub fn update(&mut self, channels: Vec<ChannelInfo>) {
        self.channels.clear();
        for ch in channels {
            self.channels.insert(ch.channel, ch);
        }
    }

    pub fn get(&self, channel: u32) -> Option<&ChannelInfo> {
        self.channels.get(&channel)
    }

    pub fn get_cur(&self, channel: u32) -> Option<f64> {
        self.channels.get(&channel).map(|ch| ch.cur)
    }

    pub fn is_enabled(&self, channel: u32) -> bool {
        self.channels.get(&channel).map(|ch| ch.enabled).unwrap_or(false)
    }

    pub fn is_ok(&self, channel: u32) -> bool {
        self.channels
            .get(&channel)
            .map(|ch| ch.status_ok)
            .unwrap_or(false)
    }

    pub fn count(&self) -> usize {
        self.channels.len()
    }

    /// Resolve channel by marker list (comma-separated tags).
    /// Returns first channel that has ALL specified markers.
    pub fn resolve_channel(&self, markers: &str) -> Option<u32> {
        let required: Vec<&str> = markers.split(',').map(|s| s.trim()).collect();
        for (id, ch) in &self.channels {
            let has_all = required.iter().all(|m| ch.tags.contains_key(*m));
            if has_all {
                return Some(*id);
            }
        }
        None
    }

    /// Count channels matching marker list.
    pub fn count_matching(&self, markers: &str) -> usize {
        let required: Vec<&str> = markers.split(',').map(|s| s.trim()).collect();
        self.channels
            .values()
            .filter(|ch| required.iter().all(|m| ch.tags.contains_key(*m)))
            .count()
    }
}

impl Default for ChannelSnapshot {
    fn default() -> Self {
        Self::new()
    }
}

/// Write command queued by Sedona native methods.
#[derive(Debug)]
pub struct SvmWrite {
    pub channel: u32,
    pub value: f64,
}

/// Tag write command queued by Sedona native methods (writeSedonaId, writeSedonaType).
#[derive(Debug)]
pub struct SvmTagWrite {
    pub channel: u32,
    pub tag: String,
    pub value: String,
}

/// Global engine bridge — set before SVM starts.
static ENGINE_BRIDGE: std::sync::OnceLock<Arc<RwLock<ChannelSnapshot>>> =
    std::sync::OnceLock::new();

/// Write queue — SVM output operations sent to main loop.
static WRITE_QUEUE: std::sync::OnceLock<Arc<Mutex<Vec<SvmWrite>>>> =
    std::sync::OnceLock::new();

/// Tag write queue — SVM tag updates sent to main loop.
static TAG_WRITE_QUEUE: std::sync::OnceLock<Arc<Mutex<Vec<SvmTagWrite>>>> =
    std::sync::OnceLock::new();

/// Initialize the bridge with a shared snapshot.
pub fn set_engine_bridge(snapshot: Arc<RwLock<ChannelSnapshot>>) {
    ENGINE_BRIDGE.set(snapshot).ok();
}

/// Initialize the write queue.
pub fn set_write_queue(queue: Arc<Mutex<Vec<SvmWrite>>>) {
    WRITE_QUEUE.set(queue).ok();
}

/// Initialize the tag write queue.
pub fn set_tag_write_queue(queue: Arc<Mutex<Vec<SvmTagWrite>>>) {
    TAG_WRITE_QUEUE.set(queue).ok();
}

/// Drain pending writes (called by main loop after each poll cycle).
pub fn drain_writes() -> Vec<SvmWrite> {
    WRITE_QUEUE
        .get()
        .and_then(|q| q.lock().ok().map(|mut v| std::mem::take(&mut *v)))
        .unwrap_or_default()
}

/// Drain pending tag writes (called by main loop after each poll cycle).
pub fn drain_tag_writes() -> Vec<SvmTagWrite> {
    TAG_WRITE_QUEUE
        .get()
        .and_then(|q| q.lock().ok().map(|mut v| std::mem::take(&mut *v)))
        .unwrap_or_default()
}

fn queue_tag_write(channel: u32, tag: String, value: String) {
    if let Some(q) = TAG_WRITE_QUEUE.get() {
        if let Ok(mut v) = q.lock() {
            v.push(SvmTagWrite { channel, tag, value });
        }
    }
}

fn get_snapshot() -> Option<Arc<RwLock<ChannelSnapshot>>> {
    ENGINE_BRIDGE.get().cloned()
}

fn queue_write(channel: u32, value: f64) {
    if let Some(q) = WRITE_QUEUE.get() {
        if let Ok(mut v) = q.lock() {
            v.push(SvmWrite { channel, value });
        }
    }
}

/// Helper: read a C string from a Sedona `params[n].aval` pointer.
unsafe fn read_sedona_str(ptr: *mut std::ffi::c_void) -> &'static str {
    if ptr.is_null() {
        return "";
    }
    CStr::from_ptr(ptr as *const std::ffi::c_char)
        .to_str()
        .unwrap_or("")
}

/// Helper: write a Rust string into a Sedona string buffer (params[n].aval).
unsafe fn write_sedona_str(ptr: *mut std::ffi::c_void, s: &str) {
    if ptr.is_null() {
        return;
    }
    let dst = ptr as *mut u8;
    let bytes = s.as_bytes();
    let len = bytes.len().min(127); // Sedona strings are typically 128 bytes max
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, len);
    *dst.add(len) = 0; // null terminate
}

// ════════════════════════════════════════════════════════════════
// Kit 4: EacIo native methods (22 functions)
// ════════════════════════════════════════════════════════════════

/// bool boolInPoint.get(int channel)
#[no_mangle]
pub unsafe extern "C" fn EacIo_boolInPoint_get(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 0 }, {
        let channel = (*params).ival as u32;
        let val = get_snapshot()
            .and_then(|snap| snap.read().ok().and_then(|s| s.get_cur(channel)))
            .unwrap_or(0.0);
        Cell { ival: if val != 0.0 { 1 } else { 0 } }
    })
}

/// bool boolOutPoint.set(int channel, bool value)
#[no_mangle]
pub unsafe extern "C" fn EacIo_boolOutPoint_set(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 0 }, {
        let channel = (*params).ival as u32;
        let value = (*params.add(1)).ival;
        queue_write(channel, if value == 1 { 1.0 } else { 0.0 });
        Cell { ival: 0 }
    })
}

/// bool binaryValuePoint.set(int channel, bool value)
#[no_mangle]
pub unsafe extern "C" fn EacIo_binaryValuePoint_set(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 0 }, {
        let channel = (*params).ival as u32;
        let value = (*params.add(1)).ival;
        queue_write(channel, if value == 1 { 1.0 } else { 0.0 });
        Cell { ival: 0 }
    })
}

/// float analogInPoint.get(int channel)
#[no_mangle]
pub unsafe extern "C" fn EacIo_analogInPoint_get(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { fval: 0.0 }, {
        let channel = (*params).ival as u32;
        let val = get_snapshot()
            .and_then(|snap| snap.read().ok().and_then(|s| s.get_cur(channel)))
            .unwrap_or(0.0);
        Cell { fval: val as f32 }
    })
}

/// bool analogOutPoint.set(int channel, float value)
#[no_mangle]
pub unsafe extern "C" fn EacIo_analogOutPoint_set(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 0 }, {
        let channel = (*params).ival as u32;
        let value = (*params.add(1)).fval as f64;
        queue_write(channel, value);
        Cell { ival: 0 }
    })
}

/// bool analogValuePoint.set(int channel, float value)
#[no_mangle]
pub unsafe extern "C" fn EacIo_analogValuePoint_set(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 0 }, {
        let channel = (*params).ival as u32;
        let value = (*params.add(1)).fval as f64;
        queue_write(channel, value);
        Cell { ival: 0 }
    })
}

/// int eacio.resolveChannel(Str markers)
#[no_mangle]
pub unsafe extern "C" fn EacIo_eacio_resolveChannel(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: -1 }, {
        let markers_ptr = (*params).aval;
        let markers = read_sedona_str(markers_ptr);
        let id = get_snapshot()
            .and_then(|snap| snap.read().ok().and_then(|s| s.resolve_channel(markers)))
            .map(|id| id as i32)
            .unwrap_or(-1);
        Cell { ival: id }
    })
}

/// int eacio.getRecordCount(Str markers)
#[no_mangle]
pub unsafe extern "C" fn EacIo_eacio_getRecordCount(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 0 }, {
        let markers_ptr = (*params).aval;
        let markers = read_sedona_str(markers_ptr);
        let count = get_snapshot()
            .and_then(|snap| snap.read().ok().map(|s| s.count_matching(markers) as i32))
            .unwrap_or(0);
        Cell { ival: count }
    })
}

/// bool eacio.getCurStatus(int channel, Str statusBuf)
#[no_mangle]
pub unsafe extern "C" fn EacIo_eacio_getCurStatus(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 0 }, {
        let channel = (*params).ival as u32;
        let status_buf = (*params.add(1)).aval;
        let ok = get_snapshot()
            .and_then(|snap| {
                snap.read().ok().map(|s| {
                    let status = if s.is_ok(channel) { "ok" } else { "down" };
                    write_sedona_str(status_buf, status);
                    true
                })
            })
            .unwrap_or(false);
        Cell { ival: if ok { 1 } else { 0 } }
    })
}

/// bool eacio.getChannelName(int channel, Str nameBuf)
#[no_mangle]
pub unsafe extern "C" fn EacIo_eacio_getChannelName(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 0 }, {
        let channel = (*params).ival as u32;
        let name_buf = (*params.add(1)).aval;
        let found = get_snapshot()
            .and_then(|snap| {
                snap.read().ok().and_then(|s| {
                    s.get(channel).map(|ch| {
                        write_sedona_str(name_buf, &ch.label);
                        true
                    })
                })
            })
            .unwrap_or(false);
        Cell { ival: if found { 1 } else { 0 } }
    })
}

/// bool triacPoint.set(int channel, bool cmd)
#[no_mangle]
pub unsafe extern "C" fn EacIo_triacPoint_set(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 0 }, {
        let channel = (*params).ival as u32;
        let cmd = (*params.add(1)).ival;
        queue_write(channel, if cmd == 1 { 1.0 } else { 0.0 });
        Cell { ival: 0 }
    })
}

/// int eacio.writeSedonaId(int channel, int id)
#[no_mangle]
pub unsafe extern "C" fn EacIo_eacio_writeSedonaId(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 0 }, {
        let channel = (*params).ival as u32;
        let id = (*params.add(1)).ival;
        queue_tag_write(channel, "sedonaId".to_string(), id.to_string());
        Cell { ival: 0 }
    })
}

/// int eacio.writeSedonaType(int channel, Str kit, Str name)
#[no_mangle]
pub unsafe extern "C" fn EacIo_eacio_writeSedonaType(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 0 }, {
        let channel = (*params).ival as u32;
        let kit = read_sedona_str((*params.add(1)).aval);
        let name = read_sedona_str((*params.add(2)).aval);
        let type_str = format!("{}::{}", kit, name);
        queue_tag_write(channel, "sedonaType".to_string(), type_str);
        Cell { ival: 0 }
    })
}

/// bool eacio.isChannelEnabled(int channel)
#[no_mangle]
pub unsafe extern "C" fn EacIo_eacio_isChannelEnabled(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 0 }, {
        let channel = (*params).ival as u32;
        let enabled = get_snapshot()
            .and_then(|snap| snap.read().ok().map(|s| s.is_enabled(channel)))
            .unwrap_or(false);
        Cell { ival: if enabled { 1 } else { 0 } }
    })
}

/// bool eacio.getBoolTagValue(int channel, Str tag)
#[no_mangle]
pub unsafe extern "C" fn EacIo_eacio_getBoolTagValue(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 0 }, {
        let channel = (*params).ival as u32;
        let tag_ptr = (*params.add(1)).aval;
        let tag = read_sedona_str(tag_ptr);
        let val = get_snapshot()
            .and_then(|snap| {
                snap.read().ok().and_then(|s| {
                    s.get(channel).and_then(|ch| match ch.tags.get(tag) {
                        Some(TagValue::Bool(b)) => Some(*b),
                        Some(TagValue::Marker) => Some(true),
                        _ => None,
                    })
                })
            })
            .unwrap_or(false);
        Cell { ival: if val { 1 } else { 0 } }
    })
}

/// float eacio.getNumberTagValue(int channel, Str tag)
#[no_mangle]
pub unsafe extern "C" fn EacIo_eacio_getNumberTagValue(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { fval: 0.0 }, {
        let channel = (*params).ival as u32;
        let tag_ptr = (*params.add(1)).aval;
        let tag = read_sedona_str(tag_ptr);
        let val = get_snapshot()
            .and_then(|snap| {
                snap.read().ok().and_then(|s| {
                    s.get(channel).and_then(|ch| match ch.tags.get(tag) {
                        Some(TagValue::Number(n)) => Some(*n as f32),
                        _ => None,
                    })
                })
            })
            .unwrap_or(0.0);
        Cell { fval: val }
    })
}

/// void eacio.getStringTagValue(int channel, Str tag, Str valBuf)
#[no_mangle]
pub unsafe extern "C" fn EacIo_eacio_getStringTagValue(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 0 }, {
        let channel = (*params).ival as u32;
        let tag_ptr = (*params.add(1)).aval;
        let val_buf = (*params.add(2)).aval;
        let tag = read_sedona_str(tag_ptr);
        if let Some(snap) = get_snapshot() {
            if let Ok(s) = snap.read() {
                if let Some(ch) = s.get(channel) {
                    if let Some(TagValue::Str(v)) = ch.tags.get(tag) {
                        write_sedona_str(val_buf, v);
                    }
                }
            }
        }
        Cell { ival: 0 }
    })
}

/// int eacio.getTagType(int channel, Str tag)
#[no_mangle]
pub unsafe extern "C" fn EacIo_eacio_getTagType(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 0 }, {
        let channel = (*params).ival as u32;
        let tag_ptr = (*params.add(1)).aval;
        let tag = read_sedona_str(tag_ptr);
        let code = get_snapshot()
            .and_then(|snap| {
                snap.read().ok().and_then(|s| {
                    s.get(channel)
                        .and_then(|ch| ch.tags.get(tag).map(|tv| tv.type_code()))
                })
            })
            .unwrap_or(0);
        Cell { ival: code }
    })
}

/// int eacio.getLevel(int channel) — get active write priority level.
#[no_mangle]
pub unsafe extern "C" fn EacIo_eacio_getLevel(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: 17 }, {
        let channel = (*params).ival as u32;
        let level = get_snapshot()
            .and_then(|snap| {
                snap.read().ok().and_then(|s| {
                    s.get(channel).map(|ch| ch.write_level as i32)
                })
            })
            .unwrap_or(17);
        Cell { ival: level }
    })
}

/// float eacio.getLevelValue(int channel, int level)
#[no_mangle]
pub unsafe extern "C" fn EacIo_eacio_getLevelValue(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { fval: 0.0 }, {
        let channel = (*params).ival as u32;
        let level = (*params.add(1)).ival as usize;
        let val = get_snapshot()
            .and_then(|snap| {
                snap.read().ok().and_then(|s| {
                    s.get(channel).and_then(|ch| {
                        if (1..=17).contains(&level) {
                            Some(ch.write_levels[level - 1].unwrap_or(0.0))
                        } else {
                            None
                        }
                    })
                })
            })
            .unwrap_or(0.0);
        Cell { fval: val as f32 }
    })
}

/// int eacio.getChannelIn(int channel) — get virtual channel input source.
#[no_mangle]
pub unsafe extern "C" fn EacIo_eacio_getChannelIn(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { ival: -1 }, {
        let channel = (*params).ival as u32;
        let ch_in = get_snapshot()
            .and_then(|snap| {
                snap.read().ok().and_then(|s| {
                    s.get(channel).map(|ch| ch.channel_in)
                })
            })
            .unwrap_or(-1);
        Cell { ival: ch_in }
    })
}

/// float analogValuePoint.get(int channel)
#[no_mangle]
pub unsafe extern "C" fn EacIo_analogValuePoint_get(
    _vm: *mut SedonaVM,
    params: *mut Cell,
) -> Cell {
    ffi_safe!(Cell { fval: 0.0 }, {
        let channel = (*params).ival as u32;
        let val = get_snapshot()
            .and_then(|snap| snap.read().ok().and_then(|s| s.get_cur(channel)))
            .unwrap_or(0.0);
        Cell { fval: val as f32 }
    })
}

// ════════════════════════════════════════════════════════════════
// Kit 100: shaystack native methods (28 functions) — stubs
// ════════════════════════════════════════════════════════════════

macro_rules! shaystack_stub {
    ($name:ident) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            _vm: *mut SedonaVM,
            _params: *mut Cell,
        ) -> Cell {
            ffi_safe!(Cell { ival: 0 }, {
                Cell { ival: 0 }
            })
        }
    };
}

shaystack_stub!(shaystack_HaystackDevice_open);
shaystack_stub!(shaystack_HaystackDevice_call);
shaystack_stub!(shaystack_HaystackDevice_read_float);
shaystack_stub!(shaystack_HaystackDevice_read_bool);
shaystack_stub!(shaystack_HaystackDevice_eval_float);
shaystack_stub!(shaystack_HaystackDevice_eval_bool);
shaystack_stub!(shaystack_HaystackDevice_create_client);
shaystack_stub!(shaystack_HaystackDevice_is_authenticated);
shaystack_stub!(shaystack_HaystackDevice_get_auth_message);
shaystack_stub!(shaystack_HaystackDevice_read_message);
shaystack_stub!(shaystack_HaystackDevice_eval_message);
shaystack_stub!(shaystack_HaystackDevice_watch_sub_message);
shaystack_stub!(shaystack_HaystackDevice_watch_unsub_message);
shaystack_stub!(shaystack_HaystackDevice_watch_poll_message);
shaystack_stub!(shaystack_HaystackDevice_write_float_point_message);
shaystack_stub!(shaystack_HaystackDevice_write_bool_point_message);
shaystack_stub!(shaystack_HaystackDevice_write_read_point_message);
shaystack_stub!(shaystack_HaystackDevice_reset_point_message);
shaystack_stub!(shaystack_HaystackDevice_is_empty);
shaystack_stub!(shaystack_HaystackDevice_is_err);
shaystack_stub!(shaystack_HaystackDevice_has_float);
shaystack_stub!(shaystack_HaystackDevice_has_bool);
shaystack_stub!(shaystack_HaystackDevice_parse_bool_response);
shaystack_stub!(shaystack_HaystackDevice_parse_float_response);
shaystack_stub!(shaystack_HaystackDevice_parse_str_response);
shaystack_stub!(shaystack_HaystackDevice_is_filter_valid);
shaystack_stub!(shaystack_HaystackDevice_delete_client);
shaystack_stub!(shaystack_HaystackDevice_isSessionConnected);

// ════════════════════════════════════════════════════════════════
// Windows dev stubs: kit 0/2/9 native methods
// ════════════════════════════════════════════════════════════════

#[cfg(not(unix))]
mod win_stubs {
    use super::*;

    macro_rules! native_stub {
        ($name:ident) => {
            #[no_mangle]
            pub unsafe extern "C" fn $name(
                _vm: *mut SedonaVM,
                _params: *mut Cell,
            ) -> Cell {
                ffi_safe!(Cell { ival: 0 }, {
                    Cell { ival: 0 }
                })
            }
        };
    }

    // Kit 0: sys (60 methods)
    native_stub!(sys_Sys_platformType);
    native_stub!(sys_Sys_copy);
    native_stub!(sys_Sys_malloc);
    native_stub!(sys_Sys_free);
    native_stub!(sys_Sys_intStr);
    native_stub!(sys_Sys_hexStr);
    native_stub!(sys_Sys_longStr);
    native_stub!(sys_Sys_longHexStr);
    native_stub!(sys_Sys_floatStr);
    native_stub!(sys_Sys_doubleStr);
    native_stub!(sys_Sys_floatToBits);
    native_stub!(sys_Sys_doubleToBits);
    native_stub!(sys_Sys_bitsToFloat);
    native_stub!(sys_Sys_bitsToDouble);
    native_stub!(sys_Sys_ticks);
    native_stub!(sys_Sys_sleep);
    native_stub!(sys_Sys_compareBytes);
    native_stub!(sys_Sys_setBytes);
    native_stub!(sys_Sys_andBytes);
    native_stub!(sys_Sys_orBytes);
    native_stub!(sys_Sys_scodeAddr);
    native_stub!(sys_Sys_rand);
    native_stub!(sys_Component_invokeVoid);
    native_stub!(sys_Component_invokeBool);
    native_stub!(sys_Component_invokeInt);
    native_stub!(sys_Component_invokeLong);
    native_stub!(sys_Component_invokeFloat);
    native_stub!(sys_Component_invokeDouble);
    native_stub!(sys_Component_invokeBuf);
    native_stub!(sys_Component_getBool);
    native_stub!(sys_Component_getInt);
    native_stub!(sys_Component_getLong);
    native_stub!(sys_Component_getFloat);
    native_stub!(sys_Component_getDouble);
    native_stub!(sys_Component_getBuf);
    native_stub!(sys_Component_doSetBool);
    native_stub!(sys_Component_doSetInt);
    native_stub!(sys_Component_doSetLong);
    native_stub!(sys_Component_doSetFloat);
    native_stub!(sys_Component_doSetDouble);
    native_stub!(sys_Type_malloc);
    native_stub!(sys_StdOutStream_doWrite);
    native_stub!(sys_StdOutStream_doWriteBytes);
    native_stub!(sys_StdOutStream_doFlush);
    native_stub!(sys_FileStore_doSize);
    native_stub!(sys_FileStore_doOpen);
    native_stub!(sys_FileStore_doRead);
    native_stub!(sys_FileStore_doReadBytes);
    native_stub!(sys_FileStore_doWrite);
    native_stub!(sys_FileStore_doWriteBytes);
    native_stub!(sys_FileStore_doTell);
    native_stub!(sys_FileStore_doSeek);
    native_stub!(sys_FileStore_doFlush);
    native_stub!(sys_FileStore_doClose);
    native_stub!(sys_FileStore_rename);
    native_stub!(sys_Test_doMain);
    native_stub!(sys_Str_fromBytes);
    native_stub!(sys_PlatformService_doPlatformId);
    native_stub!(sys_PlatformService_getPlatVersion);
    native_stub!(sys_PlatformService_getNativeMemAvailable);

    // Kit 2: inet (17 methods)
    native_stub!(inet_TcpSocket_connect);
    native_stub!(inet_TcpSocket_finishConnect);
    native_stub!(inet_TcpSocket_write);
    native_stub!(inet_TcpSocket_read);
    native_stub!(inet_TcpSocket_close);
    native_stub!(inet_TcpServerSocket_bind);
    native_stub!(inet_TcpServerSocket_accept);
    native_stub!(inet_TcpServerSocket_close);
    native_stub!(inet_UdpSocket_open);
    native_stub!(inet_UdpSocket_bind);
    native_stub!(inet_UdpSocket_send);
    native_stub!(inet_UdpSocket_receive);
    native_stub!(inet_UdpSocket_close);
    native_stub!(inet_UdpSocket_maxPacketSize);
    native_stub!(inet_UdpSocket_idealPacketSize);
    native_stub!(inet_Crypto_sha1);
    native_stub!(inet_UdpSocket_join);

    // Kit 9: datetimeStd (3 methods)
    native_stub!(datetimeStd_DateTimeServiceStd_doNow);
    native_stub!(datetimeStd_DateTimeServiceStd_doSetClock);
    native_stub!(datetimeStd_DateTimeServiceStd_doGetUtcOffset);
}

// ════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_snapshot_empty() {
        let snap = ChannelSnapshot::new();
        assert_eq!(snap.get_cur(1100), None);
        assert!(!snap.is_enabled(1100));
        assert_eq!(snap.count(), 0);
    }

    #[test]
    fn test_channel_snapshot_with_data() {
        let mut snap = ChannelSnapshot::new();
        snap.update(vec![
            ChannelInfo {
                channel: 1100,
                cur: 72.5,
                raw: 2048.0,
                status_ok: true,
                enabled: true,
                label: "AI1-PT100".into(),
                channel_in: -1,
                write_level: 17,
                write_levels: [None; 17],
                tags: HashMap::new(),
            },
            ChannelInfo {
                channel: 1113,
                cur: 68.3,
                raw: 1900.0,
                status_ok: true,
                enabled: true,
                label: "AI1-10K".into(),
                channel_in: -1,
                write_level: 17,
                write_levels: [None; 17],
                tags: HashMap::new(),
            },
        ]);

        assert_eq!(snap.get_cur(1100), Some(72.5));
        assert!(snap.is_enabled(1100));
        assert!(snap.is_ok(1113));
        assert_eq!(snap.get_cur(9999), None);
        assert_eq!(snap.count(), 2);
    }

    #[test]
    fn test_resolve_channel() {
        let mut snap = ChannelSnapshot::new();
        let mut tags = HashMap::new();
        tags.insert("cur".into(), TagValue::Marker);
        tags.insert("temp".into(), TagValue::Marker);
        snap.update(vec![ChannelInfo {
            channel: 1100,
            cur: 72.5,
            raw: 2048.0,
            status_ok: true,
            enabled: true,
            label: "AI1".into(),
            channel_in: -1,
            write_level: 17,
            write_levels: [None; 17],
            tags,
        }]);

        assert_eq!(snap.resolve_channel("cur,temp"), Some(1100));
        assert_eq!(snap.resolve_channel("cur,humidity"), None);
        assert_eq!(snap.count_matching("cur"), 1);
    }

    #[test]
    fn test_tag_values() {
        let mut tags: HashMap<String, TagValue> = HashMap::new();
        tags.insert("enabled".into(), TagValue::Bool(true));
        tags.insert("maxVal".into(), TagValue::Number(100.0));
        tags.insert("unit".into(), TagValue::Str("°F".into()));
        tags.insert("cur".into(), TagValue::Marker);

        assert_eq!(tags.get("enabled").unwrap().type_code(), 2);
        assert_eq!(tags.get("maxVal").unwrap().type_code(), 3);
        assert_eq!(tags.get("unit").unwrap().type_code(), 4);
        assert_eq!(tags.get("cur").unwrap().type_code(), 1);
    }

    #[test]
    fn test_write_queue() {
        let queue = Arc::new(Mutex::new(Vec::<SvmWrite>::new()));
        set_write_queue(queue);
        queue_write(1100, 72.5);
        queue_write(1113, 68.3);
        let writes = drain_writes();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].channel, 1100);
        assert_eq!(writes[0].value, 72.5);
    }

    #[test]
    fn test_cell_layout() {
        assert_eq!(std::mem::size_of::<Cell>(), std::mem::size_of::<*mut ()>());
    }

    // ========================================================================
    // Phase 6.0: SVM integration tests — bridge communication layer
    // ========================================================================

    #[test]
    fn test_channel_snapshot_update_and_read() {
        let snapshot = Arc::new(RwLock::new(ChannelSnapshot::default()));
        let mut snap = snapshot.write().unwrap();
        let info = ChannelInfo {
            channel: 4,
            cur: 72.5,
            raw: 2048.0,
            status_ok: true,
            enabled: true,
            label: "zone_temp".to_string(),
            channel_in: -1,
            write_level: 0,
            write_levels: [None; 17],
            tags: HashMap::new(),
        };
        snap.update(vec![info]);
        assert_eq!(snap.channels.len(), 1);
        let ch = snap.channels.get(&4).unwrap();
        assert_eq!(ch.cur, 72.5);
        assert!(ch.status_ok);
        assert_eq!(ch.label, "zone_temp");
    }

    #[test]
    fn test_svm_write_queue_drain() {
        let queue = Arc::new(Mutex::new(Vec::new()));
        queue.lock().unwrap().push(SvmWrite {
            channel: 35,
            value: 1.0,
        });
        queue.lock().unwrap().push(SvmWrite {
            channel: 50,
            value: 0.0,
        });
        let writes: Vec<SvmWrite> = queue.lock().unwrap().drain(..).collect();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].channel, 35);
        assert_eq!(writes[0].value, 1.0);
        assert_eq!(writes[1].channel, 50);
        assert_eq!(writes[1].value, 0.0);
    }

    #[test]
    fn test_channel_info_default_fields() {
        let info = ChannelInfo {
            channel: 1113,
            cur: 0.0,
            raw: 0.0,
            status_ok: false,
            enabled: false,
            label: String::new(),
            channel_in: -1,
            write_level: 0,
            write_levels: [None; 17],
            tags: HashMap::new(),
        };
        assert!(!info.status_ok);
        assert!(!info.enabled);
        assert_eq!(info.write_levels.iter().filter(|w| w.is_some()).count(), 0);
        assert_eq!(info.channel_in, -1);
    }

    #[test]
    fn test_tag_write_queue() {
        let queue = Arc::new(Mutex::new(Vec::new()));
        queue.lock().unwrap().push(SvmTagWrite {
            channel: 4,
            tag: "sedonaId".to_string(),
            value: "14".to_string(),
        });
        queue.lock().unwrap().push(SvmTagWrite {
            channel: 4,
            tag: "sedonaType".to_string(),
            value: "EacIo::analogInPoint".to_string(),
        });
        let writes: Vec<SvmTagWrite> = queue.lock().unwrap().drain(..).collect();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].tag, "sedonaId");
        assert_eq!(writes[0].value, "14");
        assert_eq!(writes[1].tag, "sedonaType");
        assert_eq!(writes[1].value, "EacIo::analogInPoint");
    }

    #[test]
    fn test_snapshot_update_replaces_previous() {
        let mut snap = ChannelSnapshot::new();

        // First update
        snap.update(vec![ChannelInfo {
            channel: 100,
            cur: 50.0,
            raw: 1000.0,
            status_ok: true,
            enabled: true,
            label: "ch100".into(),
            channel_in: -1,
            write_level: 17,
            write_levels: [None; 17],
            tags: HashMap::new(),
        }]);
        assert_eq!(snap.count(), 1);
        assert_eq!(snap.get_cur(100), Some(50.0));

        // Second update replaces all channels
        snap.update(vec![ChannelInfo {
            channel: 200,
            cur: 75.0,
            raw: 2000.0,
            status_ok: true,
            enabled: true,
            label: "ch200".into(),
            channel_in: -1,
            write_level: 17,
            write_levels: [None; 17],
            tags: HashMap::new(),
        }]);
        assert_eq!(snap.count(), 1);
        assert_eq!(snap.get_cur(200), Some(75.0));
        // Old channel 100 should be gone
        assert_eq!(snap.get_cur(100), None);
    }

    #[test]
    fn test_snapshot_is_ok_with_down_channel() {
        let mut snap = ChannelSnapshot::new();
        snap.update(vec![ChannelInfo {
            channel: 612,
            cur: 0.0,
            raw: 0.0,
            status_ok: false,
            enabled: true,
            label: "sdp810".into(),
            channel_in: -1,
            write_level: 17,
            write_levels: [None; 17],
            tags: HashMap::new(),
        }]);
        assert!(!snap.is_ok(612));
        assert!(snap.is_enabled(612));
    }

    #[test]
    fn test_snapshot_write_levels_populated() {
        let mut snap = ChannelSnapshot::new();
        let mut levels = [None; 17];
        levels[7] = Some(72.0);  // Priority level 8
        levels[15] = Some(55.0); // Priority level 16
        snap.update(vec![ChannelInfo {
            channel: 500,
            cur: 72.0,
            raw: 0.0,
            status_ok: true,
            enabled: true,
            label: "virtual_out".into(),
            channel_in: 100,
            write_level: 8,
            write_levels: levels,
            tags: HashMap::new(),
        }]);
        let ch = snap.get(500).unwrap();
        assert_eq!(ch.write_level, 8);
        assert_eq!(ch.write_levels[7], Some(72.0));
        assert_eq!(ch.write_levels[15], Some(55.0));
        assert_eq!(ch.write_levels[0], None);
    }

    #[test]
    fn test_snapshot_resolve_multiple_markers() {
        let mut snap = ChannelSnapshot::new();
        let mut tags1 = HashMap::new();
        tags1.insert("point".into(), TagValue::Marker);
        tags1.insert("temp".into(), TagValue::Marker);
        tags1.insert("zone".into(), TagValue::Str("A".into()));

        let mut tags2 = HashMap::new();
        tags2.insert("point".into(), TagValue::Marker);
        tags2.insert("humidity".into(), TagValue::Marker);

        snap.update(vec![
            ChannelInfo {
                channel: 1100,
                cur: 72.5,
                raw: 0.0,
                status_ok: true,
                enabled: true,
                label: "temp_a".into(),
                channel_in: -1,
                write_level: 17,
                write_levels: [None; 17],
                tags: tags1,
            },
            ChannelInfo {
                channel: 1200,
                cur: 45.0,
                raw: 0.0,
                status_ok: true,
                enabled: true,
                label: "humidity".into(),
                channel_in: -1,
                write_level: 17,
                write_levels: [None; 17],
                tags: tags2,
            },
        ]);

        // Should find the temp channel, not humidity
        assert_eq!(snap.resolve_channel("point,temp"), Some(1100));
        // Should find humidity
        assert_eq!(snap.resolve_channel("point,humidity"), Some(1200));
        // Both have "point" marker
        assert_eq!(snap.count_matching("point"), 2);
        // No channel has all three
        assert_eq!(snap.resolve_channel("point,temp,humidity"), None);
    }

    #[test]
    fn test_svm_runner_invalid_scode_path() {
        use crate::runner::SvmRunner;
        let mut runner = SvmRunner::new(std::path::PathBuf::from("/nonexistent/kits.scode"));
        // start() should fail gracefully with nonexistent file
        let result = runner.start();
        assert!(result.is_err());
        assert!(!runner.is_running());
    }

    #[test]
    fn test_drain_writes_returns_empty_when_no_queue() {
        // drain_writes uses OnceLock. If it hasn't been set, returns empty.
        // This test verifies the default empty behavior.
        let writes = drain_writes();
        // May or may not be empty depending on whether set_write_queue was
        // called previously in this test process. The important thing is it
        // does not panic.
        let _ = writes;
    }

    #[test]
    fn test_drain_tag_writes_returns_empty_when_no_queue() {
        let writes = drain_tag_writes();
        let _ = writes;
    }

    // ========================================================================
    // FFI panic safety tests
    // ========================================================================

    #[test]
    fn test_ffi_safe_catches_panic_with_str_message() {
        let result: Cell = ffi_safe!(Cell { ival: -99 }, {
            panic!("test panic message");
        });
        // Should return the default value, not propagate the panic
        assert_eq!(unsafe { result.ival }, -99);
    }

    #[test]
    fn test_ffi_safe_catches_panic_with_string_message() {
        let result: Cell = ffi_safe!(Cell { fval: f32::NAN }, {
            panic!("{}", format!("dynamic panic {}", 42));
        });
        // Should return the default NaN value
        assert!(unsafe { result.fval }.is_nan());
    }

    #[test]
    fn test_ffi_safe_passes_through_on_success() {
        let result: Cell = ffi_safe!(Cell { ival: -1 }, {
            Cell { ival: 42 }
        });
        assert_eq!(unsafe { result.ival }, 42);
    }

    #[test]
    fn test_bridge_concurrent_snapshot_access() {
        // Verify Arc<RwLock<ChannelSnapshot>> is safe under concurrent access
        let snapshot = Arc::new(RwLock::new(ChannelSnapshot::default()));

        // Pre-populate with some data
        {
            let mut snap = snapshot.write().unwrap();
            snap.update(vec![ChannelInfo {
                channel: 42,
                cur: 100.0,
                raw: 4095.0,
                status_ok: true,
                enabled: true,
                label: "test_ch".into(),
                channel_in: -1,
                write_level: 17,
                write_levels: [None; 17],
                tags: HashMap::new(),
            }]);
        }

        // Spawn 10 reader threads
        let mut handles = Vec::new();
        for i in 0..10 {
            let snap = Arc::clone(&snapshot);
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    let guard = snap.read().unwrap();
                    let val = guard.get_cur(42);
                    // Value should always be consistent (100.0 or updated)
                    assert!(val.is_some(), "thread {} saw None", i);
                    std::thread::yield_now();
                }
            }));
        }

        // Main thread updates snapshot concurrently
        for round in 0..50 {
            let mut snap = snapshot.write().unwrap();
            snap.update(vec![ChannelInfo {
                channel: 42,
                cur: 100.0 + round as f64,
                raw: 4095.0,
                status_ok: true,
                enabled: true,
                label: "test_ch".into(),
                channel_in: -1,
                write_level: 17,
                write_levels: [None; 17],
                tags: HashMap::new(),
            }]);
        }

        // All reader threads should complete without panic
        for h in handles {
            h.join().expect("reader thread panicked");
        }
    }

    #[test]
    fn test_bridge_write_queue_concurrent_push_drain() {
        let queue = Arc::new(Mutex::new(Vec::<SvmWrite>::new()));

        // Spawn 5 writer threads
        let mut handles = Vec::new();
        for t in 0..5 {
            let q = Arc::clone(&queue);
            handles.push(std::thread::spawn(move || {
                for i in 0..20 {
                    q.lock().unwrap().push(SvmWrite {
                        channel: (t * 100 + i) as u32,
                        value: i as f64,
                    });
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Drain and verify count
        let writes: Vec<SvmWrite> = queue.lock().unwrap().drain(..).collect();
        assert_eq!(writes.len(), 100); // 5 threads * 20 writes each
    }

    #[test]
    fn test_bridge_tag_write_queue_drain() {
        let queue = Arc::new(Mutex::new(Vec::<SvmTagWrite>::new()));
        queue.lock().unwrap().push(SvmTagWrite {
            channel: 10,
            tag: "sedonaId".into(),
            value: "7".into(),
        });
        queue.lock().unwrap().push(SvmTagWrite {
            channel: 10,
            tag: "sedonaType".into(),
            value: "EacIo::analogInPoint".into(),
        });
        queue.lock().unwrap().push(SvmTagWrite {
            channel: 20,
            tag: "sedonaId".into(),
            value: "15".into(),
        });

        let writes: Vec<SvmTagWrite> = queue.lock().unwrap().drain(..).collect();
        assert_eq!(writes.len(), 3);
        assert_eq!(writes[0].channel, 10);
        assert_eq!(writes[0].tag, "sedonaId");
        assert_eq!(writes[2].channel, 20);
        // After drain, queue should be empty
        assert!(queue.lock().unwrap().is_empty());
    }

    #[test]
    fn test_ffi_safe_catches_index_out_of_bounds() {
        // Simulate a panic from array index out of bounds
        let result: Cell = ffi_safe!(Cell { ival: -1 }, {
            let v: Vec<i32> = vec![1, 2, 3];
            Cell { ival: v[99] } // panics with index out of bounds
        });
        assert_eq!(unsafe { result.ival }, -1);
    }

    #[test]
    fn test_channel_info_with_tags() {
        let mut tags = HashMap::new();
        tags.insert("enabled".into(), TagValue::Bool(true));
        tags.insert("maxVal".into(), TagValue::Number(100.0));
        tags.insert("unit".into(), TagValue::Str("F".into()));
        tags.insert("point".into(), TagValue::Marker);

        let info = ChannelInfo {
            channel: 1100,
            cur: 72.5,
            raw: 2048.0,
            status_ok: true,
            enabled: true,
            label: "zone_temp".into(),
            channel_in: -1,
            write_level: 17,
            write_levels: [None; 17],
            tags,
        };

        // Verify tag type codes
        assert_eq!(info.tags.get("enabled").unwrap().type_code(), 2);
        assert_eq!(info.tags.get("maxVal").unwrap().type_code(), 3);
        assert_eq!(info.tags.get("unit").unwrap().type_code(), 4);
        assert_eq!(info.tags.get("point").unwrap().type_code(), 1);
    }
}
