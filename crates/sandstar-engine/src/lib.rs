pub mod channel;
pub mod components;
pub mod conversion;
pub mod engine;
pub mod error;
pub mod notify;
pub mod pid;
pub mod poll;
pub mod priority;
pub mod sequencer;
pub mod table;
pub mod value;
pub mod watch;

pub use engine::{Engine, Notification};
pub use error::{EngineError, Result};

/// Channel identifier (maps C `ENGINE_CHANNEL = unsigned int`).
pub type ChannelId = u32;

/// Engine data type (maps C `ENGINE_DATA = double`).
pub type EngineData = f64;

/// Channel status (maps C `ENGINE_STATUS` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EngineStatus {
    Ok,
    #[default]
    Unknown,
    Stale,
    Disabled,
    Fault,
    Down,
}

impl EngineStatus {
    /// Returns a static string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "Ok",
            Self::Unknown => "Unknown",
            Self::Stale => "Stale",
            Self::Disabled => "Disabled",
            Self::Fault => "Fault",
            Self::Down => "Down",
        }
    }
}

/// Engine value (maps C `ENGINE_VALUE` struct).
#[derive(Debug, Clone, Copy)]
pub struct EngineValue {
    pub status: EngineStatus,
    pub raw: f64,
    pub cur: f64,
    pub flags: ValueFlags,
    pub trigger: bool,
}

impl Default for EngineValue {
    fn default() -> Self {
        Self {
            status: EngineStatus::Unknown,
            raw: 0.0,
            cur: 0.0,
            flags: ValueFlags::empty(),
            trigger: false,
        }
    }
}

impl EngineValue {
    pub fn with_status(status: EngineStatus) -> Self {
        Self {
            status,
            ..Default::default()
        }
    }

    /// Set raw value (matches C `value_raw`).
    /// Replaces flags (not OR) and clears trigger.
    pub fn set_raw(&mut self, raw: f64) {
        self.raw = raw;
        self.flags = ValueFlags::RAW;
        self.trigger = false;
    }

    /// Set cur value (matches C `value_cur`).
    /// Replaces flags (not OR) and clears trigger.
    pub fn set_cur(&mut self, cur: f64) {
        self.cur = cur;
        self.flags = ValueFlags::CUR;
        self.trigger = false;
    }

    /// Compare two values by status and cur (matches C `value_cmp`).
    pub fn values_equal(&self, other: &EngineValue) -> bool {
        self.status == other.status && self.cur == other.cur
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ValueFlags: u8 {
        const RAW = 0x01;
        const CUR = 0x02;
    }
}
