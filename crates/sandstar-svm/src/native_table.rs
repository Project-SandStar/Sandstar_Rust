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

        // ── Kit 3: serial (6 methods, placeholder) ────────────
        crate::native_serial::register_serial(&mut table);

        // ── Kit 4: EacIo (23 methods, slot 0 is NULL in C) ──
        crate::native_eacio::register_kit4_eacio(&mut table);

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
        crate::native_inet::register_kit2(&mut table);
        crate::native_datetime::register_kit9(&mut table);
        // Kit 3 (serial) and Kit 4 (EacIo) are registered above via
        // their dedicated register functions — no need to re-register.

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

    /// Total number of registered method slots across all kits.
    pub fn total_methods(&self) -> usize {
        self.kits.iter().map(|v| v.len()).sum()
    }

    /// Total number of non-stub (real) implementations across all kits.
    pub fn total_implemented(&self) -> usize {
        self.kits
            .iter()
            .flat_map(|v| v.iter())
            .filter(|e| !matches!(e, NativeEntry::Stub))
            .count()
    }

    // ── Sedonac-compatible aliases ──────────────────────────────

    /// Build the default native table with all known kits registered.
    ///
    /// This is the sedonac-compatible entry point — it produces the same
    /// 2D dispatch layout that sedonac generates in `nativetable.c`:
    /// `native_table[kit_id][method_id] -> function pointer`.
    ///
    /// Alias for [`with_defaults()`](Self::with_defaults).
    pub fn build_default() -> Self {
        Self::with_defaults()
    }

    // ── Traced dispatch ─────────────────────────────────────────

    /// Dispatch a normal (32-bit) native call with tracing.
    ///
    /// Logs kit_id, method_id, and kit name at trace level before calling
    /// the method.  Use this for debugging native dispatch issues.
    pub fn call_traced(
        &self,
        kit_id: u8,
        method_id: u16,
        ctx: &mut NativeContext<'_>,
        params: &[i32],
    ) -> VmResult<i32> {
        let kit_name = self.kit_name(kit_id).unwrap_or("unknown");
        let entry = self.lookup(kit_id, method_id)?;
        let entry_kind = match entry {
            NativeEntry::Normal(_) => "normal",
            NativeEntry::Wide(_) => "wide",
            NativeEntry::Stub => "stub",
        };
        tracing::trace!(
            kit_id,
            method_id,
            kit = kit_name,
            kind = entry_kind,
            "native call dispatch"
        );
        self.call(kit_id, method_id, ctx, params)
    }

    /// Dispatch a wide (64-bit) native call with tracing.
    pub fn call_wide_traced(
        &self,
        kit_id: u8,
        method_id: u16,
        ctx: &mut NativeContext<'_>,
        params: &[i32],
    ) -> VmResult<i64> {
        let kit_name = self.kit_name(kit_id).unwrap_or("unknown");
        tracing::trace!(
            kit_id,
            method_id,
            kit = kit_name,
            "native wide call dispatch"
        );
        self.call_wide(kit_id, method_id, ctx, params)
    }

    // ── Validation ──────────────────────────────────────────────

    /// Validate the native table against expected kit/method counts.
    ///
    /// Returns a list of mismatches between the registered table and the
    /// expected layout.  An empty return means the table matches exactly.
    ///
    /// `expected` is a slice of `(kit_id, kit_name, expected_method_count)`.
    pub fn validate(&self, expected: &[(u8, &str, usize)]) -> Vec<NativeTableMismatch> {
        let mut mismatches = Vec::new();

        for &(kit_id, expected_name, expected_count) in expected {
            let actual_count = self.method_count(kit_id);
            let actual_name = self.kit_name(kit_id);

            if actual_count != expected_count {
                mismatches.push(NativeTableMismatch::MethodCount {
                    kit_id,
                    kit_name: expected_name.to_string(),
                    expected: expected_count,
                    actual: actual_count,
                });
            }

            if let Some(actual) = actual_name {
                if actual != expected_name {
                    mismatches.push(NativeTableMismatch::KitName {
                        kit_id,
                        expected: expected_name.to_string(),
                        actual: actual.to_string(),
                    });
                }
            } else if actual_count > 0 {
                mismatches.push(NativeTableMismatch::MissingKitName {
                    kit_id,
                    expected: expected_name.to_string(),
                });
            }
        }

        mismatches
    }

    /// Validate against the standard sedonac kit layout.
    ///
    /// Checks that the table matches the expected kit IDs and method counts
    /// from the default nativetable.c generated by sedonac for the Sandstar
    /// scode image.
    pub fn validate_sedonac_layout(&self) -> Vec<NativeTableMismatch> {
        self.validate(&[
            (0, "sys", 60),
            (2, "inet", 17),
            (3, "serial", 6),
            (4, "EacIo", 23),
            (9, "datetimeStd", 3),
            (100, "shaystack", 28),
        ])
    }

    // ── Summary ─────────────────────────────────────────────────

    /// Return a human-readable summary of the native table contents.
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!(
            "NativeTable: {} kits, {} total method slots, {} implemented",
            self.kits.len(),
            self.total_methods(),
            self.total_implemented(),
        ));
        for (kid, methods) in self.kits.iter().enumerate() {
            if methods.is_empty() {
                continue;
            }
            let impl_count = methods
                .iter()
                .filter(|e| !matches!(e, NativeEntry::Stub))
                .count();
            let stub_count = methods.len() - impl_count;
            let name = self
                .kit_name(kid as u8)
                .unwrap_or("?");
            lines.push(format!(
                "  kit {:>3} ({:<12}): {} methods ({} impl, {} stubs)",
                kid,
                name,
                methods.len(),
                impl_count,
                stub_count,
            ));
        }
        lines.join("\n")
    }

    // ── Internal helpers ─────────────────────────────────────

    /// Format a kit label suffix like " (sys)" for error messages.
    fn kit_label(&self, kit_id: u8) -> String {
        match self.kit_name(kit_id) {
            Some(name) => format!(" ({name})"),
            None => String::new(),
        }
    }
}

// ════════════════════════════════════════════════════════════════
// NativeTableMismatch — validation result type
// ════════════════════════════════════════════════════════════════

/// A mismatch found when validating the native table against expectations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeTableMismatch {
    /// Kit has a different number of method slots than expected.
    MethodCount {
        kit_id: u8,
        kit_name: String,
        expected: usize,
        actual: usize,
    },
    /// Kit name doesn't match the expected name.
    KitName {
        kit_id: u8,
        expected: String,
        actual: String,
    },
    /// Kit has methods registered but no kit name set.
    MissingKitName {
        kit_id: u8,
        expected: String,
    },
}

impl std::fmt::Display for NativeTableMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NativeTableMismatch::MethodCount {
                kit_id,
                kit_name,
                expected,
                actual,
            } => write!(
                f,
                "kit {kit_id} ({kit_name}): expected {expected} methods, got {actual}"
            ),
            NativeTableMismatch::KitName {
                kit_id,
                expected,
                actual,
            } => write!(
                f,
                "kit {kit_id}: expected name '{expected}', got '{actual}'"
            ),
            NativeTableMismatch::MissingKitName { kit_id, expected } => write!(
                f,
                "kit {kit_id}: expected name '{expected}', but no name set"
            ),
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

// Kit 4 (EacIo) native methods are now in native_eacio.rs.
// Kit 3 (serial) native methods are now in native_serial.rs.

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
        // Kit 0 sys methods (22 from native_sys) + file methods (11 from native_file) + component methods (20) = 53+
        let count = t.implemented_count(0);
        assert!(
            count >= 53,
            "kit 0 should have at least 53 real implementations (sys + file + component), got {count}"
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

    // ── Kit 4 smoke tests (via native_eacio module) ────────────

    #[test]
    fn kit4_bool_in_point_get_returns_zero() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(
            crate::native_eacio::eacio_bool_in_point_get(&mut ctx, &[1113]).unwrap(),
            0,
        );
    }

    #[test]
    fn kit4_analog_in_point_get_returns_zero_float() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let bits = crate::native_eacio::eacio_analog_in_point_get(&mut ctx, &[1100]).unwrap();
        assert_eq!(f32::from_bits(bits as u32), 0.0);
    }

    #[test]
    fn kit4_bool_out_point_set_returns_success() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(
            crate::native_eacio::eacio_bool_out_point_set(&mut ctx, &[1113, 1]).unwrap(),
            1,
        );
    }

    #[test]
    fn kit4_eacio_resolve_channel_returns_not_found() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(
            crate::native_eacio::eacio_resolve_channel(&mut ctx, &[]).unwrap(),
            -1,
        );
    }

    #[test]
    fn kit4_eacio_get_level_returns_default() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(
            crate::native_eacio::eacio_get_level(&mut ctx, &[1113]).unwrap(),
            17,
        );
    }

    #[test]
    fn kit4_eacio_get_channel_in_returns_not_virtual() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(
            crate::native_eacio::eacio_get_channel_in(&mut ctx, &[1113]).unwrap(),
            -1,
        );
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

    // ── build_default (sedonac-compatible alias) ────────────────

    #[test]
    fn build_default_creates_table_with_all_kits() {
        let t = NativeTable::build_default();
        assert!(t.kit_count() >= 101, "should include kit 100");
        assert_eq!(t.method_count(0), 60, "kit 0 (sys) should have 60 methods");
        assert_eq!(t.method_count(2), 17, "kit 2 (inet) should have 17 methods");
        assert_eq!(t.method_count(4), 23, "kit 4 (EacIo) should have 23 methods");
        assert_eq!(t.method_count(9), 3, "kit 9 (datetimeStd) should have 3 methods");
        assert_eq!(t.method_count(100), 28, "kit 100 (shaystack) should have 28 methods");
    }

    #[test]
    fn build_default_lookup_returns_correct_entry() {
        let t = NativeTable::build_default();
        // Kit 4, method 1 should be a real implementation (boolInPoint.get)
        assert!(t.is_implemented(4, 1));
        // Kit 4, method 0 should be a stub (reserved NULL in C)
        assert!(!t.is_implemented(4, 0));
    }

    #[test]
    fn build_default_unregistered_method_returns_none() {
        let t = NativeTable::build_default();
        // Kit 0 only has 60 methods, so method 99 should fail
        assert!(t.lookup(0, 99).is_err());
    }

    #[test]
    fn build_default_call_dispatches_correctly() {
        let t = NativeTable::build_default();
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        // Kit 4, method 1 (boolInPoint.get) should return 0 with no bridge
        let result = t.call(4, 1, &mut ctx, &[1113]).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn build_default_call_unregistered_returns_error() {
        let t = NativeTable::build_default();
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        // Kit 200 doesn't exist
        let err = t.call(200, 0, &mut ctx, &[]);
        assert!(err.is_err());
        match err.unwrap_err() {
            VmError::NativeError { kit: 200, .. } => {}
            other => panic!("expected NativeError for kit 200, got {other:?}"),
        }
    }

    #[test]
    fn build_default_kit0_sys_has_expected_implementations() {
        let t = NativeTable::build_default();
        // At least 33 real implementations (sys + file + component)
        let count = t.implemented_count(0);
        assert!(
            count >= 33,
            "kit 0 should have >= 33 real methods, got {count}"
        );
    }

    #[test]
    fn build_default_kit4_eacio_has_22_implementations() {
        let t = NativeTable::build_default();
        assert_eq!(t.implemented_count(4), 22);
    }

    #[test]
    fn build_default_kit3_serial_has_6_methods() {
        let t = NativeTable::build_default();
        assert_eq!(t.method_count(3), 6);
        assert_eq!(t.implemented_count(3), 6);
    }

    // ── total_methods / total_implemented ───────────────────────

    #[test]
    fn total_methods_counts_all_slots() {
        let mut t = NativeTable::new();
        t.register_stub(0, 2); // 3 slots in kit 0
        t.register_stub(1, 4); // 5 slots in kit 1
        assert_eq!(t.total_methods(), 8);
    }

    #[test]
    fn total_implemented_excludes_stubs() {
        let mut t = NativeTable::new();
        fn f(_ctx: &mut NativeContext<'_>, _p: &[i32]) -> VmResult<i32> {
            Ok(0)
        }
        t.register_stub(0, 2);
        t.register(0, 0, f);
        assert_eq!(t.total_implemented(), 1);
    }

    #[test]
    fn build_default_total_methods_nonzero() {
        let t = NativeTable::build_default();
        assert!(t.total_methods() > 100, "should have many method slots");
        assert!(t.total_implemented() > 50, "should have many real impls");
    }

    // ── Validation ──────────────────────────────────────────────

    #[test]
    fn validate_sedonac_layout_passes_on_default() {
        let t = NativeTable::build_default();
        let mismatches = t.validate_sedonac_layout();
        assert!(
            mismatches.is_empty(),
            "default table should match sedonac layout, but got mismatches: {:?}",
            mismatches
        );
    }

    #[test]
    fn validate_detects_method_count_mismatch() {
        let mut t = NativeTable::new();
        t.set_kit_name(0, "sys");
        for id in 0..50u16 {
            t.register_stub(0, id);
        }
        let mismatches = t.validate(&[(0, "sys", 60)]);
        assert_eq!(mismatches.len(), 1);
        assert!(matches!(
            &mismatches[0],
            NativeTableMismatch::MethodCount {
                kit_id: 0,
                expected: 60,
                actual: 50,
                ..
            }
        ));
    }

    #[test]
    fn validate_detects_kit_name_mismatch() {
        let mut t = NativeTable::new();
        t.set_kit_name(0, "wrong_name");
        for id in 0..60u16 {
            t.register_stub(0, id);
        }
        let mismatches = t.validate(&[(0, "sys", 60)]);
        assert_eq!(mismatches.len(), 1);
        assert!(matches!(
            &mismatches[0],
            NativeTableMismatch::KitName { kit_id: 0, .. }
        ));
    }

    #[test]
    fn validate_detects_missing_kit_name() {
        let mut t = NativeTable::new();
        // No kit name set, but methods exist
        for id in 0..60u16 {
            t.register_stub(0, id);
        }
        let mismatches = t.validate(&[(0, "sys", 60)]);
        assert_eq!(mismatches.len(), 1);
        assert!(matches!(
            &mismatches[0],
            NativeTableMismatch::MissingKitName { kit_id: 0, .. }
        ));
    }

    #[test]
    fn mismatch_display_formatting() {
        let m = NativeTableMismatch::MethodCount {
            kit_id: 4,
            kit_name: "EacIo".into(),
            expected: 23,
            actual: 20,
        };
        let s = m.to_string();
        assert!(s.contains("EacIo"));
        assert!(s.contains("23"));
        assert!(s.contains("20"));
    }

    // ── Summary ─────────────────────────────────────────────────

    #[test]
    fn summary_includes_kit_info() {
        let t = NativeTable::build_default();
        let s = t.summary();
        assert!(s.contains("sys"), "summary should mention sys kit");
        assert!(s.contains("EacIo"), "summary should mention EacIo kit");
        assert!(s.contains("NativeTable:"), "summary should have header");
    }

    // ── Traced dispatch ─────────────────────────────────────────

    #[test]
    fn call_traced_dispatches_correctly() {
        let t = NativeTable::build_default();
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        // Kit 4, method 1 (boolInPoint.get) via traced path
        let result = t.call_traced(4, 1, &mut ctx, &[1113]).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn call_traced_returns_error_for_missing() {
        let t = NativeTable::new();
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert!(t.call_traced(255, 0, &mut ctx, &[]).is_err());
    }

    #[test]
    fn call_wide_traced_dispatches_correctly() {
        let t = NativeTable::build_default();
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        // Kit 9, method 0 (doNow) is a wide method — should succeed
        let result = t.call_wide_traced(9, 0, &mut ctx, &[]);
        assert!(result.is_ok());
    }
}
