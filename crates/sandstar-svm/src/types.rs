//! FFI type definitions matching sedona.h.
//!
//! These must be kept in exact sync with the C `Cell` union and `SedonaVM`
//! struct defined in `csrc/sedona.h`.

use std::ffi::c_void;
use std::os::raw::c_char;

/// Cell is a single stack unit: 32-bit int, 32-bit float, or memory pointer.
/// Matches the C `Cell` union from sedona.h (lines 288-294).
#[repr(C)]
#[derive(Copy, Clone)]
pub union Cell {
    pub ival: i32,
    pub fval: f32,
    pub aval: *mut c_void,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { ival: 0 }
    }
}

/// Native method function pointer: takes VM + params, returns Cell.
pub type NativeMethod = unsafe extern "C" fn(vm: *mut SedonaVM, params: *mut Cell) -> Cell;

/// Wide native method: returns int64 (for long/double returns).
pub type NativeMethodWide =
    unsafe extern "C" fn(vm: *mut SedonaVM, params: *mut Cell) -> i64;

/// Assert failure callback signature.
pub type OnAssertFailure = unsafe extern "C" fn(location: *const c_char, linenum: u16);

/// VM call dispatcher signature.
pub type VmCallFn = unsafe extern "C" fn(
    vm: *mut SedonaVM,
    method: u16,
    args: *mut Cell,
    argc: i32,
) -> i32;

/// SedonaVM struct — matches the C `SedonaVM` from sedona.h (lines 337-366).
///
/// Field order and types must match exactly for FFI compatibility.
#[repr(C)]
pub struct SedonaVM {
    // Memory segments
    pub code_base_addr: *const u8,
    pub code_size: usize,
    pub stack_base_addr: *mut u8,
    pub stack_max_size: usize,
    pub sp: *mut Cell,

    // Main method arguments
    pub args: *const *const c_char,
    pub args_len: i32,

    // Callbacks
    pub on_assert_failure: Option<OnAssertFailure>,

    // Results
    pub assert_successes: u32,
    pub assert_failures: u32,

    // Native method table: 2D array indexed by [kitId][methodId]
    pub native_table: *mut *mut NativeMethod,

    // VM call dispatcher
    pub call: Option<VmCallFn>,

    // Private fields
    pub data_base_addr: *mut u8,
}

// Error codes from errorcodes.h
pub const ERR_YIELD: i32 = 253;
pub const ERR_RESTART: i32 = 254;
pub const ERR_HIBERNATE: i32 = 255;
pub const ERR_STOP_BY_USER: i32 = 252;

pub const ERR_MALLOC_IMAGE: i32 = 1;
pub const ERR_MALLOC_STACK: i32 = 2;
pub const ERR_NULL_POINTER: i32 = 100;
pub const ERR_STACK_OVERFLOW: i32 = 101;
pub const ERR_MISSING_NATIVE: i32 = 12;

// Null value constants from sedona.h
pub const NULLBOOL: i32 = 2;
pub const NULLFLOAT_BITS: u32 = 0x7fc00000;

// Type IDs from sedona.h
pub const VOID_TYPE_ID: u8 = 0;
pub const BOOL_TYPE_ID: u8 = 1;
pub const BYTE_TYPE_ID: u8 = 2;
pub const SHORT_TYPE_ID: u8 = 3;
pub const INT_TYPE_ID: u8 = 4;
pub const LONG_TYPE_ID: u8 = 5;
pub const FLOAT_TYPE_ID: u8 = 6;
pub const DOUBLE_TYPE_ID: u8 = 7;
pub const BUF_TYPE_ID: u8 = 8;
