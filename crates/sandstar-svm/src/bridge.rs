//! Bridge between the Sedona VM and the Rust engine's channel store.
//!
//! Provides shared data structures (ChannelSnapshot, SvmWrite, SvmTagWrite)
//! and queue primitives used by both the pure-Rust VM native methods
//! (native_eacio, native_sys, etc.) and the server main loop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

// ════════════════════════════════════════════════════════════════
// Panic safety: catch_unwind wrappers
// ════════════════════════════════════════════════════════════════

/// Wraps a function body with `catch_unwind` to prevent panics from
/// propagating across boundaries (which could be undefined behavior).
/// On panic, logs the error and returns a safe default value.
#[allow(unused_macros)]
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
                eprintln!("PANIC in bridge: {}", msg);
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

/// Tag value representation matching engine's tag types.
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
pub(crate) static ENGINE_BRIDGE: std::sync::OnceLock<Arc<RwLock<ChannelSnapshot>>> =
    std::sync::OnceLock::new();

/// Write queue — SVM output operations sent to main loop.
pub(crate) static WRITE_QUEUE: std::sync::OnceLock<Arc<Mutex<Vec<SvmWrite>>>> =
    std::sync::OnceLock::new();

/// Tag write queue — SVM tag updates sent to main loop.
pub(crate) static TAG_WRITE_QUEUE: std::sync::OnceLock<Arc<Mutex<Vec<SvmTagWrite>>>> =
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

#[allow(dead_code)]
pub(crate) fn queue_tag_write(channel: u32, tag: String, value: String) {
    if let Some(q) = TAG_WRITE_QUEUE.get() {
        if let Ok(mut v) = q.lock() {
            v.push(SvmTagWrite { channel, tag, value });
        }
    }
}

#[allow(dead_code)]
pub(crate) fn get_snapshot() -> Option<Arc<RwLock<ChannelSnapshot>>> {
    ENGINE_BRIDGE.get().cloned()
}

#[allow(dead_code)]
pub(crate) fn queue_write(channel: u32, value: f64) {
    if let Some(q) = WRITE_QUEUE.get() {
        if let Ok(mut v) = q.lock() {
            v.push(SvmWrite { channel, value });
        }
    }
}

// ════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Cell;

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
        tags.insert("unit".into(), TagValue::Str("\u{00b0}F".into()));
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
    // SVM integration tests — bridge communication layer
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
    // Panic safety tests
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
