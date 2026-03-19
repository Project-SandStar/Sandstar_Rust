//! Signal processing filters for sensor data.
//!
//! Three filter types:
//! - Smoothing: moving average/median/EWMA to reduce noise
//! - Spike: reject sudden large changes (sensor glitches)
//! - Rate limiting: slew rate control (max rise/fall per second)

use std::time::Instant;

/// Maximum smoothing buffer size (matches C `SMOOTH_BUFFER_MAX`).
pub const SMOOTH_BUFFER_MAX: usize = 10;

/// Smoothing method selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmoothMethod {
    /// Simple arithmetic mean of window values.
    Mean,
    /// Median of window values (best for outlier rejection).
    Median,
    /// Exponential weighted moving average.
    Ewma,
}

impl SmoothMethod {
    pub fn from_int(v: i32) -> Self {
        match v {
            0 => Self::Mean,
            2 => Self::Ewma,
            _ => Self::Median, // Default (1 and any other)
        }
    }
}

/// State for the smoothing filter (circular buffer).
#[derive(Debug, Clone)]
pub struct SmoothState {
    buffer: [f64; SMOOTH_BUFFER_MAX],
    head: usize,
    count: usize,
    ewma_value: f64,
}

impl Default for SmoothState {
    fn default() -> Self {
        Self {
            buffer: [0.0; SMOOTH_BUFFER_MAX],
            head: 0,
            count: 0,
            ewma_value: 0.0,
        }
    }
}

/// State for the rate limiter.
#[derive(Debug, Clone)]
pub struct RateLimitState {
    pub last_output: f64,
    pub last_time: Option<Instant>,
    pub initialized: bool,
}

impl Default for RateLimitState {
    fn default() -> Self {
        Self {
            last_output: 0.0,
            last_time: None,
            initialized: false,
        }
    }
}

/// State for the spike filter.
#[derive(Debug, Clone)]
pub struct SpikeState {
    pub last_valid: f64,
    pub reading_count: u32,
}

impl Default for SpikeState {
    fn default() -> Self {
        Self {
            last_valid: 0.0,
            reading_count: 0,
        }
    }
}

/// Configuration for the spike filter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpikeConfig {
    /// Reject readings changing more than threshold ratio from previous.
    pub threshold: f64,
    /// Number of readings to discard at startup.
    pub startup_discard: u32,
    /// Reverse flow threshold (raw units, negatives below this = noise).
    pub reverse_threshold: f64,
}

impl Default for SpikeConfig {
    fn default() -> Self {
        Self {
            threshold: 5.0,
            startup_discard: 8,
            reverse_threshold: 10.0,
        }
    }
}

/// Configuration for smoothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SmoothingConfig {
    pub window: usize,
    pub method: SmoothMethod,
}

impl Default for SmoothingConfig {
    fn default() -> Self {
        Self {
            window: 5,
            method: SmoothMethod::Median,
        }
    }
}

/// Configuration for rate limiting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimitConfig {
    /// Maximum rise rate (units per second).
    pub max_rise: f64,
    /// Maximum fall rate (units per second).
    pub max_fall: f64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_rise: 100.0,
            max_fall: 150.0,
        }
    }
}

/// Apply smoothing filter to a value.
///
/// Returns the smoothed value. Passes through raw input if fewer than 2 samples.
pub fn apply_smoothing(state: &mut SmoothState, value: f64, window: usize, method: SmoothMethod) -> f64 {
    let window = window.clamp(2, SMOOTH_BUFFER_MAX);

    // Add value to circular buffer
    state.buffer[state.head] = value;
    state.head = (state.head + 1) % window;
    if state.count < window {
        state.count += 1;
    }

    // Need at least 2 samples for mean/median.
    // EWMA initializes on first sample, so seed it here.
    if state.count < 2 {
        if method == SmoothMethod::Ewma {
            state.ewma_value = value;
        }
        return value;
    }

    match method {
        SmoothMethod::Mean => {
            let sum: f64 = state.buffer[..state.count].iter().sum();
            sum / state.count as f64
        }
        SmoothMethod::Median => {
            let mut sorted = [0.0f64; SMOOTH_BUFFER_MAX];
            sorted[..state.count].copy_from_slice(&state.buffer[..state.count]);
            // Bubble sort (small N, matches C implementation)
            for i in 0..state.count {
                for j in 0..state.count - 1 - i {
                    if sorted[j] > sorted[j + 1] {
                        sorted.swap(j, j + 1);
                    }
                }
            }
            sorted[state.count / 2]
        }
        SmoothMethod::Ewma => {
            let alpha = 2.0 / (window as f64 + 1.0);
            if state.count == 1 {
                state.ewma_value = value;
            } else {
                state.ewma_value = alpha * value + (1.0 - alpha) * state.ewma_value;
            }
            state.ewma_value
        }
    }
}

/// Apply rate limiting to a value.
///
/// Limits how fast the output can change (asymmetric rise/fall rates).
pub fn apply_rate_limit(state: &mut RateLimitState, value: f64, max_rise: f64, max_fall: f64) -> f64 {
    if !state.initialized {
        state.initialized = true;
        state.last_output = value;
        state.last_time = Some(Instant::now());
        return value;
    }

    let now = Instant::now();
    let dt = match state.last_time {
        Some(prev) => {
            let elapsed = now.duration_since(prev).as_secs_f64();
            elapsed.max(0.001) // Minimum 1ms to avoid division by zero
        }
        None => 0.001,
    };

    let output = if value > state.last_output {
        // Rising
        let max_change = max_rise * dt;
        if value - state.last_output > max_change {
            state.last_output + max_change
        } else {
            value
        }
    } else {
        // Falling
        let max_change = max_fall * dt;
        if state.last_output - value > max_change {
            state.last_output - max_change
        } else {
            value
        }
    };

    state.last_output = output;
    state.last_time = Some(now);
    output
}

/// Apply spike filter to a converted value.
///
/// Rejects values that change too rapidly (sensor glitch detection).
/// During startup, discards the first N readings to let the sensor settle.
pub fn apply_spike_filter(
    state: &mut SpikeState,
    value: f64,
    config: &SpikeConfig,
) -> f64 {
    state.reading_count += 1;

    // During startup discard period, accept everything to build baseline
    if state.reading_count <= config.startup_discard {
        state.last_valid = value;
        return value;
    }

    let baseline = state.last_valid;

    // Skip spike check if baseline is near zero
    if baseline.abs() < 1e-10 {
        state.last_valid = value;
        return value;
    }

    let ratio = (value / baseline).abs();
    if ratio > config.threshold || ratio < 1.0 / config.threshold {
        // Spike detected — also check for sudden drop to zero
        if baseline.abs() > 50.0 && value.abs() < 1.0 {
            return baseline; // Reject zero-drop
        }
        return baseline; // Reject spike
    }

    state.last_valid = value;
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_smoothing_mean() {
        let mut state = SmoothState::default();

        // First sample passes through
        assert_eq!(apply_smoothing(&mut state, 10.0, 3, SmoothMethod::Mean), 10.0);

        // Second sample: mean of [10, 20] = 15
        let result = apply_smoothing(&mut state, 20.0, 3, SmoothMethod::Mean);
        assert!((result - 15.0).abs() < 1e-10);

        // Third sample: mean of [10, 20, 30] = 20
        let result = apply_smoothing(&mut state, 30.0, 3, SmoothMethod::Mean);
        assert!((result - 20.0).abs() < 1e-10);
    }

    #[test]
    fn test_smoothing_median() {
        let mut state = SmoothState::default();

        apply_smoothing(&mut state, 10.0, 3, SmoothMethod::Median);
        apply_smoothing(&mut state, 100.0, 3, SmoothMethod::Median); // outlier

        // Third: [10, 100, 20] -> sorted [10, 20, 100] -> median = 20
        let result = apply_smoothing(&mut state, 20.0, 3, SmoothMethod::Median);
        assert!((result - 20.0).abs() < 1e-10);
    }

    #[test]
    fn test_smoothing_ewma() {
        let mut state = SmoothState::default();

        // First sample: EWMA = value
        let result = apply_smoothing(&mut state, 10.0, 5, SmoothMethod::Ewma);
        assert!((result - 10.0).abs() < 1e-10);

        // Second: alpha = 2/(5+1) = 0.333
        // EWMA = 0.333 * 20 + 0.667 * 10 = 13.33
        let result = apply_smoothing(&mut state, 20.0, 5, SmoothMethod::Ewma);
        let alpha = 2.0 / 6.0;
        let expected = alpha * 20.0 + (1.0 - alpha) * 10.0;
        assert!((result - expected).abs() < 0.01);
    }

    #[test]
    fn test_smoothing_window_clamp() {
        let mut state = SmoothState::default();

        // Window of 1 should clamp to 2
        apply_smoothing(&mut state, 10.0, 1, SmoothMethod::Mean);
        let result = apply_smoothing(&mut state, 20.0, 1, SmoothMethod::Mean);
        assert!((result - 15.0).abs() < 1e-10);
    }

    #[test]
    fn test_rate_limit_passthrough() {
        let mut state = RateLimitState::default();

        // First call always passes through
        let result = apply_rate_limit(&mut state, 100.0, 100.0, 150.0);
        assert_eq!(result, 100.0);
    }

    #[test]
    fn test_rate_limit_clamping() {
        let mut state = RateLimitState {
            last_output: 100.0,
            last_time: Some(Instant::now()),
            initialized: true,
        };

        // Very large jump should be clamped
        // With max_rise = 100/s and ~0ms elapsed, max_change ≈ 0.1
        let result = apply_rate_limit(&mut state, 10000.0, 100.0, 150.0);
        assert!(result < 10000.0);
        assert!(result >= 100.0);
    }

    #[test]
    fn test_spike_filter_startup() {
        let config = SpikeConfig {
            threshold: 5.0,
            startup_discard: 3,
            reverse_threshold: 10.0,
        };
        let mut state = SpikeState::default();

        // During startup, all values accepted
        assert_eq!(apply_spike_filter(&mut state, 100.0, &config), 100.0);
        assert_eq!(apply_spike_filter(&mut state, 200.0, &config), 200.0);
        assert_eq!(apply_spike_filter(&mut state, 300.0, &config), 300.0);
    }

    #[test]
    fn test_spike_filter_rejects_spike() {
        let config = SpikeConfig {
            threshold: 5.0,
            startup_discard: 2,
            reverse_threshold: 10.0,
        };
        let mut state = SpikeState::default();

        // Startup
        apply_spike_filter(&mut state, 100.0, &config);
        apply_spike_filter(&mut state, 100.0, &config);

        // Spike (600 = 6x baseline, > threshold of 5x)
        let result = apply_spike_filter(&mut state, 600.0, &config);
        assert_eq!(result, 100.0); // Returns baseline
    }

    #[test]
    fn test_spike_filter_accepts_normal() {
        let config = SpikeConfig {
            threshold: 5.0,
            startup_discard: 2,
            reverse_threshold: 10.0,
        };
        let mut state = SpikeState::default();

        apply_spike_filter(&mut state, 100.0, &config);
        apply_spike_filter(&mut state, 100.0, &config);

        // Normal change (150 = 1.5x baseline, < threshold of 5x)
        let result = apply_spike_filter(&mut state, 150.0, &config);
        assert_eq!(result, 150.0);
    }

    #[test]
    fn test_spike_filter_zero_drop() {
        let config = SpikeConfig::default();
        let mut state = SpikeState {
            last_valid: 100.0,
            reading_count: 10, // Past startup
        };

        // Sudden drop to near-zero when baseline > 50
        let result = apply_spike_filter(&mut state, 0.5, &config);
        assert_eq!(result, 100.0); // Rejected
    }

    #[test]
    fn test_smooth_method_from_int() {
        assert_eq!(SmoothMethod::from_int(0), SmoothMethod::Mean);
        assert_eq!(SmoothMethod::from_int(1), SmoothMethod::Median);
        assert_eq!(SmoothMethod::from_int(2), SmoothMethod::Ewma);
        assert_eq!(SmoothMethod::from_int(99), SmoothMethod::Median); // default
    }
}
