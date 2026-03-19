pub mod auto_detect;
pub mod filters;
pub mod sdp610;

pub use filters::{
    RateLimitConfig, RateLimitState, SmoothMethod, SmoothState, SmoothingConfig, SpikeConfig,
    SpikeState,
};
pub use sdp610::{
    DEFAULT_DEAD_BAND, DEFAULT_HYST_OFF, DEFAULT_HYST_ON, DEFAULT_K_FACTOR, DEFAULT_SCALE_FACTOR,
};
