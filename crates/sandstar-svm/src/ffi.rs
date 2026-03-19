//! Raw FFI bindings to the Sedona VM C functions defined in vm.c.

use crate::types::{Cell, SedonaVM};

extern "C" {
    /// Run the VM from its entry point (main method).
    /// Returns 0 on success, or an error code from errorcodes.h.
    pub fn vmRun(vm: *mut SedonaVM) -> i32;

    /// Resume VM execution after yield/hibernate.
    /// Returns 0 on success, or an error code.
    pub fn vmResume(vm: *mut SedonaVM) -> i32;

    /// Call a specific method by block index.
    pub fn vmCall(
        vm: *mut SedonaVM,
        method: u16,
        args: *mut Cell,
        argc: i32,
    ) -> i32;

    /// Signal the VM to stop (sets global gStopByUser flag).
    pub fn stopVm();

    // Cell constants defined in vm.c
    pub static zeroCell: Cell;
    pub static oneCell: Cell;
    pub static negOneCell: Cell;
}
