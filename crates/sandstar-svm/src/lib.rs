#![allow(clippy::missing_safety_doc)]
//! Pure Rust Sedona Virtual Machine for the Sandstar engine.
//!
//! This crate provides a complete Rust implementation of the Sedona VM
//! bytecode interpreter, native method tables, and engine bridge.
//! No C code is compiled — all native methods (sys, inet, datetimeStd,
//! EacIo, shaystack) are implemented in Rust.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────┐
//! │  Sedona VM (pure Rust)                           │
//! │  - bytecode interpreter (vm_interpreter)         │
//! │  - native methods via native_table               │
//! └────────────┬─────────────────────────────────────┘
//!              │
//!    ┌─────────┴──────────────┐
//!    │   Kit 0  (native_sys)  │  sys
//!    │   Kit 2  (native_inet) │  inet
//!    │   Kit 4  (native_eacio)│  EacIo
//!    │   Kit 9  (native_datetime)│  datetimeStd
//!    │   Kit 100 (stubs)      │  shaystack
//!    └────────────────────────┘
//! ```

pub mod bridge;
pub mod types;

// Pure Rust VM modules
pub mod component_store;
pub mod image_loader;
pub mod native_component;
pub mod native_datetime;
pub mod native_eacio;
pub mod native_file;
pub mod native_inet;
pub mod native_mod;
pub mod native_serial;
pub mod native_sys;
pub mod native_table;
pub mod opcodes;
pub mod rust_runner;
pub mod sab_validator;
pub mod test_utils;
pub mod vm_config;
pub mod vm_error;
pub mod vm_interpreter;
pub mod vm_memory;
pub mod vm_stack;

pub use bridge::{
    drain_tag_writes, drain_writes, set_engine_bridge, set_tag_write_queue, set_write_queue,
    ChannelInfo, ChannelSnapshot, SvmTagWrite, SvmWrite, TagValue,
};
pub use rust_runner::RustSvmRunner;
pub use types::{Cell, SedonaVM};
