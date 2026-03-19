use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("channel {0} not found")]
    ChannelNotFound(u32),

    #[error("channel capacity exceeded")]
    ChannelCapacityExceeded,

    #[error("table '{0}' not found")]
    TableNotFound(String),

    #[error("table capacity exceeded")]
    TableCapacityExceeded,

    #[error("conversion fault on channel {0}: {1}")]
    ConversionFault(u32, String),

    #[error("hal error: {0}")]
    Hal(#[from] sandstar_hal::HalError),

    #[error("write not supported for channel {0}")]
    WriteNotSupported(u32),

    #[error("invalid write level {0} (must be 1-17)")]
    InvalidWriteLevel(u8),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("poll capacity exceeded")]
    PollCapacityExceeded,

    #[error("duplicate channel {0}")]
    DuplicateChannel(u32),

    #[error("duplicate table '{0}'")]
    DuplicateTable(String),

    #[error("invalid table file: {0}")]
    InvalidTableFile(String),
}

pub type Result<T> = std::result::Result<T, EngineError>;
