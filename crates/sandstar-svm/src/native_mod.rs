//! Centralized native method registration.
//!
//! Provides a single entry point to register all pure-Rust native method
//! implementations into a [`NativeTable`].  This is the canonical way to
//! populate the table — called by `NativeTable::with_defaults()` and
//! available for external callers who want to build a custom table.

use crate::native_table::NativeTable;

/// Register all native methods — called by the interpreter/runner.
///
/// This overwrites any existing stub entries with real Rust implementations
/// for every kit that has been ported.  Kits not yet ported retain their
/// stub entries from the initial table construction.
pub fn register_all_natives(table: &mut NativeTable) {
    // Kit 0: sys — core system methods (Sys, Str, Type, etc.)
    crate::native_sys::register_kit0_sys(table);

    // Kit 0: file — FileStore and File I/O methods
    crate::native_file::register_kit0_file(table);

    // Kit 0: component — Component lifecycle (uncomment when ready)
    // crate::native_component::register_kit0_component(table);

    // Kit 2: inet — TCP/UDP sockets, crypto (uncomment when ready)
    // crate::native_inet::register_kit2(table);

    // Kit 9: datetimeStd — doNow, doSetClock, getUtcOffset
    crate::native_datetime::register_kit9(table);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_all_natives_populates_table() {
        let mut table = NativeTable::new();
        // Pre-populate with stubs so register functions have slots to overwrite
        for id in 0..60u16 {
            table.register_stub(0, id);
        }
        for id in 0..3u16 {
            table.register_stub(9, id);
        }

        register_all_natives(&mut table);

        // Kit 0 should have real implementations (sys + file)
        assert!(
            table.implemented_count(0) >= 33,
            "expected >= 33 real kit 0 methods, got {}",
            table.implemented_count(0)
        );

        // Kit 9 should have all 3 methods implemented
        assert_eq!(table.implemented_count(9), 3);
    }
}
