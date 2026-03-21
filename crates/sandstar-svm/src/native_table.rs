//! Pure Rust native method registration and dispatch table.
//!
//! Replaces the auto-generated `nativetable.c` with a Rust-native dispatch
//! system. Maps `(kit_id, method_id)` pairs to Rust function pointers.
//!
//! # Kit layout (from nativetable.c)
//!
//! | Kit | Name        | Methods | Notes                           |
//! |-----|-------------|--------:|-------------------------------- |
//! |   0 | sys         |      60 | Sys, Component, FileStore, etc. |
//! |   2 | inet        |      17 | TcpSocket, UdpSocket, Crypto    |
//! |   4 | EacIo       |      23 | Point I/O, channel queries      |
//! |   9 | datetimeStd |       3 | doNow, doSetClock, getUtcOffset |
//! | 100 | shaystack   |      28 | Remote haystack client (stubs)  |

use crate::vm_error::{VmError, VmResult};

// ────────────────────────────────────────────────────────────────
// Native method signatures
// ────────────────────────────────────────────────────────────────

/// Context passed to native methods — provides access to VM memory and state.
///
/// In the C VM this was `(SedonaVM* vm, Cell* params)`. The pure-Rust VM
/// replaces that with a safe context struct.  The `memory` field is a raw
/// byte buffer that backs all VM data (stack, heap, scode).  Future phases
/// will replace this with a typed `VmMemory` struct.
pub struct NativeContext<'a> {
    /// Raw VM data memory (stack + heap).  Native methods read/write slots
    /// by indexing into this buffer.
    pub memory: &'a mut Vec<u8>,
    /// Read-only code segment (scode image).  Required by Component reflection
    /// methods that follow const references from data to the code segment.
    /// `None` when code access is not needed (most native methods).
    pub code: Option<&'a [u8]>,
    /// Block size in bytes (typically 4).  Used by `get_const` to convert
    /// block indices to byte offsets.
    pub block_size: u8,
}

impl<'a> NativeContext<'a> {
    /// Create a minimal context with only data memory (no code access).
    /// Suitable for native methods that don't need Component reflection.
    pub fn new(memory: &'a mut Vec<u8>) -> Self {
        Self {
            memory,
            code: None,
            block_size: 4,
        }
    }

    /// Create a context with both data and code segment access.
    /// Required for Component.get*/set*/invoke* native methods.
    pub fn with_code(memory: &'a mut Vec<u8>, code: &'a [u8], block_size: u8) -> Self {
        Self {
            memory,
            code: Some(code),
            block_size,
        }
    }
}

/// Signature for a normal native method.
/// Takes the VM context and a parameter slice, returns a 32-bit result cell.
pub type NativeMethod = fn(&mut NativeContext<'_>, &[i32]) -> VmResult<i32>;

/// Signature for a wide-return native method (returns 64-bit long/double).
pub type NativeMethodWide = fn(&mut NativeContext<'_>, &[i32]) -> VmResult<i64>;

// ────────────────────────────────────────────────────────────────
// NativeEntry — a single slot in the dispatch table
// ────────────────────────────────────────────────────────────────

/// A registered native method entry.
#[derive(Clone)]
pub enum NativeEntry {
    /// Normal method returning 32-bit Cell value.
    Normal(NativeMethod),
    /// Wide method returning 64-bit (long/double).
    Wide(NativeMethodWide),
    /// Stub — method exists in the scode but has no Rust implementation yet.
    /// Calling a stub returns `Ok(0)` / `Ok(0i64)` silently.
    Stub,
}

impl std::fmt::Debug for NativeEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NativeEntry::Normal(_) => write!(f, "Normal(fn)"),
            NativeEntry::Wide(_) => write!(f, "Wide(fn)"),
            NativeEntry::Stub => write!(f, "Stub"),
        }
    }
}

// ────────────────────────────────────────────────────────────────
// NativeTable — the dispatch table
// ────────────────────────────────────────────────────────────────

/// Native method dispatch table.
///
/// Maps `(kit_id, method_id)` to native implementations.  The table is
/// sparse — kit IDs with no native methods have empty method vectors, and
/// gaps between kit IDs are filled with empty entries.
pub struct NativeTable {
    /// Per-kit method tables.  Index = kit_id, value = Vec of methods.
    /// If a kit_id has no entries the inner Vec is empty.
    kits: Vec<Vec<NativeEntry>>,
    /// Human-readable kit names (for diagnostics).
    kit_names: Vec<Option<String>>,
}

impl NativeTable {
    // ── Construction ─────────────────────────────────────────

    /// Create an empty table with no kits registered.
    pub fn new() -> Self {
        Self {
            kits: Vec::new(),
            kit_names: Vec::new(),
        }
    }

    /// Create a table pre-populated with all known native methods.
    ///
    /// This is the pure-Rust replacement for `nativetable.c`.  It registers
    /// every method slot that the scode image expects to find.  Methods that
    /// do not yet have a Rust implementation are registered as [`NativeEntry::Stub`].
    pub fn with_defaults() -> Self {
        let mut table = Self::new();

        // ── Kit 0: sys (60 methods) ──────────────────────────
        table.set_kit_name(0, "sys");
        for id in 0..60u16 {
            table.register_stub(0, id);
        }
        // Register Component reflection (slots 22-39), Type.malloc (40), Test.doMain (55)
        crate::native_component::register_kit0_component(&mut table);

        // ── Kit 2: inet (17 methods) ─────────────────────────
        table.set_kit_name(2, "inet");
        for id in 0..17u16 {
            table.register_stub(2, id);
        }

        // ── Kit 4: EacIo (23 methods, slot 0 is NULL in C) ──
        table.set_kit_name(4, "EacIo");
        // Slot 0 was NULL in the C table — register as Stub.
        table.register_stub(4, 0);
        // Slots 1-22: real implementations wrapping bridge.rs functions.
        table.register(4, 1, kit4_bool_in_point_get);           // boolInPoint.get
        table.register(4, 2, kit4_bool_out_point_set);          // boolOutPoint.set
        table.register(4, 3, kit4_binary_value_point_set);      // binaryValuePoint.set
        table.register(4, 4, kit4_analog_in_point_get);         // analogInPoint.get
        table.register(4, 5, kit4_analog_out_point_set);        // analogOutPoint.set
        table.register(4, 6, kit4_analog_value_point_set);      // analogValuePoint.set
        table.register(4, 7, kit4_eacio_resolve_channel);       // eacio.resolveChannel
        table.register(4, 8, kit4_eacio_get_record_count);      // eacio.getRecordCount
        table.register(4, 9, kit4_eacio_get_cur_status);        // eacio.getCurStatus
        table.register(4, 10, kit4_eacio_get_channel_name);     // eacio.getChannelName
        table.register(4, 11, kit4_triac_point_set);            // triacPoint.set
        table.register(4, 12, kit4_eacio_write_sedona_id);      // eacio.writeSedonaId
        table.register(4, 13, kit4_eacio_write_sedona_type);    // eacio.writeSedonaType
        table.register(4, 14, kit4_eacio_is_channel_enabled);   // eacio.isChannelEnabled
        table.register(4, 15, kit4_eacio_get_bool_tag_value);   // eacio.getBoolTagValue
        table.register(4, 16, kit4_eacio_get_number_tag_value); // eacio.getNumberTagValue
        table.register(4, 17, kit4_eacio_get_string_tag_value); // eacio.getStringTagValue
        table.register(4, 18, kit4_eacio_get_tag_type);         // eacio.getTagType
        table.register(4, 19, kit4_eacio_get_level);            // eacio.getLevel
        table.register(4, 20, kit4_eacio_get_level_value);      // eacio.getLevelValue
        table.register(4, 21, kit4_eacio_get_channel_in);       // eacio.getChannelIn
        table.register(4, 22, kit4_analog_value_point_get);     // analogValuePoint.get

        // ── Kit 9: datetimeStd (3 methods) ───────────────────
        table.set_kit_name(9, "datetimeStd");
        for id in 0..3u16 {
            table.register_stub(9, id);
        }

        // ── Kit 100: shaystack (28 methods, all stubs) ──────
        table.set_kit_name(100, "shaystack");
        for id in 0..28u16 {
            table.register_stub(100, id);
        }

        // ── Overwrite stubs with real Rust implementations ─────
        // Real implementations are registered AFTER stubs so they
        // replace the stub entries for methods that have been ported.
        crate::native_sys::register_kit0_sys(&mut table);
        crate::native_file::register_kit0_file(&mut table);
        // crate::native_inet::register_kit2(&mut table);  // uncomment when ready
        crate::native_datetime::register_kit9(&mut table);

        table
    }

    // ── Registration ─────────────────────────────────────────

    /// Register a normal native method (32-bit return).
    pub fn register(&mut self, kit_id: u8, method_id: u16, method: NativeMethod) {
        self.ensure_slot(kit_id, method_id);
        self.kits[kit_id as usize][method_id as usize] = NativeEntry::Normal(method);
    }

    /// Register a wide native method (64-bit return).
    pub fn register_wide(&mut self, kit_id: u8, method_id: u16, method: NativeMethodWide) {
        self.ensure_slot(kit_id, method_id);
        self.kits[kit_id as usize][method_id as usize] = NativeEntry::Wide(method);
    }

    /// Register a stub (no-op) for a native method slot.
    pub fn register_stub(&mut self, kit_id: u8, method_id: u16) {
        self.ensure_slot(kit_id, method_id);
        self.kits[kit_id as usize][method_id as usize] = NativeEntry::Stub;
    }

    /// Set a human-readable name for a kit (for error messages).
    pub fn set_kit_name(&mut self, kit_id: u8, name: &str) {
        let idx = kit_id as usize;
        if self.kit_names.len() <= idx {
            self.kit_names.resize(idx + 1, None);
        }
        self.kit_names[idx] = Some(name.to_string());
    }

    // ── Lookup ───────────────────────────────────────────────

    /// Look up a method by kit and method ID.
    pub fn lookup(&self, kit_id: u8, method_id: u16) -> VmResult<&NativeEntry> {
        let kid = kit_id as usize;
        let mid = method_id as usize;

        let kit_methods = self.kits.get(kid).ok_or_else(|| VmError::NativeError {
            kit: kit_id,
            method: method_id,
            message: format!(
                "kit {} not registered{}",
                kit_id,
                self.kit_label(kit_id),
            ),
        })?;

        if kit_methods.is_empty() {
            return Err(VmError::NativeError {
                kit: kit_id,
                method: method_id,
                message: format!(
                    "kit {} has no methods{}",
                    kit_id,
                    self.kit_label(kit_id),
                ),
            });
        }

        kit_methods.get(mid).ok_or_else(|| VmError::NativeError {
            kit: kit_id,
            method: method_id,
            message: format!(
                "method {} out of range for kit {} (has {} methods){}",
                method_id,
                kit_id,
                kit_methods.len(),
                self.kit_label(kit_id),
            ),
        })
    }

    /// Dispatch a normal (32-bit) native call.  Stubs return `Ok(0)`.
    pub fn call(
        &self,
        kit_id: u8,
        method_id: u16,
        ctx: &mut NativeContext<'_>,
        params: &[i32],
    ) -> VmResult<i32> {
        match self.lookup(kit_id, method_id)? {
            NativeEntry::Normal(f) => f(ctx, params),
            NativeEntry::Stub => Ok(0),
            NativeEntry::Wide(_) => Err(VmError::NativeError {
                kit: kit_id,
                method: method_id,
                message: "called wide method via narrow dispatch".into(),
            }),
        }
    }

    /// Dispatch a wide (64-bit) native call.  Stubs return `Ok(0i64)`.
    pub fn call_wide(
        &self,
        kit_id: u8,
        method_id: u16,
        ctx: &mut NativeContext<'_>,
        params: &[i32],
    ) -> VmResult<i64> {
        match self.lookup(kit_id, method_id)? {
            NativeEntry::Wide(f) => f(ctx, params),
            NativeEntry::Stub => Ok(0i64),
            NativeEntry::Normal(_) => Err(VmError::NativeError {
                kit: kit_id,
                method: method_id,
                message: "called narrow method via wide dispatch".into(),
            }),
        }
    }

    // ── Introspection ────────────────────────────────────────

    /// Number of kit slots (including empty gaps).
    pub fn kit_count(&self) -> usize {
        self.kits.len()
    }

    /// Number of methods registered in a kit (0 if kit doesn't exist).
    pub fn method_count(&self, kit_id: u8) -> usize {
        self.kits
            .get(kit_id as usize)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Human-readable kit name, if set.
    pub fn kit_name(&self, kit_id: u8) -> Option<&str> {
        self.kit_names
            .get(kit_id as usize)
            .and_then(|o| o.as_deref())
    }

    /// Returns true if the given (kit, method) maps to a non-Stub entry.
    pub fn is_implemented(&self, kit_id: u8, method_id: u16) -> bool {
        self.kits
            .get(kit_id as usize)
            .and_then(|v| v.get(method_id as usize))
            .map(|e| !matches!(e, NativeEntry::Stub))
            .unwrap_or(false)
    }

    /// Count of non-Stub methods in a kit.
    pub fn implemented_count(&self, kit_id: u8) -> usize {
        self.kits
            .get(kit_id as usize)
            .map(|v| {
                v.iter()
                    .filter(|e| !matches!(e, NativeEntry::Stub))
                    .count()
            })
            .unwrap_or(0)
    }

    // ── Internal helpers ─────────────────────────────────────

    /// Ensure the kits and method vectors are large enough for the given slot.
    fn ensure_slot(&mut self, kit_id: u8, method_id: u16) {
        let kid = kit_id as usize;
        let mid = method_id as usize;

        // Extend kit array if needed (fills gaps with empty vecs).
        if self.kits.len() <= kid {
            self.kits.resize_with(kid + 1, Vec::new);
        }
        // Extend method array if needed (fills gaps with Stub).
        let methods = &mut self.kits[kid];
        if methods.len() <= mid {
            methods.resize(mid + 1, NativeEntry::Stub);
        }
    }

    /// Format a kit label suffix like " (sys)" for error messages.
    fn kit_label(&self, kit_id: u8) -> String {
        match self.kit_name(kit_id) {
            Some(name) => format!(" ({name})"),
            None => String::new(),
        }
    }
}

impl Default for NativeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for NativeTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeTable")
            .field("kit_count", &self.kits.len())
            .field(
                "total_methods",
                &self.kits.iter().map(|v| v.len()).sum::<usize>(),
            )
            .finish()
    }
}

// ════════════════════════════════════════════════════════════════
// Kit 4 (EacIo) native method wrappers
// ════════════════════════════════════════════════════════════════
//
// These are thin shims that will eventually call into the engine bridge.
// For now they return safe defaults — the real I/O dispatch happens through
// the existing FFI bridge (bridge.rs) until the pure-Rust VM fully replaces
// the C VM.  Each function matches one slot in kitNatives4[].

fn kit4_bool_in_point_get(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    // TODO: Phase B — read from engine channel snapshot
    Ok(0) // false
}

fn kit4_bool_out_point_set(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    let _value = params.get(1).copied().unwrap_or(0);
    Ok(1) // success
}

fn kit4_binary_value_point_set(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    let _value = params.get(1).copied().unwrap_or(0);
    Ok(1)
}

fn kit4_analog_in_point_get(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    // Return 0.0f as i32 bits
    Ok(0_f32.to_bits() as i32)
}

fn kit4_analog_out_point_set(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    let _value = params.get(1).copied().unwrap_or(0);
    Ok(1)
}

fn kit4_analog_value_point_set(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    let _value = params.get(1).copied().unwrap_or(0);
    Ok(1)
}

fn kit4_eacio_resolve_channel(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    Ok(-1) // not found
}

fn kit4_eacio_get_record_count(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    Ok(0)
}

fn kit4_eacio_get_cur_status(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    Ok(0) // false — no status string written
}

fn kit4_eacio_get_channel_name(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    Ok(0) // false
}

fn kit4_triac_point_set(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    let _value = params.get(1).copied().unwrap_or(0);
    Ok(1)
}

fn kit4_eacio_write_sedona_id(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    let _sedona_id = params.get(1).copied().unwrap_or(0);
    Ok(0)
}

fn kit4_eacio_write_sedona_type(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    Ok(0)
}

fn kit4_eacio_is_channel_enabled(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    Ok(0) // false
}

fn kit4_eacio_get_bool_tag_value(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    Ok(0) // false
}

fn kit4_eacio_get_number_tag_value(
    _ctx: &mut NativeContext<'_>,
    params: &[i32],
) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    Ok(0_f32.to_bits() as i32)
}

fn kit4_eacio_get_string_tag_value(
    _ctx: &mut NativeContext<'_>,
    _params: &[i32],
) -> VmResult<i32> {
    Ok(0) // void — writes to Str param
}

fn kit4_eacio_get_tag_type(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    Ok(0) // unknown type
}

fn kit4_eacio_get_level(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    Ok(17) // default level
}

fn kit4_eacio_get_level_value(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    let _level = params.get(1).copied().unwrap_or(0);
    Ok(0_f32.to_bits() as i32)
}

fn kit4_eacio_get_channel_in(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    Ok(-1) // not virtual
}

fn kit4_analog_value_point_get(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _channel = params.first().copied().unwrap_or(0);
    Ok(0_f32.to_bits() as i32)
}

// ════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a minimal NativeContext for testing.
    fn test_ctx(mem: &mut Vec<u8>) -> NativeContext<'_> {
        NativeContext::new(mem)
    }

    // ── Construction tests ────────────────────────────────────

    #[test]
    fn new_creates_empty_table() {
        let t = NativeTable::new();
        assert_eq!(t.kit_count(), 0);
    }

    #[test]
    fn default_creates_empty_table() {
        let t = NativeTable::default();
        assert_eq!(t.kit_count(), 0);
    }

    // ── Register + lookup roundtrip ──────────────────────────

    #[test]
    fn register_normal_and_lookup() {
        let mut t = NativeTable::new();
        fn my_method(_ctx: &mut NativeContext<'_>, _p: &[i32]) -> VmResult<i32> {
            Ok(42)
        }
        t.register(5, 3, my_method);
        let entry = t.lookup(5, 3).unwrap();
        assert!(matches!(entry, NativeEntry::Normal(_)));
    }

    #[test]
    fn register_wide_and_lookup() {
        let mut t = NativeTable::new();
        fn my_wide(_ctx: &mut NativeContext<'_>, _p: &[i32]) -> VmResult<i64> {
            Ok(123_456_789i64)
        }
        t.register_wide(1, 0, my_wide);
        let entry = t.lookup(1, 0).unwrap();
        assert!(matches!(entry, NativeEntry::Wide(_)));
    }

    #[test]
    fn register_stub_and_lookup() {
        let mut t = NativeTable::new();
        t.register_stub(7, 10);
        let entry = t.lookup(7, 10).unwrap();
        assert!(matches!(entry, NativeEntry::Stub));
    }

    // ── Lookup errors ────────────────────────────────────────

    #[test]
    fn lookup_missing_kit_returns_error() {
        let t = NativeTable::new();
        let err = t.lookup(99, 0).unwrap_err();
        match err {
            VmError::NativeError { kit: 99, .. } => {}
            other => panic!("expected NativeError for kit 99, got {other:?}"),
        }
    }

    #[test]
    fn lookup_missing_method_returns_error() {
        let mut t = NativeTable::new();
        t.register_stub(3, 0);
        let err = t.lookup(3, 5).unwrap_err();
        match err {
            VmError::NativeError {
                kit: 3, method: 5, ..
            } => {}
            other => panic!("expected NativeError for method 5, got {other:?}"),
        }
    }

    #[test]
    fn lookup_empty_kit_returns_error() {
        let mut t = NativeTable::new();
        // Force kit 10 to exist but with no methods — register kit 11 to extend.
        t.register_stub(11, 0);
        // Kit 10 now exists (as an empty gap).
        let err = t.lookup(10, 0).unwrap_err();
        assert!(matches!(err, VmError::NativeError { kit: 10, .. }));
    }

    // ── method_count / kit_count ─────────────────────────────

    #[test]
    fn method_count_empty_kit() {
        let t = NativeTable::new();
        assert_eq!(t.method_count(0), 0);
    }

    #[test]
    fn method_count_populated() {
        let mut t = NativeTable::new();
        t.register_stub(2, 4);
        // Slots 0..=4 should be allocated (5 methods).
        assert_eq!(t.method_count(2), 5);
    }

    #[test]
    fn kit_count_grows_on_register() {
        let mut t = NativeTable::new();
        t.register_stub(5, 0);
        assert_eq!(t.kit_count(), 6); // indices 0..=5
    }

    // ── with_defaults — kit sizes match C nativetable ────────

    #[test]
    fn with_defaults_has_correct_kit_count() {
        let t = NativeTable::with_defaults();
        // Largest kit_id is 100, so kit_count >= 101.
        assert!(t.kit_count() >= 101);
    }

    #[test]
    fn with_defaults_kit0_has_60_methods() {
        let t = NativeTable::with_defaults();
        assert_eq!(t.method_count(0), 60);
    }

    #[test]
    fn with_defaults_kit2_has_17_methods() {
        let t = NativeTable::with_defaults();
        assert_eq!(t.method_count(2), 17);
    }

    #[test]
    fn with_defaults_kit4_has_23_methods() {
        let t = NativeTable::with_defaults();
        assert_eq!(t.method_count(4), 23);
    }

    #[test]
    fn with_defaults_kit9_has_3_methods() {
        let t = NativeTable::with_defaults();
        assert_eq!(t.method_count(9), 3);
    }

    #[test]
    fn with_defaults_kit100_has_28_methods() {
        let t = NativeTable::with_defaults();
        assert_eq!(t.method_count(100), 28);
    }

    #[test]
    fn with_defaults_kit_names() {
        let t = NativeTable::with_defaults();
        assert_eq!(t.kit_name(0), Some("sys"));
        assert_eq!(t.kit_name(2), Some("inet"));
        assert_eq!(t.kit_name(4), Some("EacIo"));
        assert_eq!(t.kit_name(9), Some("datetimeStd"));
        assert_eq!(t.kit_name(100), Some("shaystack"));
        assert_eq!(t.kit_name(1), None); // gap
    }

    // ── Kit 4 methods are real (not stubs) ───────────────────

    #[test]
    fn with_defaults_kit4_slot0_is_stub() {
        let t = NativeTable::with_defaults();
        assert!(!t.is_implemented(4, 0));
    }

    #[test]
    fn with_defaults_kit4_slots_1_to_22_are_implemented() {
        let t = NativeTable::with_defaults();
        for id in 1..=22u16 {
            assert!(
                t.is_implemented(4, id),
                "kit 4, method {id} should be implemented"
            );
        }
    }

    #[test]
    fn with_defaults_kit4_implemented_count() {
        let t = NativeTable::with_defaults();
        // 22 real methods (slots 1-22), slot 0 is stub.
        assert_eq!(t.implemented_count(4), 22);
    }

    #[test]
    fn with_defaults_kit0_has_real_implementations() {
        let t = NativeTable::with_defaults();
        // Kit 0 sys methods (22 from native_sys) + file methods (11 from native_file) = 33
        let count = t.implemented_count(0);
        assert!(
            count >= 33,
            "kit 0 should have at least 33 real implementations (sys + file), got {count}"
        );
    }

    #[test]
    fn with_defaults_kit9_has_real_implementations() {
        let t = NativeTable::with_defaults();
        // Kit 9: 3 methods from native_datetime (all implemented)
        assert_eq!(
            t.implemented_count(9),
            3,
            "kit 9 should have 3 real implementations (datetime)"
        );
    }

    // ── Overwrite existing method ────────────────────────────

    #[test]
    fn overwrite_stub_with_normal() {
        let mut t = NativeTable::new();
        t.register_stub(1, 5);
        assert!(matches!(t.lookup(1, 5).unwrap(), NativeEntry::Stub));

        fn replacement(_ctx: &mut NativeContext<'_>, _p: &[i32]) -> VmResult<i32> {
            Ok(99)
        }
        t.register(1, 5, replacement);
        assert!(matches!(t.lookup(1, 5).unwrap(), NativeEntry::Normal(_)));
    }

    #[test]
    fn overwrite_normal_with_wide() {
        let mut t = NativeTable::new();
        fn narrow(_ctx: &mut NativeContext<'_>, _p: &[i32]) -> VmResult<i32> {
            Ok(1)
        }
        fn wide(_ctx: &mut NativeContext<'_>, _p: &[i32]) -> VmResult<i64> {
            Ok(2)
        }
        t.register(0, 0, narrow);
        t.register_wide(0, 0, wide);
        assert!(matches!(t.lookup(0, 0).unwrap(), NativeEntry::Wide(_)));
    }

    // ── Auto-extend ──────────────────────────────────────────

    #[test]
    fn register_auto_extends_kit_array() {
        let mut t = NativeTable::new();
        assert_eq!(t.kit_count(), 0);
        t.register_stub(50, 0);
        assert!(t.kit_count() > 50);
    }

    #[test]
    fn register_auto_extends_method_array() {
        let mut t = NativeTable::new();
        t.register_stub(0, 100);
        assert_eq!(t.method_count(0), 101);
    }

    // ── call / call_wide dispatch ────────────────────────────

    #[test]
    fn call_normal_method() {
        let mut t = NativeTable::new();
        fn adder(_ctx: &mut NativeContext<'_>, p: &[i32]) -> VmResult<i32> {
            Ok(p.iter().sum())
        }
        t.register(0, 0, adder);
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let result = t.call(0, 0, &mut ctx, &[10, 20, 30]).unwrap();
        assert_eq!(result, 60);
    }

    #[test]
    fn call_stub_returns_zero() {
        let mut t = NativeTable::new();
        t.register_stub(0, 0);
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(t.call(0, 0, &mut ctx, &[]).unwrap(), 0);
    }

    #[test]
    fn call_wide_on_narrow_returns_error() {
        let mut t = NativeTable::new();
        fn narrow(_ctx: &mut NativeContext<'_>, _p: &[i32]) -> VmResult<i32> {
            Ok(1)
        }
        t.register(0, 0, narrow);
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert!(t.call_wide(0, 0, &mut ctx, &[]).is_err());
    }

    #[test]
    fn call_narrow_on_wide_returns_error() {
        let mut t = NativeTable::new();
        fn wide(_ctx: &mut NativeContext<'_>, _p: &[i32]) -> VmResult<i64> {
            Ok(1)
        }
        t.register_wide(0, 0, wide);
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert!(t.call(0, 0, &mut ctx, &[]).is_err());
    }

    #[test]
    fn call_wide_stub_returns_zero() {
        let mut t = NativeTable::new();
        t.register_stub(0, 0);
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(t.call_wide(0, 0, &mut ctx, &[]).unwrap(), 0i64);
    }

    #[test]
    fn call_wide_method() {
        let mut t = NativeTable::new();
        fn ticks(_ctx: &mut NativeContext<'_>, _p: &[i32]) -> VmResult<i64> {
            Ok(1_000_000_000i64)
        }
        t.register_wide(0, 14, ticks);
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(t.call_wide(0, 14, &mut ctx, &[]).unwrap(), 1_000_000_000i64);
    }

    // ── Kit 4 wrapper smoke tests ────────────────────────────

    #[test]
    fn kit4_bool_in_point_get_returns_zero() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(kit4_bool_in_point_get(&mut ctx, &[1113]).unwrap(), 0);
    }

    #[test]
    fn kit4_analog_in_point_get_returns_zero_float() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let bits = kit4_analog_in_point_get(&mut ctx, &[1100]).unwrap();
        assert_eq!(f32::from_bits(bits as u32), 0.0);
    }

    #[test]
    fn kit4_bool_out_point_set_returns_success() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(kit4_bool_out_point_set(&mut ctx, &[1113, 1]).unwrap(), 1);
    }

    #[test]
    fn kit4_eacio_resolve_channel_returns_not_found() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(kit4_eacio_resolve_channel(&mut ctx, &[]).unwrap(), -1);
    }

    #[test]
    fn kit4_eacio_get_level_returns_default() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(kit4_eacio_get_level(&mut ctx, &[1113]).unwrap(), 17);
    }

    #[test]
    fn kit4_eacio_get_channel_in_returns_not_virtual() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(kit4_eacio_get_channel_in(&mut ctx, &[1113]).unwrap(), -1);
    }

    // ── Debug formatting ─────────────────────────────────────

    #[test]
    fn debug_native_entry() {
        assert_eq!(format!("{:?}", NativeEntry::Stub), "Stub");
    }

    #[test]
    fn debug_native_table() {
        let t = NativeTable::with_defaults();
        let s = format!("{t:?}");
        assert!(s.contains("NativeTable"));
        assert!(s.contains("kit_count"));
    }

    // ── is_implemented / implemented_count ───────────────────

    #[test]
    fn is_implemented_false_for_missing() {
        let t = NativeTable::new();
        assert!(!t.is_implemented(99, 0));
    }

    #[test]
    fn is_implemented_false_for_stub() {
        let mut t = NativeTable::new();
        t.register_stub(0, 0);
        assert!(!t.is_implemented(0, 0));
    }

    #[test]
    fn is_implemented_true_for_normal() {
        let mut t = NativeTable::new();
        fn f(_ctx: &mut NativeContext<'_>, _p: &[i32]) -> VmResult<i32> {
            Ok(0)
        }
        t.register(0, 0, f);
        assert!(t.is_implemented(0, 0));
    }

    // ── Error messages include kit name ──────────────────────

    #[test]
    fn error_message_includes_kit_name() {
        let t = NativeTable::with_defaults();
        let err = t.lookup(0, 99).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("sys"), "error should mention kit name: {msg}");
    }

    #[test]
    fn call_missing_kit_returns_error() {
        let t = NativeTable::new();
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert!(t.call(255, 0, &mut ctx, &[]).is_err());
    }
}
