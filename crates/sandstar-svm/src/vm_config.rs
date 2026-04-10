//! Configurable VM limits — replaces hardcoded Sedona constants.
//!
//! The original Sedona VM has several hardcoded limits (16KB stack, 256
//! components, 256KB scode, etc.) that constrain scalability on modern
//! hardware.  [`VmConfig`] makes every limit configurable at construction
//! time while providing preset profiles for backwards compatibility and
//! for the BeagleBone target.

/// Memory address width for scode references.
///
/// Classic Sedona uses 16-bit *block* addresses (block index × 4 = byte
/// offset), limiting scode to 256KB.  Extended mode uses 32-bit byte
/// addresses, supporting up to 4GB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressWidth {
    /// Classic Sedona: 16-bit block addresses (256KB max scode).
    Block16,
    /// Extended: 32-bit byte addresses (4GB max scode).
    Byte32,
}

/// Configurable VM limits.
///
/// Every field has a sane default (see [`Default`] impl) that is 4-16x
/// larger than the original Sedona values, suitable for a 512MB
/// BeagleBone.  Use [`VmConfig::sedona_compat`] for strict
/// backwards-compatibility with the original Sedona VM limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmConfig {
    /// Maximum stack size in bytes (Sedona default: 16KB).
    pub max_stack_size: usize,
    /// Maximum component count (Sedona default: 256).
    pub max_components: u32,
    /// Maximum scode size in bytes (Sedona default: 256KB).
    pub max_code_size: usize,
    /// Maximum data segment size in bytes (Sedona default: 64KB).
    pub max_data_size: usize,
    /// Maximum call depth (Sedona default: 16).
    pub max_call_depth: usize,
    /// Maximum execution steps per tick (infinite-loop protection).
    pub max_steps_per_tick: u64,
    /// Address width for scode references.
    pub address_width: AddressWidth,
}

impl Default for VmConfig {
    /// Relaxed defaults suitable for a 512MB BeagleBone.
    ///
    /// Uses [`AddressWidth::Byte32`] to support scode images larger than
    /// 256KB.  Use [`VmConfig::sedona_compat`] for strict classic limits.
    fn default() -> Self {
        Self {
            max_stack_size: 64 * 1024,           // 64KB  (4x Sedona)
            max_components: 4096,                // 4096  (16x Sedona)
            max_code_size: 4 * 1024 * 1024,      // 4MB   (16x Sedona)
            max_data_size: 1024 * 1024,          // 1MB   (16x Sedona)
            max_call_depth: 64,                  // 64    (4x Sedona)
            max_steps_per_tick: 1_000_000,       // 1M steps
            address_width: AddressWidth::Byte32, // Extended for large scode
        }
    }
}

impl VmConfig {
    /// Sedona-compatible defaults — same limits as the original C VM.
    pub fn sedona_compat() -> Self {
        Self {
            max_stack_size: 16 * 1024, // 16KB
            max_components: 256,
            max_code_size: 256 * 1024, // 256KB
            max_data_size: 64 * 1024,  // 64KB
            max_call_depth: 16,
            max_steps_per_tick: 100_000,
            address_width: AddressWidth::Block16,
        }
    }

    /// Relaxed limits for BeagleBone (512MB RAM).
    ///
    /// Identical to [`Default::default`] — provided as a named
    /// constructor for clarity in configuration code.
    pub fn beaglebone() -> Self {
        Self::default()
    }

    /// Validate that the configuration is internally consistent.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_stack_size == 0 {
            return Err("max_stack_size must be > 0".into());
        }
        if self.max_components == 0 {
            return Err("max_components must be > 0".into());
        }
        if self.max_code_size == 0 {
            return Err("max_code_size must be > 0".into());
        }
        if self.max_data_size == 0 {
            return Err("max_data_size must be > 0".into());
        }
        if self.max_call_depth == 0 {
            return Err("max_call_depth must be > 0".into());
        }
        if self.max_steps_per_tick == 0 {
            return Err("max_steps_per_tick must be > 0".into());
        }
        // Block16 limits scode to 256KB
        if self.address_width == AddressWidth::Block16 && self.max_code_size > 256 * 1024 {
            return Err(format!(
                "Block16 addressing limits scode to 256KB, but max_code_size = {}",
                self.max_code_size
            ));
        }
        Ok(())
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let cfg = VmConfig::default();
        cfg.validate().unwrap();
    }

    #[test]
    fn sedona_compat_is_valid() {
        let cfg = VmConfig::sedona_compat();
        cfg.validate().unwrap();
    }

    #[test]
    fn beaglebone_equals_default() {
        assert_eq!(VmConfig::beaglebone(), VmConfig::default());
    }

    #[test]
    fn sedona_compat_limits() {
        let cfg = VmConfig::sedona_compat();
        assert_eq!(cfg.max_stack_size, 16 * 1024);
        assert_eq!(cfg.max_components, 256);
        assert_eq!(cfg.max_code_size, 256 * 1024);
        assert_eq!(cfg.max_data_size, 64 * 1024);
        assert_eq!(cfg.max_call_depth, 16);
        assert_eq!(cfg.max_steps_per_tick, 100_000);
        assert_eq!(cfg.address_width, AddressWidth::Block16);
    }

    #[test]
    fn default_limits_are_larger() {
        let def = VmConfig::default();
        let compat = VmConfig::sedona_compat();
        assert!(def.max_stack_size >= compat.max_stack_size);
        assert!(def.max_components >= compat.max_components);
        assert!(def.max_code_size >= compat.max_code_size);
        assert!(def.max_data_size >= compat.max_data_size);
        assert!(def.max_call_depth >= compat.max_call_depth);
        assert!(def.max_steps_per_tick >= compat.max_steps_per_tick);
    }

    #[test]
    fn validate_rejects_zero_stack() {
        let mut cfg = VmConfig::default();
        cfg.max_stack_size = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_components() {
        let mut cfg = VmConfig::default();
        cfg.max_components = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_code_size() {
        let mut cfg = VmConfig::default();
        cfg.max_code_size = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_data_size() {
        let mut cfg = VmConfig::default();
        cfg.max_data_size = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_call_depth() {
        let mut cfg = VmConfig::default();
        cfg.max_call_depth = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_steps() {
        let mut cfg = VmConfig::default();
        cfg.max_steps_per_tick = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_block16_code_size_limit() {
        let mut cfg = VmConfig::default();
        cfg.address_width = AddressWidth::Block16;
        cfg.max_code_size = 256 * 1024 + 1; // just over 256KB
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_byte32_allows_large_code() {
        let mut cfg = VmConfig::default();
        cfg.address_width = AddressWidth::Byte32;
        cfg.max_code_size = 16 * 1024 * 1024; // 16MB
        cfg.validate().unwrap();
    }

    #[test]
    fn address_width_debug_display() {
        assert_eq!(format!("{:?}", AddressWidth::Block16), "Block16");
        assert_eq!(format!("{:?}", AddressWidth::Byte32), "Byte32");
    }

    #[test]
    fn address_width_equality() {
        assert_eq!(AddressWidth::Block16, AddressWidth::Block16);
        assert_eq!(AddressWidth::Byte32, AddressWidth::Byte32);
        assert_ne!(AddressWidth::Block16, AddressWidth::Byte32);
    }

    #[test]
    fn config_clone() {
        let cfg = VmConfig::default();
        let cfg2 = cfg.clone();
        assert_eq!(cfg, cfg2);
    }
}
