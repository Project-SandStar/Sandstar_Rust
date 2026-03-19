//! SDP610/SDP810 differential pressure sensor conversion functions.
//!
//! These implement the physics conversions for the Sensirion SDP810 sensor:
//! - Raw I2C value -> Pascal (differential pressure)
//! - Pascal -> Inches of water column
//! - Pascal -> PSI
//! - Pascal -> CFM (volumetric airflow via duct K-factor)
//! - Pascal -> LPS (liters per second)

/// Pascal to inches of water column conversion factor.
const PA_TO_INCHES_WC: f64 = 0.004_018_65;

/// Pascal to PSI conversion factor.
const PA_TO_PSI: f64 = 0.000_145_038;

/// CFM to liters per second conversion factor.
const CFM_TO_LPS: f64 = 0.4719;

/// Default dead band (raw units below which CFM = 0).
pub const DEFAULT_DEAD_BAND: f64 = 5.0;

/// Default K-factor for CFM calculation: CFM = kFactor * sqrt(inH2O).
pub const DEFAULT_K_FACTOR: f64 = 14_000.0;

/// Default hysteresis ON threshold (raw units).
pub const DEFAULT_HYST_ON: f64 = 16.0;

/// Default hysteresis OFF threshold (raw units).
pub const DEFAULT_HYST_OFF: f64 = 8.0;

/// Default scale factor (60 for SDP810-500Pa, 240 for SDP810-125Pa).
pub const DEFAULT_SCALE_FACTOR: f64 = 60.0;

/// Convert raw I2C value to Pascal.
///
/// Formula: Pa = raw / scale_factor
pub fn raw_to_pa(raw: f64, scale_factor: f64) -> f64 {
    raw / scale_factor
}

/// Convert raw I2C value to inches of water column.
///
/// Formula: inH2O = (raw / scale_factor) * PA_TO_INCHES_WC
pub fn raw_to_inh2o(raw: f64, scale_factor: f64) -> f64 {
    raw_to_pa(raw, scale_factor) * PA_TO_INCHES_WC
}

/// Convert raw I2C value to PSI.
///
/// Formula: PSI = (raw / scale_factor) * PA_TO_PSI
pub fn raw_to_psi(raw: f64, scale_factor: f64) -> f64 {
    raw_to_pa(raw, scale_factor) * PA_TO_PSI
}

/// Convert raw I2C value to CFM (cubic feet per minute).
///
/// Formula: CFM = k_factor * sqrt(inH2O), with dead band.
/// If raw < dead_band, returns 0 (noise floor).
pub fn raw_to_cfm(raw: f64, k_factor: f64, dead_band: f64, scale_factor: f64) -> f64 {
    if raw < dead_band {
        return 0.0;
    }

    let inh2o = raw_to_inh2o(raw, scale_factor);
    if inh2o < 0.0 {
        return 0.0;
    }

    k_factor * inh2o.sqrt()
}

/// Convert raw I2C value to liters per second.
///
/// Formula: LPS = CFM * 0.4719
pub fn raw_to_lps(raw: f64, k_factor: f64, dead_band: f64, scale_factor: f64) -> f64 {
    raw_to_cfm(raw, k_factor, dead_band, scale_factor) * CFM_TO_LPS
}

/// Apply hysteresis (Schmitt trigger) to a raw flow reading.
///
/// State transitions:
/// - NOT_DETECTED + raw >= hyst_on  => DETECTED, return raw
/// - NOT_DETECTED + raw <  hyst_on  => NOT_DETECTED, return 0
/// - DETECTED     + raw >= hyst_off => DETECTED, return raw
/// - DETECTED     + raw <  hyst_off => NOT_DETECTED, return 0
/// - Any state    + raw <= 0        => NOT_DETECTED, return 0
pub fn apply_hysteresis(raw: f64, hyst_on: f64, hyst_off: f64, flow_detected: &mut bool) -> f64 {
    if raw <= 0.0 {
        *flow_detected = false;
        return 0.0;
    }

    if *flow_detected {
        // Currently detecting flow
        if raw >= hyst_off {
            raw
        } else {
            *flow_detected = false;
            0.0
        }
    } else {
        // Not currently detecting flow
        if raw >= hyst_on {
            *flow_detected = true;
            raw
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_to_pa() {
        // SDP810-500Pa: scale_factor = 60
        let pa = raw_to_pa(600.0, 60.0);
        assert!((pa - 10.0).abs() < 1e-10);

        // SDP810-125Pa: scale_factor = 240
        let pa = raw_to_pa(2400.0, 240.0);
        assert!((pa - 10.0).abs() < 1e-10);
    }

    #[test]
    fn test_raw_to_inh2o() {
        // 60 raw / 60 scale = 1 Pa, * 0.00401865 = 0.00401865 inH2O
        let inh2o = raw_to_inh2o(60.0, 60.0);
        assert!((inh2o - PA_TO_INCHES_WC).abs() < 1e-10);
    }

    #[test]
    fn test_raw_to_psi() {
        let psi = raw_to_psi(60.0, 60.0);
        assert!((psi - PA_TO_PSI).abs() < 1e-10);
    }

    #[test]
    fn test_raw_to_cfm_dead_band() {
        // Below dead band -> 0
        assert_eq!(raw_to_cfm(3.0, DEFAULT_K_FACTOR, DEFAULT_DEAD_BAND, DEFAULT_SCALE_FACTOR), 0.0);

        // At dead band -> 0
        assert_eq!(raw_to_cfm(4.9, DEFAULT_K_FACTOR, DEFAULT_DEAD_BAND, DEFAULT_SCALE_FACTOR), 0.0);

        // Above dead band -> positive CFM
        let cfm = raw_to_cfm(60.0, DEFAULT_K_FACTOR, DEFAULT_DEAD_BAND, DEFAULT_SCALE_FACTOR);
        assert!(cfm > 0.0);
    }

    #[test]
    fn test_raw_to_cfm_formula() {
        // raw=60, scale=60 -> Pa=1 -> inH2O = 0.00401865
        // CFM = 14000 * sqrt(0.00401865) = 14000 * 0.06339 ≈ 887.5
        let cfm = raw_to_cfm(60.0, DEFAULT_K_FACTOR, DEFAULT_DEAD_BAND, DEFAULT_SCALE_FACTOR);
        let expected_inh2o = PA_TO_INCHES_WC; // 1 Pa
        let expected_cfm = DEFAULT_K_FACTOR * expected_inh2o.sqrt();
        assert!((cfm - expected_cfm).abs() < 0.01);
    }

    #[test]
    fn test_raw_to_lps() {
        let cfm = raw_to_cfm(60.0, DEFAULT_K_FACTOR, DEFAULT_DEAD_BAND, DEFAULT_SCALE_FACTOR);
        let lps = raw_to_lps(60.0, DEFAULT_K_FACTOR, DEFAULT_DEAD_BAND, DEFAULT_SCALE_FACTOR);
        assert!((lps - cfm * CFM_TO_LPS).abs() < 1e-10);
    }

    #[test]
    fn test_hysteresis_basic() {
        let mut detected = false;

        // Below on threshold -> stays off
        assert_eq!(apply_hysteresis(10.0, DEFAULT_HYST_ON, DEFAULT_HYST_OFF, &mut detected), 0.0);
        assert!(!detected);

        // At on threshold -> turns on
        assert_eq!(apply_hysteresis(16.0, DEFAULT_HYST_ON, DEFAULT_HYST_OFF, &mut detected), 16.0);
        assert!(detected);

        // Above off threshold -> stays on
        assert_eq!(apply_hysteresis(10.0, DEFAULT_HYST_ON, DEFAULT_HYST_OFF, &mut detected), 10.0);
        assert!(detected);

        // Below off threshold -> turns off
        assert_eq!(apply_hysteresis(5.0, DEFAULT_HYST_ON, DEFAULT_HYST_OFF, &mut detected), 0.0);
        assert!(!detected);
    }

    #[test]
    fn test_hysteresis_negative() {
        let mut detected = true;

        // Negative value always turns off
        assert_eq!(apply_hysteresis(-1.0, DEFAULT_HYST_ON, DEFAULT_HYST_OFF, &mut detected), 0.0);
        assert!(!detected);
    }

    #[test]
    fn test_hysteresis_zero() {
        let mut detected = true;
        assert_eq!(apply_hysteresis(0.0, DEFAULT_HYST_ON, DEFAULT_HYST_OFF, &mut detected), 0.0);
        assert!(!detected);
    }

    #[test]
    fn test_cfm_negative_raw() {
        // Negative raw with dead_band=0 would give negative inH2O -> sqrt protection
        let cfm = raw_to_cfm(-10.0, DEFAULT_K_FACTOR, 0.0, DEFAULT_SCALE_FACTOR);
        assert_eq!(cfm, 0.0);
    }
}
