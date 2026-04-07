#![allow(clippy::missing_safety_doc)]
//! Sedona Virtual Machine FFI bridge for the Sandstar Rust engine.
//!
//! This crate compiles the Sedona VM bytecode interpreter (`vm.c`) and its
//! standard native method libraries via the `cc` crate, then provides Rust
//! FFI bindings and a high-level runner API.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────┐
//! │  Sedona VM (vm.c)                                │
//! │  - bytecode interpreter                          │
//! │  - calls native methods via nativeTable[kit][id] │
//! └────────────┬─────────────────────────────────────┘
//!              │
//!    ┌─────────┴─────────┐
//!    │   Kit 0,2,9 (C)   │  sys, inet, datetimeStd
//!    │   Kit 4 (Rust)    │  EacIo — direct engine access
//!    │   Kit 100 (Rust)  │  shaystack — stubs
//!    └───────────────────┘
//! ```

pub mod bridge;
pub mod ffi;
pub mod runner;
pub mod types;

// Pure Rust VM modules (Phase A)
pub mod opcodes;
pub mod vm_error;
pub mod image_loader;
pub mod vm_memory;
pub mod vm_config;
pub mod component_store;
pub mod native_table;
pub mod native_sys;
pub mod native_file;
pub mod test_utils;
pub mod native_datetime;
pub mod native_component;
pub mod native_inet;
pub mod native_mod;
pub mod native_serial;
pub mod native_eacio;
// Future phases:
pub mod vm_stack;
pub mod vm_interpreter;
pub mod rust_runner;

pub use bridge::{
    set_engine_bridge, set_write_queue, set_tag_write_queue,
    drain_writes, drain_tag_writes,
    ChannelInfo, ChannelSnapshot, SvmWrite, SvmTagWrite, TagValue,
};
pub use runner::SvmRunner;
pub use types::{Cell, SedonaVM};
