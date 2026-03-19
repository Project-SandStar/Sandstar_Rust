//! IPC command and response types.
//!
//! These enums define every operation the engine server supports.
//! Both server and CLI serialize/deserialize these via bincode.

use serde::{Deserialize, Serialize};

/// Commands sent from CLI tools to the engine server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EngineCommand {
    /// Gracefully shut down the engine.
    Shutdown,

    /// Read a channel value through the full pipeline.
    ReadChannel { channel: u32 },

    /// Write a value to an output channel at a priority level.
    WriteChannel { channel: u32, value: f64, level: u8 },

    /// Relinquish a priority level (set to null).
    RelinquishLevel { channel: u32, level: u8 },

    /// Get the 17-level priority array for a channel.
    GetWriteLevels { channel: u32 },

    /// Convert a raw value for a channel (without writing to hardware).
    ConvertValue { channel: u32, raw: f64 },

    /// List all configured channels.
    ListChannels,

    /// List all loaded lookup tables.
    ListTables,

    /// List all polled channels.
    ListPolls,

    /// Get engine status (uptime, channel count, poll count, etc.).
    Status,

    /// Trigger a single poll cycle immediately.
    PollNow,

    /// Reload configuration from disk.
    ReloadConfig,

    /// Query channel value history from the ring buffer.
    GetHistory { channel: u32, since_secs: u64, limit: usize },
}

/// Responses sent from the engine server back to CLI tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EngineResponse {
    /// Generic success acknowledgment.
    Ok,

    /// A single channel value reading.
    Value {
        channel: u32,
        status: String,
        raw: f64,
        cur: f64,
    },

    /// List of channels.
    Channels(Vec<ChannelInfo>),

    /// List of tables.
    Tables(Vec<String>),

    /// List of polled channels.
    Polls(Vec<PollInfo>),

    /// Engine status information.
    Status(StatusInfo),

    /// 17-level priority array for a channel.
    WriteLevels(Vec<WriteLevelInfo>),

    /// Channel value history.
    History(Vec<HistoryEntry>),

    /// Error message.
    Error(String),
}

/// Summary info for a single channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub id: u32,
    pub label: String,
    pub channel_type: String,
    pub direction: String,
    pub enabled: bool,
    pub status: String,
    pub cur: f64,
    pub raw: f64,
}

/// Summary info for a polled channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollInfo {
    pub channel: u32,
    pub last_cur: f64,
    pub last_status: String,
}

/// Engine status summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    pub uptime_secs: u64,
    pub channel_count: usize,
    pub poll_count: usize,
    pub table_count: usize,
    pub poll_interval_ms: u64,
}

/// A single row in the 17-level priority array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteLevelInfo {
    pub level: u8,
    pub level_dis: String,
    pub val: Option<f64>,
    pub who: String,
}

/// A single history entry from the ring buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub ts: u64,
    pub cur: f64,
    pub raw: f64,
    pub status: String,
}

// --- Helpers for converting engine types to IPC types ---

impl ChannelInfo {
    pub fn from_engine(ch: &sandstar_engine::channel::Channel) -> Self {
        Self {
            id: ch.id,
            label: ch.label.clone(),
            channel_type: format!("{:?}", ch.channel_type),
            direction: format!("{:?}", ch.direction),
            enabled: ch.enabled,
            status: ch.value.status.as_str().into(),
            cur: ch.value.cur,
            raw: ch.value.raw,
        }
    }
}
