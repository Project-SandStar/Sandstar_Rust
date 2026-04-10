//! PID (Proportional-Integral-Derivative) controller.
//!
//! Ported from Sedona `control::LP`. Implements a standard PID loop with:
//! - Proportional, Integral, and Derivative terms
//! - Integral anti-windup (clamped to scaled min/max)
//! - Output rate limiting (`max_delta`)
//! - Direct and reverse action
//! - Configurable execution interval

use std::time::{Duration, Instant};

/// PID controller configuration and state.
#[derive(Debug, Clone)]
pub struct PidController {
    // -- Configuration (set by user) --
    /// Proportional gain.
    pub kp: f64,
    /// Integral gain (resets per minute). 0 = no integral.
    pub ki: f64,
    /// Derivative gain. 0 = no derivative.
    pub kd: f64,
    /// Output minimum (engineering units, e.g., 0.0).
    pub out_min: f64,
    /// Output maximum (engineering units, e.g., 100.0).
    pub out_max: f64,
    /// Bias (added to output when ki=0). Default 50.0.
    pub bias: f64,
    /// Maximum output change per execution (0 = unlimited).
    pub max_delta: f64,
    /// True = direct action (output increases when error increases).
    /// False = reverse action (output decreases when error increases).
    pub direct: bool,
    /// Execution interval in milliseconds (default 1000).
    pub exec_interval_ms: u64,
    /// Enable/disable the controller.
    pub enabled: bool,

    // -- Runtime state (managed internally) --
    /// Current output value.
    output: f64,
    /// Accumulated integral term.
    integral: f64,
    /// Previous error (for derivative).
    last_error: f64,
    /// Last execution time.
    last_exec: Option<Instant>,
    /// Whether the controller has been initialized.
    initialized: bool,
}

impl Default for PidController {
    fn default() -> Self {
        Self::new()
    }
}

impl PidController {
    /// Create a new PID controller with default parameters.
    ///
    /// Defaults: kp=1, ki=0, kd=0, min=0, max=100, bias=50, direct=true,
    /// interval=1000ms, enabled=true.
    pub fn new() -> Self {
        Self {
            kp: 1.0,
            ki: 0.0,
            kd: 0.0,
            out_min: 0.0,
            out_max: 100.0,
            bias: 50.0,
            max_delta: 0.0,
            direct: true,
            exec_interval_ms: 1000,
            enabled: true,
            output: 0.0,
            integral: 0.0,
            last_error: 0.0,
            last_exec: None,
            initialized: false,
        }
    }

    /// Execute one PID cycle. Returns the new output value.
    ///
    /// `setpoint` is the desired value, `process_variable` is the measured value.
    /// `now` is the current instant (for deterministic testing).
    pub fn execute(&mut self, setpoint: f64, process_variable: f64, now: Instant) -> f64 {
        // Disabled: return current output unchanged.
        if !self.enabled {
            return self.output;
        }

        // First call: initialize timing, no computation yet.
        if !self.initialized {
            self.last_exec = Some(now);
            self.last_error = 0.0;
            self.initialized = true;
            // Return bias as initial output when no integral, or out_min otherwise.
            if self.ki == 0.0 {
                self.output = self.bias.clamp(self.out_min, self.out_max);
            }
            return self.output;
        }

        // Check execution interval.
        let interval = Duration::from_millis(self.exec_interval_ms);
        let elapsed = now.duration_since(self.last_exec.unwrap_or(now));
        if elapsed < interval {
            return self.output;
        }

        let dt_secs = elapsed.as_secs_f64();
        if dt_secs <= 0.0 {
            return self.output;
        }

        // Compute error.
        let mut error = setpoint - process_variable;
        if !self.direct {
            error = -error;
        }

        // Proportional term.
        let p = self.kp * error;

        // Integral term with anti-windup.
        if self.ki != 0.0 {
            // ki is "resets per minute", so scale by dt/60.
            self.integral += self.ki * error * dt_secs / 60.0;

            // Anti-windup: clamp integral to output range.
            // When ki > 0, the integral alone must stay within [out_min, out_max].
            let integral_min = self.out_min;
            let integral_max = self.out_max;
            self.integral = self.integral.clamp(integral_min, integral_max);
        }

        // Derivative term.
        let d = if self.kd != 0.0 {
            self.kd * (error - self.last_error) / dt_secs
        } else {
            0.0
        };

        // Raw output.
        let raw = if self.ki != 0.0 {
            // With integral: P + I + D (integral includes accumulated history).
            p + self.integral + d
        } else {
            // Without integral: P + D + bias.
            p + d + self.bias
        };

        // Clamp to output range.
        let mut new_output = raw.clamp(self.out_min, self.out_max);

        // Rate limit.
        if self.max_delta > 0.0 {
            let change = new_output - self.output;
            if change.abs() > self.max_delta {
                new_output = self.output + change.signum() * self.max_delta;
                // Re-clamp after rate limiting.
                new_output = new_output.clamp(self.out_min, self.out_max);
            }
        }

        // Update state.
        self.last_error = error;
        self.last_exec = Some(now);
        self.output = new_output;

        self.output
    }

    /// Get the current output value.
    pub fn output(&self) -> f64 {
        self.output
    }

    /// Reset the controller state (integral, derivative, timing).
    pub fn reset(&mut self) {
        self.output = 0.0;
        self.integral = 0.0;
        self.last_error = 0.0;
        self.last_exec = None;
        self.initialized = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create an Instant and advance by given milliseconds.
    fn advance(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn test_pid_proportional_only() {
        let mut pid = PidController::new();
        pid.kp = 10.0;
        pid.ki = 0.0;
        pid.kd = 0.0;
        pid.bias = 50.0;

        let t0 = Instant::now();
        // First call initializes.
        pid.execute(75.0, 70.0, t0);

        // Second call after interval: error=5, p=50, output=50+50=100 (clamped)
        let out = pid.execute(75.0, 70.0, advance(t0, 1000));
        assert!((out - 100.0).abs() < 0.001, "expected ~100.0, got {out}");
    }

    #[test]
    fn test_pid_proportional_moderate_error() {
        let mut pid = PidController::new();
        pid.kp = 2.0;
        pid.ki = 0.0;
        pid.kd = 0.0;
        pid.bias = 50.0;

        let t0 = Instant::now();
        pid.execute(75.0, 70.0, t0); // init

        // error=5, p=10, output=10+50=60
        let out = pid.execute(75.0, 70.0, advance(t0, 1000));
        assert!((out - 60.0).abs() < 0.001, "expected 60.0, got {out}");
    }

    #[test]
    fn test_pid_reverse_action() {
        let mut pid = PidController::new();
        pid.kp = 2.0;
        pid.ki = 0.0;
        pid.kd = 0.0;
        pid.bias = 50.0;
        pid.direct = false; // reverse action

        let t0 = Instant::now();
        pid.execute(75.0, 70.0, t0); // init

        // Reverse: error = -(75-70) = -5, p = 2*(-5) = -10, output = -10+50 = 40
        let out = pid.execute(75.0, 70.0, advance(t0, 1000));
        assert!((out - 40.0).abs() < 0.001, "expected 40.0, got {out}");
    }

    #[test]
    fn test_pid_integral_accumulation() {
        let mut pid = PidController::new();
        pid.kp = 0.0;
        pid.ki = 1.0; // 1 reset per minute
        pid.kd = 0.0;

        let t0 = Instant::now();
        pid.execute(75.0, 70.0, t0); // init

        // Run several cycles. Each cycle: integral += 1.0 * 5 * 1.0/60 = 0.0833...
        let mut out = 0.0;
        for i in 1..=10 {
            out = pid.execute(75.0, 70.0, advance(t0, 1000 * i));
        }

        // After 10 seconds: integral ~ 10 * 5 / 60 = 0.8333
        // Output = integral = 0.8333
        assert!(out > 0.5, "integral should grow, got {out}");
        assert!(out < 2.0, "integral should be moderate, got {out}");
    }

    #[test]
    fn test_pid_anti_windup() {
        let mut pid = PidController::new();
        pid.kp = 0.0;
        pid.ki = 60.0; // Very high integral gain (60 resets per minute = 1/sec)
        pid.kd = 0.0;
        pid.out_min = 0.0;
        pid.out_max = 100.0;

        let t0 = Instant::now();
        pid.execute(200.0, 0.0, t0); // init, huge error

        // Run many cycles with huge error.
        let mut out = 0.0;
        for i in 1..=100 {
            out = pid.execute(200.0, 0.0, advance(t0, 1000 * i));
        }

        // Output should be clamped to max, integral should not exceed max.
        assert!(
            (out - 100.0).abs() < 0.001,
            "output should be clamped to 100.0, got {out}"
        );
        assert!(
            pid.integral <= 100.0,
            "integral should be clamped, got {}",
            pid.integral
        );
    }

    #[test]
    fn test_pid_max_delta_rate_limit() {
        let mut pid = PidController::new();
        pid.kp = 10.0;
        pid.ki = 0.0;
        pid.kd = 0.0;
        pid.bias = 50.0;
        pid.max_delta = 5.0; // max 5% change per cycle

        let t0 = Instant::now();
        pid.execute(75.0, 70.0, t0); // init -> output = bias = 50.0

        // Without rate limit: output would jump to 100.
        // With max_delta=5: change limited to +5 -> 55.0.
        let out = pid.execute(75.0, 70.0, advance(t0, 1000));
        assert!(
            (out - 55.0).abs() < 0.001,
            "rate limited to 55.0, got {out}"
        );

        // Next cycle: another +5 -> 60.0
        let out = pid.execute(75.0, 70.0, advance(t0, 2000));
        assert!(
            (out - 60.0).abs() < 0.001,
            "rate limited to 60.0, got {out}"
        );
    }

    #[test]
    fn test_pid_disabled_returns_current() {
        let mut pid = PidController::new();
        pid.kp = 10.0;
        pid.ki = 0.0;
        pid.kd = 0.0;

        let t0 = Instant::now();
        pid.execute(75.0, 70.0, t0); // init
        let out1 = pid.execute(75.0, 70.0, advance(t0, 1000));

        // Disable the controller.
        pid.enabled = false;

        // Output should be frozen.
        let out2 = pid.execute(100.0, 0.0, advance(t0, 2000));
        assert!(
            (out1 - out2).abs() < 0.001,
            "disabled: output should be frozen at {out1}, got {out2}"
        );
    }

    #[test]
    fn test_pid_interval_skip() {
        let mut pid = PidController::new();
        pid.kp = 10.0;
        pid.ki = 0.0;
        pid.kd = 0.0;
        pid.exec_interval_ms = 2000; // 2 second interval

        let t0 = Instant::now();
        pid.execute(75.0, 70.0, t0); // init
        let init_out = pid.output();

        // Call at 1 second: should skip (interval not elapsed).
        let out = pid.execute(75.0, 70.0, advance(t0, 1000));
        assert!(
            (out - init_out).abs() < 0.001,
            "should skip before interval, got {out}"
        );

        // Call at 2 seconds: should compute.
        let out = pid.execute(75.0, 70.0, advance(t0, 2000));
        assert!(
            (out - init_out).abs() > 0.001,
            "should compute after interval, got {out}"
        );
    }

    #[test]
    fn test_pid_clamp_to_min_max() {
        let mut pid = PidController::new();
        pid.kp = 100.0; // Very high gain -> output wants to go way above max.
        pid.ki = 0.0;
        pid.kd = 0.0;
        pid.bias = 50.0;
        pid.out_min = 10.0;
        pid.out_max = 90.0;

        let t0 = Instant::now();
        pid.execute(100.0, 0.0, t0); // init

        // Huge positive error -> output should clamp to 90.
        let out = pid.execute(100.0, 0.0, advance(t0, 1000));
        assert!(
            (out - 90.0).abs() < 0.001,
            "should clamp to max 90, got {out}"
        );

        // Huge negative error -> output should clamp to 10.
        let out = pid.execute(0.0, 100.0, advance(t0, 2000));
        assert!(
            (out - 10.0).abs() < 0.001,
            "should clamp to min 10, got {out}"
        );
    }

    #[test]
    fn test_pid_zero_error_stable() {
        let mut pid = PidController::new();
        pid.kp = 5.0;
        pid.ki = 0.0;
        pid.kd = 0.0;
        pid.bias = 50.0;

        let t0 = Instant::now();
        pid.execute(70.0, 70.0, t0); // init

        // sp == pv -> error=0 -> output = 0 + 0 + 50 = 50 (bias).
        let out = pid.execute(70.0, 70.0, advance(t0, 1000));
        assert!((out - 50.0).abs() < 0.001, "zero error -> bias, got {out}");

        // Stays stable on subsequent calls.
        let out = pid.execute(70.0, 70.0, advance(t0, 2000));
        assert!(
            (out - 50.0).abs() < 0.001,
            "should remain stable, got {out}"
        );
    }

    #[test]
    fn test_pid_derivative_kick() {
        let mut pid = PidController::new();
        pid.kp = 0.0; // No proportional.
        pid.ki = 0.0;
        pid.kd = 10.0;
        pid.bias = 50.0;

        let t0 = Instant::now();
        pid.execute(70.0, 70.0, t0); // init

        // First computation: error=0, last_error=0, d=0. Output = 50 (bias).
        let out = pid.execute(70.0, 70.0, advance(t0, 1000));
        assert!(
            (out - 50.0).abs() < 0.001,
            "no error change -> bias, got {out}"
        );

        // Now introduce error: sp=80, pv=70, error=10. d = 10*(10-0)/1 = 100.
        // Output = 0 + 100 + 50 = 150 -> clamped to 100.
        let out = pid.execute(80.0, 70.0, advance(t0, 2000));
        assert!(
            (out - 100.0).abs() < 0.001,
            "derivative kick clamped to 100, got {out}"
        );
    }

    #[test]
    fn test_pid_reset() {
        let mut pid = PidController::new();
        pid.kp = 5.0;
        pid.ki = 1.0;
        pid.kd = 1.0;

        let t0 = Instant::now();
        pid.execute(80.0, 70.0, t0);
        pid.execute(80.0, 70.0, advance(t0, 1000));
        pid.execute(80.0, 70.0, advance(t0, 2000));

        // State should be non-zero.
        assert!(pid.output != 0.0 || pid.integral != 0.0);

        // Reset.
        pid.reset();

        assert_eq!(pid.output, 0.0);
        assert_eq!(pid.integral, 0.0);
        assert_eq!(pid.last_error, 0.0);
        assert!(pid.last_exec.is_none());
        assert!(!pid.initialized);
    }

    #[test]
    fn test_pid_production_params() {
        // Simulates a realistic HVAC PID: kp=20, ki=5, cooling a zone.
        let mut pid = PidController::new();
        pid.kp = 20.0;
        pid.ki = 5.0;
        pid.kd = 0.0;
        pid.out_min = 0.0;
        pid.out_max = 100.0;
        pid.direct = false; // Cooling: reverse action.
        pid.exec_interval_ms = 1000;

        let t0 = Instant::now();
        pid.execute(72.0, 75.0, t0); // init, zone is 75, setpoint 72

        // error = 72 - 75 = -3, reverse -> error = 3
        // p = 20 * 3 = 60
        // integral = 5 * 3 * 1/60 = 0.25
        // output = 60 + 0.25 = 60.25
        let out = pid.execute(72.0, 75.0, advance(t0, 1000));
        assert!(out > 55.0 && out < 65.0, "expected ~60.25, got {out}");

        // Run a few more cycles; integral should increase output further.
        let out2 = pid.execute(72.0, 75.0, advance(t0, 2000));
        assert!(
            out2 > out,
            "integral should increase output: {out2} > {out}"
        );
    }

    #[test]
    fn test_pid_negative_error_reduces_output() {
        let mut pid = PidController::new();
        pid.kp = 5.0;
        pid.ki = 0.0;
        pid.kd = 0.0;
        pid.bias = 50.0;

        let t0 = Instant::now();
        pid.execute(70.0, 75.0, t0); // init, pv > sp

        // error = 70 - 75 = -5, p = -25, output = -25 + 50 = 25
        let out = pid.execute(70.0, 75.0, advance(t0, 1000));
        assert!((out - 25.0).abs() < 0.001, "expected 25.0, got {out}");
    }

    #[test]
    fn test_pid_default_impl() {
        // Verify Default trait is correctly derived.
        let pid = PidController::default();
        assert_eq!(pid.kp, 1.0);
        assert_eq!(pid.ki, 0.0);
        assert_eq!(pid.kd, 0.0);
        assert_eq!(pid.out_min, 0.0);
        assert_eq!(pid.out_max, 100.0);
        assert_eq!(pid.bias, 50.0);
        assert_eq!(pid.max_delta, 0.0);
        assert!(pid.direct);
        assert_eq!(pid.exec_interval_ms, 1000);
        assert!(pid.enabled);
    }

    #[test]
    fn test_pid_nan_input() {
        let mut pid = PidController::new();
        pid.kp = 5.0;
        pid.ki = 1.0;
        pid.kd = 1.0;

        let t0 = Instant::now();
        pid.execute(72.0, 70.0, t0); // init

        // Pass NaN as process_variable.
        let out = pid.execute(72.0, f64::NAN, advance(t0, 1000));
        // NaN propagates through arithmetic: error = 72 - NaN = NaN.
        // The f64::clamp function returns NaN when the input is NaN.
        // This documents the current behavior: NaN input produces NaN output.
        assert!(
            out.is_nan(),
            "NaN input propagates to NaN output, got {out}"
        );

        // Verify the controller can recover after reset.
        pid.reset();
        pid.execute(72.0, 70.0, t0);
        let out = pid.execute(72.0, 70.0, advance(t0, 1000));
        assert!(!out.is_nan(), "after reset, output should be valid");
    }

    #[test]
    fn test_pid_infinity_input() {
        let mut pid = PidController::new();
        pid.kp = 5.0;
        pid.ki = 0.0;
        pid.kd = 0.0;
        pid.bias = 50.0;

        let t0 = Instant::now();
        pid.execute(72.0, 70.0, t0); // init

        // Pass +INFINITY as process_variable.
        // error = 72 - INF = -INF, p = 5 * -INF = -INF
        // raw = -INF + 50 = -INF, clamp(-INF, 0, 100) = 0.0
        let out = pid.execute(72.0, f64::INFINITY, advance(t0, 1000));
        assert!(
            (out - 0.0).abs() < 0.001,
            "infinity input should clamp to out_min, got {out}"
        );

        // Pass -INFINITY as process_variable.
        // error = 72 - (-INF) = +INF, p = 5 * INF = INF
        // raw = INF + 50 = INF, clamp(INF, 0, 100) = 100.0
        let out = pid.execute(72.0, f64::NEG_INFINITY, advance(t0, 2000));
        assert!(
            (out - 100.0).abs() < 0.001,
            "neg infinity input should clamp to out_max, got {out}"
        );
    }

    #[test]
    fn test_pid_zero_exec_interval() {
        let mut pid = PidController::new();
        pid.kp = 5.0;
        pid.ki = 1.0;
        pid.kd = 1.0;
        pid.exec_interval_ms = 0; // zero interval

        let t0 = Instant::now();
        pid.execute(72.0, 70.0, t0); // init

        // With exec_interval_ms=0, Duration::from_millis(0) is zero,
        // so elapsed >= interval is true immediately. dt_secs will be 0.0
        // when called at the same instant, and the guard `dt_secs <= 0.0`
        // will return early without division by zero.
        let out = pid.execute(72.0, 70.0, t0);
        // Same instant => dt_secs=0 => early return with current output.
        assert!(
            !out.is_nan(),
            "zero interval at same instant should not produce NaN"
        );
        assert!(out.is_finite(), "output should be finite, got {out}");

        // Advance by 1ms — dt_secs > 0 so computation proceeds.
        let out = pid.execute(72.0, 70.0, advance(t0, 1));
        assert!(
            out.is_finite(),
            "output with 1ms dt should be finite, got {out}"
        );
    }

    #[test]
    fn test_pid_very_large_dt() {
        let mut pid = PidController::new();
        pid.kp = 0.0;
        pid.ki = 1.0; // 1 reset per minute
        pid.kd = 0.0;
        pid.out_min = 0.0;
        pid.out_max = 100.0;

        let t0 = Instant::now();
        pid.execute(75.0, 70.0, t0); // init

        // Simulate a 1-hour gap: 3_600_000 ms.
        // integral += 1.0 * 5 * 3600.0 / 60.0 = 300.0
        // But anti-windup clamps integral to [0, 100], so integral = 100.
        let out = pid.execute(75.0, 70.0, advance(t0, 3_600_000));
        assert!(
            out.is_finite(),
            "1-hour gap should not cause overflow, got {out}"
        );
        assert!(
            (out - 100.0).abs() < 0.001,
            "large dt should clamp to max via anti-windup, got {out}"
        );
        assert!(
            pid.integral <= 100.0,
            "integral should be clamped after large dt, got {}",
            pid.integral
        );
    }
}
