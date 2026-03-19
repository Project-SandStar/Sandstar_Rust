//! Lead Sequencer for staged equipment control.
//!
//! Ported from Sedona `hvac::LSeq`. Divides a float input (0-100%)
//! into N equal bands, each controlling one boolean output stage.
//! Includes hysteresis to prevent rapid cycling.

/// Maximum number of output stages.
pub const MAX_STAGES: usize = 16;

/// Lead Sequencer configuration and state.
#[derive(Debug, Clone)]
pub struct LeadSequencer {
    /// Number of output stages (1-16).
    pub num_stages: usize,
    /// Input minimum (default 0.0).
    pub in_min: f64,
    /// Input maximum (default 100.0).
    pub in_max: f64,
    /// Hysteresis delta as fraction of band width (0.0-1.0, default 0.5).
    pub hysteresis: f64,

    // -- Runtime state --

    /// Current stage outputs.
    stages: Vec<bool>,
}

impl LeadSequencer {
    /// Create a new Lead Sequencer with the given number of stages.
    ///
    /// `num_stages` is clamped to 1..=16.
    pub fn new(num_stages: usize) -> Self {
        let n = num_stages.clamp(1, MAX_STAGES);
        Self {
            num_stages: n,
            in_min: 0.0,
            in_max: 100.0,
            hysteresis: 0.5,
            stages: vec![false; n],
        }
    }

    /// Execute one cycle. Returns slice of stage outputs.
    ///
    /// The input range `[in_min, in_max]` is divided into `num_stages` equal
    /// bands. Each stage turns on when the input reaches its band threshold,
    /// and turns off when the input drops below the threshold minus the
    /// hysteresis dead-band.
    pub fn execute(&mut self, input: f64) -> &[bool] {
        let range = self.in_max - self.in_min;
        if range <= 0.0 || self.num_stages == 0 {
            return &self.stages;
        }

        let band_width = range / self.num_stages as f64;

        for i in 0..self.num_stages {
            let threshold_on = self.in_min + band_width * i as f64;
            let threshold_off = threshold_on - (band_width * self.hysteresis);

            if input > threshold_on {
                self.stages[i] = true;
            } else if input < threshold_off {
                self.stages[i] = false;
            }
            // else: hold current state (inside hysteresis band)
        }

        &self.stages
    }

    /// Get current stage outputs.
    pub fn stages(&self) -> &[bool] {
        &self.stages
    }

    /// Get number of active (true) stages.
    pub fn active_count(&self) -> usize {
        self.stages.iter().filter(|&&s| s).count()
    }

    /// Resize the sequencer to a new number of stages.
    ///
    /// Resets all stages to false. `num_stages` is clamped to 1..=16.
    pub fn set_num_stages(&mut self, num_stages: usize) {
        let n = num_stages.clamp(1, MAX_STAGES);
        self.num_stages = n;
        self.stages = vec![false; n];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_seq_all_off_below_min() {
        let mut seq = LeadSequencer::new(4);
        // Input well below in_min: all stages off.
        seq.execute(-10.0);
        assert!(seq.stages().iter().all(|&s| !s));
        assert_eq!(seq.active_count(), 0);
    }

    #[test]
    fn test_seq_all_on_above_max() {
        let mut seq = LeadSequencer::new(4);
        seq.execute(100.0);
        assert!(seq.stages().iter().all(|&s| s));
        assert_eq!(seq.active_count(), 4);
    }

    #[test]
    fn test_seq_progressive_staging() {
        let mut seq = LeadSequencer::new(4);
        // 4 stages: bands at 0, 25, 50, 75. Strictly greater-than comparison.

        // At 0: all off (0 is not > 0).
        seq.execute(0.0);
        assert_eq!(seq.stages(), &[false, false, false, false]);

        // At 1: stage 0 on (1 > 0).
        seq.execute(1.0);
        assert_eq!(seq.stages(), &[true, false, false, false]);

        // At 30: stages 0,1 on.
        seq.execute(30.0);
        assert_eq!(seq.stages(), &[true, true, false, false]);

        // At 55: stages 0,1,2 on.
        seq.execute(55.0);
        assert_eq!(seq.stages(), &[true, true, true, false]);

        // At 80: all on.
        seq.execute(80.0);
        assert_eq!(seq.stages(), &[true, true, true, true]);
    }

    #[test]
    fn test_seq_hysteresis_prevents_cycling() {
        let mut seq = LeadSequencer::new(4);
        // Bands at 0, 25, 50, 75. hysteresis=0.5 -> dead-band = 12.5.
        // Stage 2: threshold_on=50, threshold_off=50-12.5=37.5.

        // Turn on stage 2 at 55.
        seq.execute(55.0);
        assert!(seq.stages()[2]);

        // Drop to 42 (above threshold_off=37.5): stage 2 should HOLD (stay on).
        seq.execute(42.0);
        assert!(
            seq.stages()[2],
            "hysteresis should keep stage 2 on at 42"
        );

        // Drop to 35 (below threshold_off=37.5): stage 2 should turn off.
        seq.execute(35.0);
        assert!(
            !seq.stages()[2],
            "stage 2 should turn off below threshold_off"
        );
    }

    #[test]
    fn test_seq_4_stages_equal_bands() {
        let seq = LeadSequencer::new(4);
        assert_eq!(seq.num_stages, 4);
        assert_eq!(seq.stages().len(), 4);

        // Band width = (100 - 0) / 4 = 25.
        // Thresholds: 0, 25, 50, 75.
        let band_width = (seq.in_max - seq.in_min) / seq.num_stages as f64;
        assert!((band_width - 25.0).abs() < 0.001);
    }

    #[test]
    fn test_seq_1_stage() {
        let mut seq = LeadSequencer::new(1);
        assert_eq!(seq.num_stages, 1);

        // Single stage: threshold_on=0.0, threshold_off=0-50=-50.0.
        // Band width = 100, hysteresis = 0.5 -> dead-band = 50.
        seq.execute(-1.0);
        // -1 is not > 0 and not < -50 -> hold (starts false).
        assert!(!seq.stages()[0]);

        // Exactly 0.0: not > 0.0, hold (still false).
        seq.execute(0.0);
        assert!(!seq.stages()[0]);

        // 1.0 > 0.0: turns on.
        seq.execute(1.0);
        assert!(seq.stages()[0]);

        // Back to -1: hysteresis should hold (not < -50).
        seq.execute(-1.0);
        assert!(seq.stages()[0], "hysteresis should hold single stage on");

        // Way below: turn off.
        seq.execute(-60.0);
        assert!(!seq.stages()[0]);
    }

    #[test]
    fn test_seq_custom_range() {
        let mut seq = LeadSequencer::new(3);
        seq.in_min = 20.0;
        seq.in_max = 80.0;
        // Band width = 60/3 = 20. Thresholds: 20, 40, 60.

        seq.execute(10.0);
        // 10 < 20 (threshold_on for stage 0). threshold_off = 20 - 10 = 10.
        // 10 is NOT < 10, so hold (starts false). Still false.
        assert_eq!(seq.active_count(), 0);

        seq.execute(25.0);
        // 25 >= 20: stage 0 on. 25 < 40: stage 1 off.
        assert_eq!(seq.stages(), &[true, false, false]);

        seq.execute(45.0);
        assert_eq!(seq.stages(), &[true, true, false]);

        seq.execute(80.0);
        assert_eq!(seq.stages(), &[true, true, true]);
    }

    #[test]
    fn test_seq_stage_count_change() {
        let mut seq = LeadSequencer::new(4);
        seq.execute(60.0);
        assert_eq!(seq.active_count(), 3); // stages 0,1,2 on

        // Change to 2 stages.
        seq.set_num_stages(2);
        assert_eq!(seq.num_stages, 2);
        assert_eq!(seq.stages().len(), 2);
        // All stages reset to false.
        assert_eq!(seq.active_count(), 0);

        // Re-execute.
        seq.execute(60.0);
        // 2 stages: bands at 0, 50. Both on at 60.
        assert_eq!(seq.stages(), &[true, true]);
    }

    #[test]
    fn test_seq_clamp_stages() {
        // 0 stages clamps to 1.
        let seq = LeadSequencer::new(0);
        assert_eq!(seq.num_stages, 1);

        // 20 stages clamps to 16.
        let seq = LeadSequencer::new(20);
        assert_eq!(seq.num_stages, MAX_STAGES);
    }

    #[test]
    fn test_seq_zero_hysteresis() {
        let mut seq = LeadSequencer::new(4);
        seq.hysteresis = 0.0;
        // threshold_off = threshold_on (no dead-band).

        seq.execute(55.0);
        assert_eq!(seq.stages(), &[true, true, true, false]);

        // Drop to 49.9: stage 2 (threshold_on=50) should turn off immediately.
        seq.execute(49.9);
        assert!(!seq.stages()[2], "zero hysteresis: should turn off immediately");
    }

    #[test]
    fn test_seq_just_above_threshold() {
        let mut seq = LeadSequencer::new(4);
        // Stage 1 threshold_on = 25.0. Strictly greater-than comparison.
        // Exact threshold: 25.0 is NOT > 25.0, so stage 1 stays off (hysteresis hold).
        seq.execute(25.0);
        assert!(!seq.stages()[1], "exact threshold should NOT turn on (strict >)");

        // Just above: 25.1 > 25.0 -> stage 1 on.
        seq.execute(25.1);
        assert!(seq.stages()[1], "should turn on just above threshold");
    }
}
