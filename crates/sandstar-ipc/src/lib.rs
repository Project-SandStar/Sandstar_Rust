//! Shared IPC types and wire protocol for Sandstar engine communication.
//!
//! Both the engine server and CLI tools depend on this crate to ensure
//! type-compatible serialization over Unix domain sockets (or named pipes on Windows).
//!
//! # Wire Protocol
//!
//! Length-prefixed bincode frames:
//! ```text
//! [4 bytes: length (u32 LE)] [N bytes: bincode-serialized payload]
//! ```

pub mod protocol;
pub mod types;

pub use protocol::{read_frame, write_frame};
pub use types::{ChannelInfo, EngineCommand, EngineResponse, PollInfo, StatusInfo};
