//! Simple control components library.
//!
//! Implements arithmetic, logic, timing, HVAC, and scheduling components
//! for use in the configuration-driven control engine. Each component is
//! a concrete struct following the same pattern as `PidController` and
//! `LeadSequencer`.

use std::time::Instant;

// ── Arithmetic Components ───────────────────────────────────

/// Two-input addition: out = in1 + in2.
#[derive(Debug, Clone)]
pub struct Add2 {
    pub in1: f64,
    pub in2: f64,
}

impl Default for Add2 {
    fn default() -> Self {
        Self::new()
    }
}

impl Add2 {
    pub fn new() -> Self {
        Self { in1: 0.0, in2: 0.0 }
    }

    pub fn execute(&self) -> f64 {
        self.in1 + self.in2
    }
}

/// Two-input subtraction: out = in1 - in2.
#[derive(Debug, Clone)]
pub struct Sub2 {
    pub in1: f64,
    pub in2: f64,
}

impl Default for Sub2 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sub2 {
    pub fn new() -> Self {
        Self { in1: 0.0, in2: 0.0 }
    }

    pub fn execute(&self) -> f64 {
        self.in1 - self.in2
    }
}

/// Two-input multiplication: out = in1 * in2.
#[derive(Debug, Clone)]
pub struct Mul2 {
    pub in1: f64,
    pub in2: f64,
}

impl Default for Mul2 {
    fn default() -> Self {
        Self::new()
    }
}

impl Mul2 {
    pub fn new() -> Self {
        Self { in1: 0.0, in2: 0.0 }
    }

    pub fn execute(&self) -> f64 {
        self.in1 * self.in2
    }
}

/// Two-input division: out = in1 / in2 (returns 0.0 on divide-by-zero).
#[derive(Debug, Clone)]
pub struct Div2 {
    pub in1: f64,
    pub in2: f64,
}

impl Default for Div2 {
    fn default() -> Self {
        Self::new()
    }
}

impl Div2 {
    pub fn new() -> Self {
        Self { in1: 0.0, in2: 0.0 }
    }

    pub fn execute(&self) -> f64 {
        if self.in2 == 0.0 {
            0.0
        } else {
            self.in1 / self.in2
        }
    }
}

/// Negate: out = -input.
#[derive(Debug, Clone)]
pub struct Neg {
    pub input: f64,
}

impl Default for Neg {
    fn default() -> Self {
        Self::new()
    }
}

impl Neg {
    pub fn new() -> Self {
        Self { input: 0.0 }
    }

    pub fn execute(&self) -> f64 {
        -self.input
    }
}

/// Round to N decimal places.
#[derive(Debug, Clone)]
pub struct Round {
    pub input: f64,
    pub decimals: u32,
}

impl Default for Round {
    fn default() -> Self {
        Self::new()
    }
}

impl Round {
    pub fn new() -> Self {
        Self {
            input: 0.0,
            decimals: 0,
        }
    }

    pub fn execute(&self) -> f64 {
        let factor = 10_f64.powi(self.decimals as i32);
        (self.input * factor).round() / factor
    }
}

/// Float offset: out = input + offset.
#[derive(Debug, Clone)]
pub struct FloatOffset {
    pub input: f64,
    pub offset: f64,
}

impl Default for FloatOffset {
    fn default() -> Self {
        Self::new()
    }
}

impl FloatOffset {
    pub fn new() -> Self {
        Self {
            input: 0.0,
            offset: 0.0,
        }
    }

    pub fn execute(&self) -> f64 {
        self.input + self.offset
    }
}

// ── Logic Components ────────────────────────────────────────

/// Two-input AND: out = in1 && in2.
#[derive(Debug, Clone)]
pub struct And2 {
    pub in1: bool,
    pub in2: bool,
}

impl Default for And2 {
    fn default() -> Self {
        Self::new()
    }
}

impl And2 {
    pub fn new() -> Self {
        Self {
            in1: false,
            in2: false,
        }
    }

    pub fn execute(&self) -> bool {
        self.in1 && self.in2
    }
}

/// Two-input OR: out = in1 || in2.
#[derive(Debug, Clone)]
pub struct Or2 {
    pub in1: bool,
    pub in2: bool,
}

impl Default for Or2 {
    fn default() -> Self {
        Self::new()
    }
}

impl Or2 {
    pub fn new() -> Self {
        Self {
            in1: false,
            in2: false,
        }
    }

    pub fn execute(&self) -> bool {
        self.in1 || self.in2
    }
}

/// NOT gate: out = !input.
#[derive(Debug, Clone)]
pub struct Not {
    pub input: bool,
}

impl Default for Not {
    fn default() -> Self {
        Self::new()
    }
}

impl Not {
    pub fn new() -> Self {
        Self { input: false }
    }

    pub fn execute(&self) -> bool {
        !self.input
    }
}

/// SR Latch: set has priority over reset.
#[derive(Debug, Clone)]
pub struct SRLatch {
    pub set: bool,
    pub reset: bool,
    output: bool,
}

impl Default for SRLatch {
    fn default() -> Self {
        Self::new()
    }
}

impl SRLatch {
    pub fn new() -> Self {
        Self {
            set: false,
            reset: false,
            output: false,
        }
    }

    pub fn execute(&mut self) -> bool {
        if self.set {
            self.output = true;
        } else if self.reset {
            self.output = false;
        }
        self.output
    }

    pub fn output(&self) -> bool {
        self.output
    }
}

// ── Timing Components ───────────────────────────────────────

/// Delay On: output goes true only after input has been true for `delay_ms`.
#[derive(Debug, Clone)]
pub struct DelayOn {
    pub delay_ms: u64,
    pub input: bool,
    pub enabled: bool,
    start_time: Option<Instant>,
    output: bool,
}

impl Default for DelayOn {
    fn default() -> Self {
        Self::new()
    }
}

impl DelayOn {
    pub fn new() -> Self {
        Self {
            delay_ms: 1000,
            input: false,
            enabled: true,
            start_time: None,
            output: false,
        }
    }

    pub fn execute(&mut self, now: Instant) -> bool {
        if !self.enabled {
            return self.output;
        }

        if self.input {
            match self.start_time {
                None => {
                    // Input just went true, start timing.
                    self.start_time = Some(now);
                }
                Some(start) => {
                    let elapsed = now.duration_since(start).as_millis() as u64;
                    if elapsed >= self.delay_ms {
                        self.output = true;
                    }
                }
            }
        } else {
            // Input is false: reset timer and output.
            self.start_time = None;
            self.output = false;
        }

        self.output
    }

    pub fn output(&self) -> bool {
        self.output
    }
}

/// Delay Off: output stays true for `delay_ms` after input goes false.
#[derive(Debug, Clone)]
pub struct DelayOff {
    pub delay_ms: u64,
    pub input: bool,
    pub enabled: bool,
    off_time: Option<Instant>,
    output: bool,
}

impl Default for DelayOff {
    fn default() -> Self {
        Self::new()
    }
}

impl DelayOff {
    pub fn new() -> Self {
        Self {
            delay_ms: 1000,
            input: false,
            enabled: true,
            off_time: None,
            output: false,
        }
    }

    pub fn execute(&mut self, now: Instant) -> bool {
        if !self.enabled {
            return self.output;
        }

        if self.input {
            // Input is true: output is true, reset off timer.
            self.output = true;
            self.off_time = None;
        } else if self.output {
            // Input went false while output was true: start delay timer.
            match self.off_time {
                None => {
                    self.off_time = Some(now);
                }
                Some(off_start) => {
                    let elapsed = now.duration_since(off_start).as_millis() as u64;
                    if elapsed >= self.delay_ms {
                        self.output = false;
                        self.off_time = None;
                    }
                }
            }
        }

        self.output
    }

    pub fn output(&self) -> bool {
        self.output
    }
}

/// One-Shot: output goes true for `duration_ms` when input transitions false -> true.
#[derive(Debug, Clone)]
pub struct OneShot {
    pub duration_ms: u64,
    pub input: bool,
    prev_input: bool,
    trigger_time: Option<Instant>,
    output: bool,
}

impl Default for OneShot {
    fn default() -> Self {
        Self::new()
    }
}

impl OneShot {
    pub fn new() -> Self {
        Self {
            duration_ms: 1000,
            input: false,
            prev_input: false,
            trigger_time: None,
            output: false,
        }
    }

    pub fn execute(&mut self, now: Instant) -> bool {
        // Detect rising edge.
        if self.input && !self.prev_input {
            self.trigger_time = Some(now);
            self.output = true;
        }

        // Check if pulse has expired.
        if let Some(trigger) = self.trigger_time {
            let elapsed = now.duration_since(trigger).as_millis() as u64;
            if elapsed >= self.duration_ms {
                self.output = false;
                self.trigger_time = None;
            }
        }

        self.prev_input = self.input;
        self.output
    }

    pub fn output(&self) -> bool {
        self.output
    }
}

/// Ramp: output ramps linearly from min to max over period_ms, then resets.
#[derive(Debug, Clone)]
pub struct Ramp {
    pub min: f64,
    pub max: f64,
    pub period_ms: u64,
    pub enabled: bool,
    start_time: Option<Instant>,
    output: f64,
}

impl Default for Ramp {
    fn default() -> Self {
        Self::new()
    }
}

impl Ramp {
    pub fn new() -> Self {
        Self {
            min: 0.0,
            max: 100.0,
            period_ms: 5000,
            enabled: true,
            start_time: None,
            output: 0.0,
        }
    }

    pub fn execute(&mut self, now: Instant) -> f64 {
        if !self.enabled {
            return self.output;
        }

        if self.period_ms == 0 {
            self.output = self.min;
            return self.output;
        }

        let start = match self.start_time {
            Some(t) => t,
            None => {
                self.start_time = Some(now);
                self.output = self.min;
                return self.output;
            }
        };

        let elapsed_ms = now.duration_since(start).as_millis() as u64;
        // Cycle within the period.
        let position_ms = elapsed_ms % self.period_ms;
        let fraction = position_ms as f64 / self.period_ms as f64;
        self.output = self.min + (self.max - self.min) * fraction;

        self.output
    }

    pub fn output(&self) -> f64 {
        self.output
    }

    pub fn reset(&mut self) {
        self.start_time = None;
        self.output = self.min;
    }
}

// ── HVAC Components ─────────────────────────────────────────

/// Thermostat: simple on/off control with deadband.
///
/// In heating mode: output turns on when temperature drops below (setpoint - deadband/2),
/// turns off when temperature rises above (setpoint + deadband/2).
///
/// In cooling mode: output turns on when temperature rises above (setpoint + deadband/2),
/// turns off when temperature drops below (setpoint - deadband/2).
#[derive(Debug, Clone)]
pub struct Thermostat {
    /// Desired temperature.
    pub setpoint: f64,
    /// Deadband width (total range).
    pub deadband: f64,
    /// True = heating mode, false = cooling mode.
    pub heating: bool,
    /// Current output state.
    output: bool,
}

impl Default for Thermostat {
    fn default() -> Self {
        Self::new()
    }
}

impl Thermostat {
    pub fn new() -> Self {
        Self {
            setpoint: 72.0,
            deadband: 2.0,
            heating: true,
            output: false,
        }
    }

    pub fn execute(&mut self, temperature: f64) -> bool {
        let low = self.setpoint - self.deadband / 2.0;
        let high = self.setpoint + self.deadband / 2.0;

        if self.heating {
            // Heating: turn on when cold, off when warm.
            if temperature < low {
                self.output = true;
            } else if temperature > high {
                self.output = false;
            }
        } else {
            // Cooling: turn on when warm, off when cold.
            if temperature > high {
                self.output = true;
            } else if temperature < low {
                self.output = false;
            }
        }

        self.output
    }

    pub fn output(&self) -> bool {
        self.output
    }
}

/// Hysteresis: output switches between high/low values based on input crossing thresholds.
#[derive(Debug, Clone)]
pub struct Hysteresis {
    pub rising_threshold: f64,
    pub falling_threshold: f64,
    pub high_value: f64,
    pub low_value: f64,
    output: f64,
}

impl Default for Hysteresis {
    fn default() -> Self {
        Self::new()
    }
}

impl Hysteresis {
    pub fn new() -> Self {
        Self {
            rising_threshold: 75.0,
            falling_threshold: 70.0,
            high_value: 100.0,
            low_value: 0.0,
            output: 0.0,
        }
    }

    pub fn execute(&mut self, input: f64) -> f64 {
        if input >= self.rising_threshold {
            self.output = self.high_value;
        } else if input <= self.falling_threshold {
            self.output = self.low_value;
        }
        // Between thresholds: hold current value.
        self.output
    }

    pub fn output(&self) -> f64 {
        self.output
    }
}

// ── Scheduling Components ───────────────────────────────────

/// A single entry in a daily float schedule.
#[derive(Debug, Clone)]
pub struct ScheduleEntry {
    /// Hour of day (0-23).
    pub hour: u8,
    /// Minute of hour (0-59).
    pub minute: u8,
    /// Value to output at this time.
    pub value: f64,
}

impl ScheduleEntry {
    /// Convert to minutes since midnight for comparison.
    fn minutes_since_midnight(&self) -> u16 {
        self.hour as u16 * 60 + self.minute as u16
    }
}

/// Time-of-day based float schedule.
///
/// Schedule entries are evaluated in order. The output value is determined
/// by the last entry whose time has passed. If no entry has passed (i.e.,
/// current time is before all entries), the default value is used.
#[derive(Debug, Clone)]
pub struct DailyScheduleFloat {
    pub entries: Vec<ScheduleEntry>,
    pub default_value: f64,
    pub enabled: bool,
}

impl Default for DailyScheduleFloat {
    fn default() -> Self {
        Self::new()
    }
}

impl DailyScheduleFloat {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            default_value: 0.0,
            enabled: true,
        }
    }

    /// Given current time (hour, minute), returns the scheduled value.
    ///
    /// Finds the latest entry whose time is at or before the current time.
    /// If no entry matches, returns `default_value`.
    pub fn evaluate(&self, hour: u8, minute: u8) -> f64 {
        if !self.enabled || self.entries.is_empty() {
            return self.default_value;
        }

        let now_mins = hour as u16 * 60 + minute as u16;
        let mut best: Option<&ScheduleEntry> = None;

        for entry in &self.entries {
            let entry_mins = entry.minutes_since_midnight();
            if entry_mins <= now_mins {
                match best {
                    None => best = Some(entry),
                    Some(prev) => {
                        if entry_mins >= prev.minutes_since_midnight() {
                            best = Some(entry);
                        }
                    }
                }
            }
        }

        match best {
            Some(entry) => entry.value,
            None => self.default_value,
        }
    }
}

/// A single entry in a daily bool schedule.
#[derive(Debug, Clone)]
pub struct BoolScheduleEntry {
    /// Hour of day (0-23).
    pub hour: u8,
    /// Minute of hour (0-59).
    pub minute: u8,
    /// Value to output at this time.
    pub value: bool,
}

impl BoolScheduleEntry {
    /// Convert to minutes since midnight for comparison.
    fn minutes_since_midnight(&self) -> u16 {
        self.hour as u16 * 60 + self.minute as u16
    }
}

/// Time-of-day based bool schedule.
#[derive(Debug, Clone)]
pub struct DailyScheduleBool {
    pub entries: Vec<BoolScheduleEntry>,
    pub default_value: bool,
    pub enabled: bool,
}

impl Default for DailyScheduleBool {
    fn default() -> Self {
        Self::new()
    }
}

impl DailyScheduleBool {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            default_value: false,
            enabled: true,
        }
    }

    /// Given current time (hour, minute), returns the scheduled value.
    pub fn evaluate(&self, hour: u8, minute: u8) -> bool {
        if !self.enabled || self.entries.is_empty() {
            return self.default_value;
        }

        let now_mins = hour as u16 * 60 + minute as u16;
        let mut best: Option<&BoolScheduleEntry> = None;

        for entry in &self.entries {
            let entry_mins = entry.minutes_since_midnight();
            if entry_mins <= now_mins {
                match best {
                    None => best = Some(entry),
                    Some(prev) => {
                        if entry_mins >= prev.minutes_since_midnight() {
                            best = Some(entry);
                        }
                    }
                }
            }
        }

        match best {
            Some(entry) => entry.value,
            None => self.default_value,
        }
    }
}

// ── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Helper: create an Instant advanced by given milliseconds.
    fn advance(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    // ── Add2 ────────────────────────────────────────────────

    #[test]
    fn test_add2_basic() {
        let mut a = Add2::new();
        a.in1 = 3.0;
        a.in2 = 4.0;
        assert!((a.execute() - 7.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_add2_negative() {
        let mut a = Add2::new();
        a.in1 = -5.0;
        a.in2 = 3.0;
        assert!((a.execute() - (-2.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_add2_default() {
        let a = Add2::default();
        assert!((a.execute() - 0.0).abs() < f64::EPSILON);
    }

    // ── Sub2 ────────────────────────────────────────────────

    #[test]
    fn test_sub2_basic() {
        let mut s = Sub2::new();
        s.in1 = 10.0;
        s.in2 = 3.0;
        assert!((s.execute() - 7.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_sub2_negative_result() {
        let mut s = Sub2::new();
        s.in1 = 3.0;
        s.in2 = 10.0;
        assert!((s.execute() - (-7.0)).abs() < f64::EPSILON);
    }

    // ── Mul2 ────────────────────────────────────────────────

    #[test]
    fn test_mul2_basic() {
        let mut m = Mul2::new();
        m.in1 = 3.0;
        m.in2 = 4.0;
        assert!((m.execute() - 12.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_mul2_zero() {
        let mut m = Mul2::new();
        m.in1 = 100.0;
        m.in2 = 0.0;
        assert!((m.execute() - 0.0).abs() < f64::EPSILON);
    }

    // ── Div2 ────────────────────────────────────────────────

    #[test]
    fn test_div2_basic() {
        let mut d = Div2::new();
        d.in1 = 10.0;
        d.in2 = 4.0;
        assert!((d.execute() - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_div2_divide_by_zero() {
        let mut d = Div2::new();
        d.in1 = 10.0;
        d.in2 = 0.0;
        assert!((d.execute() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_div2_default() {
        let d = Div2::default();
        // 0.0 / 0.0 -> 0.0 (divide by zero guard).
        assert!((d.execute() - 0.0).abs() < f64::EPSILON);
    }

    // ── Neg ─────────────────────────────────────────────────

    #[test]
    fn test_neg_positive() {
        let mut n = Neg::new();
        n.input = 5.0;
        assert!((n.execute() - (-5.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_neg_negative() {
        let mut n = Neg::new();
        n.input = -3.0;
        assert!((n.execute() - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_neg_zero() {
        let n = Neg::default();
        assert!((n.execute() - 0.0).abs() < f64::EPSILON);
    }

    // ── Round ───────────────────────────────────────────────

    #[test]
    fn test_round_zero_decimals() {
        let mut r = Round::new();
        r.input = 3.7;
        r.decimals = 0;
        assert!((r.execute() - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_round_two_decimals() {
        let mut r = Round::new();
        r.input = 3.14159;
        r.decimals = 2;
        assert!((r.execute() - 3.14).abs() < 0.001);
    }

    #[test]
    fn test_round_negative() {
        let mut r = Round::new();
        r.input = -2.555;
        r.decimals = 1;
        assert!((r.execute() - (-2.6)).abs() < 0.001);
    }

    // ── FloatOffset ─────────────────────────────────────────

    #[test]
    fn test_float_offset_basic() {
        let mut fo = FloatOffset::new();
        fo.input = 70.0;
        fo.offset = 2.5;
        assert!((fo.execute() - 72.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_float_offset_negative() {
        let mut fo = FloatOffset::new();
        fo.input = 70.0;
        fo.offset = -5.0;
        assert!((fo.execute() - 65.0).abs() < f64::EPSILON);
    }

    // ── And2 ────────────────────────────────────────────────

    #[test]
    fn test_and2_truth_table() {
        let mut a = And2::new();

        a.in1 = false;
        a.in2 = false;
        assert!(!a.execute());

        a.in1 = true;
        a.in2 = false;
        assert!(!a.execute());

        a.in1 = false;
        a.in2 = true;
        assert!(!a.execute());

        a.in1 = true;
        a.in2 = true;
        assert!(a.execute());
    }

    // ── Or2 ─────────────────────────────────────────────────

    #[test]
    fn test_or2_truth_table() {
        let mut o = Or2::new();

        o.in1 = false;
        o.in2 = false;
        assert!(!o.execute());

        o.in1 = true;
        o.in2 = false;
        assert!(o.execute());

        o.in1 = false;
        o.in2 = true;
        assert!(o.execute());

        o.in1 = true;
        o.in2 = true;
        assert!(o.execute());
    }

    // ── Not ─────────────────────────────────────────────────

    #[test]
    fn test_not_gate() {
        let mut n = Not::new();
        assert!(n.execute()); // !false = true

        n.input = true;
        assert!(!n.execute()); // !true = false
    }

    // ── SRLatch ─────────────────────────────────────────────

    #[test]
    fn test_sr_latch_set() {
        let mut latch = SRLatch::new();
        assert!(!latch.output());

        latch.set = true;
        assert!(latch.execute());
        assert!(latch.output());
    }

    #[test]
    fn test_sr_latch_reset() {
        let mut latch = SRLatch::new();
        latch.set = true;
        latch.execute();
        assert!(latch.output());

        latch.set = false;
        latch.reset = true;
        latch.execute();
        assert!(!latch.output());
    }

    #[test]
    fn test_sr_latch_set_priority() {
        let mut latch = SRLatch::new();
        // Both set and reset: set wins.
        latch.set = true;
        latch.reset = true;
        assert!(latch.execute());
    }

    #[test]
    fn test_sr_latch_hold() {
        let mut latch = SRLatch::new();
        latch.set = true;
        latch.execute();

        // Neither set nor reset: holds current value.
        latch.set = false;
        latch.reset = false;
        assert!(latch.execute());
    }

    // ── DelayOn ─────────────────────────────────────────────

    #[test]
    fn test_delay_on_basic() {
        let mut d = DelayOn::new();
        d.delay_ms = 500;
        let t0 = Instant::now();

        // Input goes true.
        d.input = true;
        assert!(!d.execute(t0)); // Not yet.
        assert!(!d.execute(advance(t0, 200))); // Not yet.
        assert!(d.execute(advance(t0, 500))); // Delay elapsed.
    }

    #[test]
    fn test_delay_on_reset_on_false() {
        let mut d = DelayOn::new();
        d.delay_ms = 500;
        let t0 = Instant::now();

        d.input = true;
        d.execute(t0);
        d.execute(advance(t0, 300));

        // Input goes false before delay.
        d.input = false;
        assert!(!d.execute(advance(t0, 400)));

        // Input goes true again: timer restarts.
        d.input = true;
        assert!(!d.execute(advance(t0, 500)));
        assert!(d.execute(advance(t0, 1000)));
    }

    #[test]
    fn test_delay_on_disabled() {
        let mut d = DelayOn::new();
        d.enabled = false;
        d.input = true;
        let t0 = Instant::now();
        assert!(!d.execute(t0));
        assert!(!d.execute(advance(t0, 5000)));
    }

    // ── DelayOff ────────────────────────────────────────────

    #[test]
    fn test_delay_off_basic() {
        let mut d = DelayOff::new();
        d.delay_ms = 500;
        let t0 = Instant::now();

        // Input goes true.
        d.input = true;
        assert!(d.execute(t0));

        // Input goes false: output stays true for delay.
        d.input = false;
        assert!(d.execute(advance(t0, 100)));
        assert!(d.execute(advance(t0, 400)));
        // After delay: output goes false.
        assert!(!d.execute(advance(t0, 600)));
    }

    #[test]
    fn test_delay_off_retrigger() {
        let mut d = DelayOff::new();
        d.delay_ms = 500;
        let t0 = Instant::now();

        d.input = true;
        d.execute(t0);

        d.input = false;
        d.execute(advance(t0, 100)); // start delay

        // Input goes true again before delay expires.
        d.input = true;
        d.execute(advance(t0, 200));

        // Goes false again: timer restarts.
        d.input = false;
        assert!(d.execute(advance(t0, 300)));
        assert!(d.execute(advance(t0, 700)));
        assert!(!d.execute(advance(t0, 900)));
    }

    // ── OneShot ─────────────────────────────────────────────

    #[test]
    fn test_oneshot_basic() {
        let mut os = OneShot::new();
        os.duration_ms = 200;
        let t0 = Instant::now();

        // Rising edge triggers pulse.
        os.input = true;
        assert!(os.execute(t0));
        assert!(os.execute(advance(t0, 100)));
        // Pulse expires.
        assert!(!os.execute(advance(t0, 200)));
    }

    #[test]
    fn test_oneshot_no_retrigger_while_held() {
        let mut os = OneShot::new();
        os.duration_ms = 200;
        let t0 = Instant::now();

        // Rising edge.
        os.input = true;
        os.execute(t0);

        // Input stays true -- no re-trigger.
        os.input = true;
        os.execute(advance(t0, 100));
        assert!(!os.execute(advance(t0, 300))); // expired

        // Still high, no new edge.
        assert!(!os.execute(advance(t0, 400)));
    }

    #[test]
    fn test_oneshot_retrigger_after_release() {
        let mut os = OneShot::new();
        os.duration_ms = 200;
        let t0 = Instant::now();

        os.input = true;
        os.execute(t0);
        os.execute(advance(t0, 300)); // expired

        // Release and re-trigger.
        os.input = false;
        os.execute(advance(t0, 400));

        os.input = true;
        assert!(os.execute(advance(t0, 500))); // new pulse
    }

    // ── Ramp ────────────────────────────────────────────────

    #[test]
    fn test_ramp_basic() {
        let mut ramp = Ramp::new();
        ramp.min = 0.0;
        ramp.max = 100.0;
        ramp.period_ms = 1000;

        let t0 = Instant::now();
        let out0 = ramp.execute(t0); // init
        assert!((out0 - 0.0).abs() < 0.001);

        let out50 = ramp.execute(advance(t0, 500));
        assert!((out50 - 50.0).abs() < 0.001);

        // Wraps around at period.
        let out_wrap = ramp.execute(advance(t0, 1200));
        assert!((out_wrap - 20.0).abs() < 0.001);
    }

    #[test]
    fn test_ramp_disabled() {
        let mut ramp = Ramp::new();
        ramp.enabled = false;
        let t0 = Instant::now();
        assert!((ramp.execute(t0) - 0.0).abs() < f64::EPSILON);
        assert!((ramp.execute(advance(t0, 5000)) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ramp_zero_period() {
        let mut ramp = Ramp::new();
        ramp.period_ms = 0;
        ramp.min = 50.0;
        let t0 = Instant::now();
        assert!((ramp.execute(t0) - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ramp_custom_range() {
        let mut ramp = Ramp::new();
        ramp.min = 50.0;
        ramp.max = 100.0;
        ramp.period_ms = 1000;

        let t0 = Instant::now();
        ramp.execute(t0); // init = 50.0

        let out = ramp.execute(advance(t0, 500));
        // 50% through: 50 + 25 = 75
        assert!((out - 75.0).abs() < 0.001);
    }

    #[test]
    fn test_ramp_reset() {
        let mut ramp = Ramp::new();
        ramp.min = 10.0;
        ramp.max = 110.0;
        ramp.period_ms = 1000;

        let t0 = Instant::now();
        ramp.execute(t0);
        ramp.execute(advance(t0, 500));

        ramp.reset();
        assert!((ramp.output() - 10.0).abs() < f64::EPSILON);
    }

    // ── Thermostat ──────────────────────────────────────────

    #[test]
    fn test_thermostat_heating_on() {
        let mut t = Thermostat::new();
        t.setpoint = 72.0;
        t.deadband = 2.0;
        t.heating = true;

        // Temperature drops below low threshold (71.0): heat turns on.
        assert!(t.execute(70.0));
    }

    #[test]
    fn test_thermostat_heating_off() {
        let mut t = Thermostat::new();
        t.setpoint = 72.0;
        t.deadband = 2.0;
        t.heating = true;

        t.execute(70.0); // on
                         // Temperature rises above high threshold (73.0): heat turns off.
        assert!(!t.execute(74.0));
    }

    #[test]
    fn test_thermostat_heating_deadband() {
        let mut t = Thermostat::new();
        t.setpoint = 72.0;
        t.deadband = 2.0;
        t.heating = true;

        t.execute(70.0); // on
                         // Temperature within deadband (71.5): holds current state (on).
        assert!(t.execute(71.5));
    }

    #[test]
    fn test_thermostat_cooling() {
        let mut t = Thermostat::new();
        t.setpoint = 72.0;
        t.deadband = 2.0;
        t.heating = false; // cooling mode

        // Temperature rises above high threshold (73.0): cooling turns on.
        assert!(t.execute(74.0));

        // Temperature drops below low threshold (71.0): cooling turns off.
        assert!(!t.execute(70.0));
    }

    #[test]
    fn test_thermostat_default() {
        let t = Thermostat::default();
        assert!((t.setpoint - 72.0).abs() < f64::EPSILON);
        assert!((t.deadband - 2.0).abs() < f64::EPSILON);
        assert!(t.heating);
        assert!(!t.output());
    }

    // ── Hysteresis ──────────────────────────────────────────

    #[test]
    fn test_hysteresis_basic() {
        let mut h = Hysteresis::new();
        h.rising_threshold = 75.0;
        h.falling_threshold = 70.0;
        h.high_value = 100.0;
        h.low_value = 0.0;

        // Below falling: output = low.
        assert!((h.execute(65.0) - 0.0).abs() < f64::EPSILON);

        // Above rising: output = high.
        assert!((h.execute(80.0) - 100.0).abs() < f64::EPSILON);

        // Between thresholds: holds high.
        assert!((h.execute(72.0) - 100.0).abs() < f64::EPSILON);

        // Below falling again: output = low.
        assert!((h.execute(69.0) - 0.0).abs() < f64::EPSILON);

        // Between thresholds: holds low.
        assert!((h.execute(72.0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_hysteresis_default() {
        let h = Hysteresis::default();
        assert!((h.rising_threshold - 75.0).abs() < f64::EPSILON);
        assert!((h.falling_threshold - 70.0).abs() < f64::EPSILON);
        assert!((h.output() - 0.0).abs() < f64::EPSILON);
    }

    // ── DailyScheduleFloat ──────────────────────────────────

    #[test]
    fn test_schedule_float_basic() {
        let mut sched = DailyScheduleFloat::new();
        sched.default_value = 72.0;
        sched.entries = vec![
            ScheduleEntry {
                hour: 6,
                minute: 0,
                value: 70.0,
            },
            ScheduleEntry {
                hour: 18,
                minute: 0,
                value: 76.0,
            },
            ScheduleEntry {
                hour: 22,
                minute: 0,
                value: 72.0,
            },
        ];

        // Before first entry: default.
        assert!((sched.evaluate(5, 0) - 72.0).abs() < f64::EPSILON);

        // At first entry.
        assert!((sched.evaluate(6, 0) - 70.0).abs() < f64::EPSILON);

        // Between first and second.
        assert!((sched.evaluate(12, 0) - 70.0).abs() < f64::EPSILON);

        // At second entry.
        assert!((sched.evaluate(18, 0) - 76.0).abs() < f64::EPSILON);

        // At third entry.
        assert!((sched.evaluate(22, 0) - 72.0).abs() < f64::EPSILON);

        // After last entry.
        assert!((sched.evaluate(23, 59) - 72.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_schedule_float_empty() {
        let sched = DailyScheduleFloat {
            entries: vec![],
            default_value: 55.0,
            enabled: true,
        };
        assert!((sched.evaluate(12, 0) - 55.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_schedule_float_disabled() {
        let mut sched = DailyScheduleFloat::new();
        sched.default_value = 72.0;
        sched.enabled = false;
        sched.entries = vec![ScheduleEntry {
            hour: 6,
            minute: 0,
            value: 70.0,
        }];
        assert!((sched.evaluate(12, 0) - 72.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_schedule_float_unsorted() {
        // Entries don't need to be sorted -- the algorithm picks the latest match.
        let mut sched = DailyScheduleFloat::new();
        sched.default_value = 60.0;
        sched.entries = vec![
            ScheduleEntry {
                hour: 18,
                minute: 0,
                value: 76.0,
            },
            ScheduleEntry {
                hour: 6,
                minute: 0,
                value: 70.0,
            },
            ScheduleEntry {
                hour: 22,
                minute: 0,
                value: 72.0,
            },
        ];

        assert!((sched.evaluate(12, 0) - 70.0).abs() < f64::EPSILON);
        assert!((sched.evaluate(20, 0) - 76.0).abs() < f64::EPSILON);
    }

    // ── DailyScheduleBool ───────────────────────────────────

    #[test]
    fn test_schedule_bool_basic() {
        let mut sched = DailyScheduleBool::new();
        sched.default_value = false;
        sched.entries = vec![
            BoolScheduleEntry {
                hour: 8,
                minute: 0,
                value: true,
            },
            BoolScheduleEntry {
                hour: 17,
                minute: 0,
                value: false,
            },
        ];

        assert!(!sched.evaluate(7, 0)); // before schedule: default (false)
        assert!(sched.evaluate(8, 0)); // at 8:00: true
        assert!(sched.evaluate(12, 0)); // midday: still true
        assert!(!sched.evaluate(17, 0)); // at 17:00: false
        assert!(!sched.evaluate(23, 0)); // evening: still false
    }

    #[test]
    fn test_schedule_bool_empty() {
        let sched = DailyScheduleBool {
            entries: vec![],
            default_value: true,
            enabled: true,
        };
        assert!(sched.evaluate(12, 0));
    }

    #[test]
    fn test_schedule_bool_disabled() {
        let mut sched = DailyScheduleBool::new();
        sched.enabled = false;
        sched.default_value = true;
        sched.entries = vec![BoolScheduleEntry {
            hour: 8,
            minute: 0,
            value: false,
        }];
        assert!(sched.evaluate(12, 0)); // disabled -> default (true)
    }
}
