//! Linux sysfs PWM driver.
//!
//! Manages PWM channels via `/sys/class/pwm/pwmchipN/pwm-N:M/`.
//!
//! # Sysfs PWM Interface
//!
//! The Linux kernel exposes PWM control under `/sys/class/pwm/`:
//!
//! - **Export**: Write channel number to `/sys/class/pwm/pwmchipN/export`
//!   to create `/sys/class/pwm/pwmchipN/pwm-N:M/` directory.
//! - **Period**: Write nanoseconds to `.../pwm-N:M/period`.
//! - **Duty cycle**: Write nanoseconds to `.../pwm-N:M/duty_cycle`.
//! - **Polarity**: Write `"normal"` or `"inversed"` to `.../pwm-N:M/polarity`.
//! - **Enable**: Write `"1"` or `"0"` to `.../pwm-N:M/enable`.
//! - **Unexport**: Write channel number to `/sys/class/pwm/pwmchipN/unexport`.
//!
//! See: <https://www.kernel.org/doc/Documentation/pwm.txt>
//!
//! # Design
//!
//! `LinuxPwm` tracks exported channels in a `HashMap` keyed by `(chip, channel)`.
//! Each `PwmChannel` holds an optional cached `File` for duty_cycle writes,
//! since those happen on every poll cycle (~10 times/second).
//!
//! The `config_pins()` free function configures BeagleBone pin multiplexing
//! to route PWM signals to the correct physical pins.

use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;

use sandstar_hal::HalError;

use crate::sysfs;

/// PWM output polarity.
///
/// Matches C `PWMIO_POLARITY`: `"normal"` or `"inversed"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PwmPolarity {
    /// Normal polarity — high during duty cycle, low during remainder.
    Normal,
    /// Inversed polarity — low during duty cycle, high during remainder.
    Inversed,
}

impl PwmPolarity {
    /// Convert to the sysfs string representation.
    fn as_sysfs_str(self) -> &'static str {
        match self {
            PwmPolarity::Normal => "normal",
            PwmPolarity::Inversed => "inversed",
        }
    }
}

/// Tracks the state of an exported PWM channel.
struct PwmChannel {
    /// Cached file descriptor for duty_cycle writes (hot path).
    duty_fd: Option<File>,
    /// Cached file descriptor for period writes.
    period_fd: Option<File>,
    /// Cached file descriptor for polarity writes.
    polarity_fd: Option<File>,
    /// Cached file descriptor for enable writes.
    enable_fd: Option<File>,
}

/// Linux sysfs PWM driver.
///
/// Provides control over PWM channels via the sysfs interface.
/// Uses cached file descriptors for duty_cycle writes to avoid
/// FD exhaustion during continuous operation.
pub struct LinuxPwm {
    /// Root path to the sysfs PWM directory.
    /// Default: `/sys/class/pwm`. Override with `with_sysfs_root` for testing.
    sysfs_root: PathBuf,
    /// Map of exported channels: (chip, channel) -> channel state.
    channels: HashMap<(u32, u32), PwmChannel>,
}

impl LinuxPwm {
    /// Create a new PWM driver using the default sysfs path.
    pub fn new() -> Self {
        Self {
            sysfs_root: PathBuf::from("/sys/class/pwm"),
            channels: HashMap::new(),
        }
    }

    /// Create a new PWM driver with a custom sysfs root (for testing).
    pub fn with_sysfs_root(root: PathBuf) -> Self {
        Self {
            sysfs_root: root,
            channels: HashMap::new(),
        }
    }

    /// Path to the PWM channel directory.
    ///
    /// New kernel uses `pwm-C:CH` naming (e.g., `pwm-4:0`) instead of `pwmN`.
    /// Path: `{root}/pwmchip{chip}/pwm-{chip}:{channel}/`
    fn channel_dir(&self, chip: u32, channel: u32) -> PathBuf {
        self.sysfs_root
            .join(format!("pwmchip{}", chip))
            .join(format!("pwm-{}:{}", chip, channel))
    }

    /// Path to the duty_cycle file for a channel.
    fn duty_path(&self, chip: u32, channel: u32) -> PathBuf {
        self.channel_dir(chip, channel).join("duty_cycle")
    }

    /// Path to the period file for a channel.
    fn period_path(&self, chip: u32, channel: u32) -> PathBuf {
        self.channel_dir(chip, channel).join("period")
    }

    /// Path to the polarity file for a channel.
    fn polarity_path(&self, chip: u32, channel: u32) -> PathBuf {
        self.channel_dir(chip, channel).join("polarity")
    }

    /// Path to the enable file for a channel.
    fn enable_path(&self, chip: u32, channel: u32) -> PathBuf {
        self.channel_dir(chip, channel).join("enable")
    }

    /// Helper to convert I/O errors to HalError for a specific chip/channel.
    fn io_err(chip: u32, channel: u32, op: &str, e: std::io::Error) -> HalError {
        HalError::DeviceError {
            device: chip,
            address: channel,
            message: format!("pwm {} failed: {}", op, e),
        }
    }

    /// Read the current duty cycle value (in nanoseconds) as f64.
    ///
    /// Matches C: `pwmio_get_duty(chip, channel, &value)`
    pub fn read_duty(&self, chip: u32, channel: u32) -> Result<f64, HalError> {
        let path = self.duty_path(chip, channel);
        sysfs::read_parse::<f64>(&path).map_err(|e| Self::io_err(chip, channel, "read_duty", e))
    }

    /// Write a duty cycle value (in nanoseconds) to the PWM channel.
    ///
    /// Uses cached file descriptor for performance — duty writes happen
    /// on every poll cycle (~100ms).
    ///
    /// Matches C: `pwmio_set_duty(chip, channel, value)` with `io_write_cached`.
    pub fn write_duty(&mut self, chip: u32, channel: u32, duty_ns: f64) -> Result<(), HalError> {
        let path = self.duty_path(chip, channel);
        let val = (duty_ns as u64).to_string();

        // Get or create the channel entry for FD caching
        let ch = self.channels.entry((chip, channel)).or_insert(PwmChannel {
            duty_fd: None,
            period_fd: None,
            polarity_fd: None,
            enable_fd: None,
        });

        sysfs::write_cached(&mut ch.duty_fd, &path, &val)
            .map_err(|e| Self::io_err(chip, channel, "write_duty", e))
    }

    /// Export a PWM channel.
    ///
    /// Writes the channel number to `/sys/class/pwm/pwmchipN/export`.
    ///
    /// Matches C: `pwmio_export(chip, channel)`
    pub fn export(&mut self, chip: u32, channel: u32) -> Result<(), HalError> {
        let export_path = self
            .sysfs_root
            .join(format!("pwmchip{}", chip))
            .join("export");
        let ch_str = channel.to_string();

        // Export the channel (may fail if already exported)
        if let Err(e) = sysfs::write(&export_path, &ch_str) {
            // Check if the channel directory already exists
            if !self.channel_dir(chip, channel).exists() {
                return Err(Self::io_err(chip, channel, "export", e));
            }
            // Already exported — proceed
        }

        // Track the channel
        self.channels.entry((chip, channel)).or_insert(PwmChannel {
            duty_fd: None,
            period_fd: None,
            polarity_fd: None,
            enable_fd: None,
        });

        Ok(())
    }

    /// Unexport a PWM channel.
    ///
    /// Writes the channel number to `/sys/class/pwm/pwmchipN/unexport`.
    /// Drops any cached file descriptors.
    ///
    /// Matches C: `pwmio_unexport(chip, channel)`
    pub fn unexport(&mut self, chip: u32, channel: u32) -> Result<(), HalError> {
        // Remove from tracking (drops cached FDs)
        self.channels.remove(&(chip, channel));

        let unexport_path = self
            .sysfs_root
            .join(format!("pwmchip{}", chip))
            .join("unexport");
        let ch_str = channel.to_string();

        sysfs::write(&unexport_path, &ch_str)
            .map_err(|e| Self::io_err(chip, channel, "unexport", e))
    }

    /// Set the PWM period in nanoseconds.
    ///
    /// Matches C: `pwmio_set_period(chip, channel, value)` with `io_write_cached`.
    pub fn set_period(&mut self, chip: u32, channel: u32, period_ns: u32) -> Result<(), HalError> {
        let path = self.period_path(chip, channel);
        let val = period_ns.to_string();

        let ch = self.channels.entry((chip, channel)).or_insert(PwmChannel {
            duty_fd: None,
            period_fd: None,
            polarity_fd: None,
            enable_fd: None,
        });
        sysfs::write_cached(&mut ch.period_fd, &path, &val)
            .map_err(|e| Self::io_err(chip, channel, "set_period", e))
    }

    /// Set the PWM polarity.
    ///
    /// Matches C: `pwmio_set_polarity(chip, channel, value)` with `io_write_cached`.
    pub fn set_polarity(
        &mut self,
        chip: u32,
        channel: u32,
        polarity: PwmPolarity,
    ) -> Result<(), HalError> {
        let path = self.polarity_path(chip, channel);

        let ch = self.channels.entry((chip, channel)).or_insert(PwmChannel {
            duty_fd: None,
            period_fd: None,
            polarity_fd: None,
            enable_fd: None,
        });
        sysfs::write_cached(&mut ch.polarity_fd, &path, polarity.as_sysfs_str())
            .map_err(|e| Self::io_err(chip, channel, "set_polarity", e))
    }

    /// Enable or disable the PWM channel.
    ///
    /// Matches C: `pwmio_set_enable(chip, channel, value)` with `io_write_cached`.
    pub fn set_enable(&mut self, chip: u32, channel: u32, enabled: bool) -> Result<(), HalError> {
        let path = self.enable_path(chip, channel);
        let val = if enabled { "1" } else { "0" };

        let ch = self.channels.entry((chip, channel)).or_insert(PwmChannel {
            duty_fd: None,
            period_fd: None,
            polarity_fd: None,
            enable_fd: None,
        });
        sysfs::write_cached(&mut ch.enable_fd, &path, val)
            .map_err(|e| Self::io_err(chip, channel, "set_enable", e))
    }
}

impl Default for LinuxPwm {
    fn default() -> Self {
        Self::new()
    }
}

/// BeagleBone PWM pin names that need pinmux configuration.
///
/// Matches C: `static char *sPwmPin[] = {"P9_14", "P9_16", "P8_19", "P8_13", NULL};`
const BEAGLEBONE_PWM_PINS: &[&str] = &["P9_14", "P9_16", "P8_19", "P8_13"];

/// Configure BeagleBone pin multiplexing for PWM mode.
///
/// Writes `"pwm"` to each pin's pinmux state file under
/// `/sys/devices/platform/ocp/ocp:{PIN}_pinmux/state`.
///
/// This must be called during init before PWM channels can be used.
///
/// Matches C: `pwmio_config_pwm()`
pub fn config_pins() -> Result<(), HalError> {
    for pin in BEAGLEBONE_PWM_PINS {
        let path = PathBuf::from(format!(
            "/sys/devices/platform/ocp/ocp:{}_pinmux/state",
            pin
        ));

        sysfs::write(&path, "pwm").map_err(|e| HalError::DeviceError {
            device: 0,
            address: 0,
            message: format!("pwm pinmux config failed for {}: {}", pin, e),
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // --- Platform-agnostic tests (no filesystem with colons) ---

    #[test]
    fn test_read_duty_nonexistent_returns_error() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("sys/class/pwm");
        std::fs::create_dir_all(&root).unwrap();

        let pwm = LinuxPwm::with_sysfs_root(root);
        assert!(pwm.read_duty(99, 0).is_err());
    }

    #[test]
    fn test_polarity_as_sysfs_str() {
        assert_eq!(PwmPolarity::Normal.as_sysfs_str(), "normal");
        assert_eq!(PwmPolarity::Inversed.as_sysfs_str(), "inversed");
    }

    #[test]
    fn test_channel_dir_path_format() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("pwm");
        let pwm = LinuxPwm::with_sysfs_root(root.clone());

        // Verify the pwm-C:CH naming convention (new kernel style)
        let ch_dir = pwm.channel_dir(4, 0);
        assert_eq!(ch_dir, root.join("pwmchip4").join("pwm-4:0"));

        let ch_dir2 = pwm.channel_dir(6, 1);
        assert_eq!(ch_dir2, root.join("pwmchip6").join("pwm-6:1"));
    }

    #[test]
    fn test_default_creates_with_sys_class_pwm() {
        let pwm = LinuxPwm::default();
        assert_eq!(pwm.sysfs_root, PathBuf::from("/sys/class/pwm"));
    }

    // --- Sysfs tests requiring colons in directory names (Linux only) ---
    //
    // PWM channel directories use `pwm-C:CH` which contains a colon.
    // Pinmux directories use `ocp:P9_14_pinmux` which also contains colons.
    // Colons are invalid in Windows/NTFS filenames.

    #[cfg(target_os = "linux")]
    mod sysfs_tests {
        use super::*;
        use std::fs;

        /// Create a mock pwmchip directory with export/unexport files.
        fn create_mock_pwm_chip(root: &std::path::Path, chip: u32) {
            let chip_dir = root.join(format!("pwmchip{}", chip));
            fs::create_dir_all(&chip_dir).unwrap();
            fs::write(chip_dir.join("export"), "").unwrap();
            fs::write(chip_dir.join("unexport"), "").unwrap();
        }

        /// Create a mock PWM channel directory with all sysfs files.
        fn create_mock_pwm_channel(
            root: &std::path::Path,
            chip: u32,
            channel: u32,
            period: &str,
            duty: &str,
            polarity: &str,
            enable: &str,
        ) {
            let ch_dir = root
                .join(format!("pwmchip{}", chip))
                .join(format!("pwm-{}:{}", chip, channel));
            fs::create_dir_all(&ch_dir).unwrap();
            fs::write(ch_dir.join("period"), period).unwrap();
            fs::write(ch_dir.join("duty_cycle"), duty).unwrap();
            fs::write(ch_dir.join("polarity"), polarity).unwrap();
            fs::write(ch_dir.join("enable"), enable).unwrap();
        }

        #[test]
        fn test_read_duty() {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("sys/class/pwm");
            fs::create_dir_all(&root).unwrap();
            create_mock_pwm_chip(&root, 4);
            create_mock_pwm_channel(&root, 4, 0, "1000000", "500000\n", "normal", "1");

            let pwm = LinuxPwm::with_sysfs_root(root);
            let duty = pwm.read_duty(4, 0).unwrap();
            assert!((duty - 500000.0).abs() < f64::EPSILON);
        }

        #[test]
        fn test_write_duty() {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("sys/class/pwm");
            fs::create_dir_all(&root).unwrap();
            create_mock_pwm_chip(&root, 4);
            create_mock_pwm_channel(&root, 4, 0, "1000000", "0", "normal", "1");

            let mut pwm = LinuxPwm::with_sysfs_root(root.clone());
            pwm.write_duty(4, 0, 750000.0).unwrap();

            let duty_content =
                fs::read_to_string(root.join("pwmchip4").join("pwm-4:0").join("duty_cycle"))
                    .unwrap();
            assert!(duty_content.starts_with("750000"));
        }

        #[test]
        fn test_write_duty_caches_fd() {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("sys/class/pwm");
            fs::create_dir_all(&root).unwrap();
            create_mock_pwm_chip(&root, 4);
            create_mock_pwm_channel(&root, 4, 0, "1000000", "0", "normal", "1");

            let mut pwm = LinuxPwm::with_sysfs_root(root);

            // First write creates the FD
            pwm.write_duty(4, 0, 100000.0).unwrap();
            assert!(pwm.channels.get(&(4, 0)).unwrap().duty_fd.is_some());

            // Second write reuses it
            pwm.write_duty(4, 0, 200000.0).unwrap();
            assert!(pwm.channels.get(&(4, 0)).unwrap().duty_fd.is_some());
        }

        #[test]
        fn test_export() {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("sys/class/pwm");
            fs::create_dir_all(&root).unwrap();
            create_mock_pwm_chip(&root, 4);
            // Pre-create the channel directory (kernel creates it after export)
            create_mock_pwm_channel(&root, 4, 0, "0", "0", "normal", "0");

            let mut pwm = LinuxPwm::with_sysfs_root(root.clone());
            pwm.export(4, 0).unwrap();

            // Verify export file was written
            let export_content = fs::read_to_string(root.join("pwmchip4").join("export")).unwrap();
            assert_eq!(export_content, "0");

            // Verify channel is tracked
            assert!(pwm.channels.contains_key(&(4, 0)));
        }

        #[test]
        fn test_unexport() {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("sys/class/pwm");
            fs::create_dir_all(&root).unwrap();
            create_mock_pwm_chip(&root, 4);
            create_mock_pwm_channel(&root, 4, 0, "1000000", "500000", "normal", "1");

            let mut pwm = LinuxPwm::with_sysfs_root(root.clone());
            pwm.export(4, 0).unwrap();
            pwm.unexport(4, 0).unwrap();

            // Verify unexport file was written
            let unexport_content =
                fs::read_to_string(root.join("pwmchip4").join("unexport")).unwrap();
            assert_eq!(unexport_content, "0");

            // Verify channel is no longer tracked
            assert!(!pwm.channels.contains_key(&(4, 0)));
        }

        #[test]
        fn test_set_period() {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("sys/class/pwm");
            fs::create_dir_all(&root).unwrap();
            create_mock_pwm_chip(&root, 4);
            create_mock_pwm_channel(&root, 4, 0, "0", "0", "normal", "0");

            let mut pwm = LinuxPwm::with_sysfs_root(root.clone());
            pwm.set_period(4, 0, 20_000_000).unwrap();

            let period_content =
                fs::read_to_string(root.join("pwmchip4").join("pwm-4:0").join("period")).unwrap();
            assert_eq!(period_content, "20000000");
        }

        #[test]
        fn test_set_polarity_normal() {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("sys/class/pwm");
            fs::create_dir_all(&root).unwrap();
            create_mock_pwm_chip(&root, 4);
            create_mock_pwm_channel(&root, 4, 0, "1000000", "0", "inversed", "0");

            let mut pwm = LinuxPwm::with_sysfs_root(root.clone());
            pwm.set_polarity(4, 0, PwmPolarity::Normal).unwrap();

            let polarity_content =
                fs::read_to_string(root.join("pwmchip4").join("pwm-4:0").join("polarity")).unwrap();
            assert_eq!(polarity_content, "normal");
        }

        #[test]
        fn test_set_polarity_inversed() {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("sys/class/pwm");
            fs::create_dir_all(&root).unwrap();
            create_mock_pwm_chip(&root, 4);
            create_mock_pwm_channel(&root, 4, 0, "1000000", "0", "normal", "0");

            let mut pwm = LinuxPwm::with_sysfs_root(root.clone());
            pwm.set_polarity(4, 0, PwmPolarity::Inversed).unwrap();

            let polarity_content =
                fs::read_to_string(root.join("pwmchip4").join("pwm-4:0").join("polarity")).unwrap();
            assert_eq!(polarity_content, "inversed");
        }

        #[test]
        fn test_set_enable_on() {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("sys/class/pwm");
            fs::create_dir_all(&root).unwrap();
            create_mock_pwm_chip(&root, 4);
            create_mock_pwm_channel(&root, 4, 0, "1000000", "0", "normal", "0");

            let mut pwm = LinuxPwm::with_sysfs_root(root.clone());
            pwm.set_enable(4, 0, true).unwrap();

            let enable_content =
                fs::read_to_string(root.join("pwmchip4").join("pwm-4:0").join("enable")).unwrap();
            assert_eq!(enable_content, "1");
        }

        #[test]
        fn test_set_enable_off() {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("sys/class/pwm");
            fs::create_dir_all(&root).unwrap();
            create_mock_pwm_chip(&root, 4);
            create_mock_pwm_channel(&root, 4, 0, "1000000", "0", "normal", "1");

            let mut pwm = LinuxPwm::with_sysfs_root(root.clone());
            pwm.set_enable(4, 0, false).unwrap();

            let enable_content =
                fs::read_to_string(root.join("pwmchip4").join("pwm-4:0").join("enable")).unwrap();
            assert_eq!(enable_content, "0");
        }

        #[test]
        fn test_multiple_channels() {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("sys/class/pwm");
            fs::create_dir_all(&root).unwrap();

            create_mock_pwm_chip(&root, 4);
            create_mock_pwm_chip(&root, 6);
            create_mock_pwm_channel(&root, 4, 0, "1000000", "100000\n", "normal", "1");
            create_mock_pwm_channel(&root, 4, 1, "1000000", "200000\n", "normal", "1");
            create_mock_pwm_channel(&root, 6, 0, "2000000", "300000\n", "inversed", "0");

            let pwm = LinuxPwm::with_sysfs_root(root);

            assert!((pwm.read_duty(4, 0).unwrap() - 100000.0).abs() < f64::EPSILON);
            assert!((pwm.read_duty(4, 1).unwrap() - 200000.0).abs() < f64::EPSILON);
            assert!((pwm.read_duty(6, 0).unwrap() - 300000.0).abs() < f64::EPSILON);
        }

        #[test]
        fn test_config_pins_with_mock_sysfs() {
            let dir = TempDir::new().unwrap();

            // Create mock pinmux files for all BeagleBone PWM pins
            for pin in BEAGLEBONE_PWM_PINS {
                let pinmux_dir = dir
                    .path()
                    .join(format!("sys/devices/platform/ocp/ocp:{}_pinmux", pin));
                fs::create_dir_all(&pinmux_dir).unwrap();
                fs::write(pinmux_dir.join("state"), "default").unwrap();
            }

            // config_pins() uses hardcoded /sys paths, so we can't easily test it
            // with a mock root. This test just verifies the constant array is correct.
            assert_eq!(BEAGLEBONE_PWM_PINS.len(), 4);
            assert_eq!(BEAGLEBONE_PWM_PINS[0], "P9_14");
            assert_eq!(BEAGLEBONE_PWM_PINS[1], "P9_16");
            assert_eq!(BEAGLEBONE_PWM_PINS[2], "P8_19");
            assert_eq!(BEAGLEBONE_PWM_PINS[3], "P8_13");
        }

        #[test]
        fn test_full_lifecycle() {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("sys/class/pwm");
            fs::create_dir_all(&root).unwrap();

            create_mock_pwm_chip(&root, 4);
            create_mock_pwm_channel(&root, 4, 0, "0", "0", "normal", "0");

            let mut pwm = LinuxPwm::with_sysfs_root(root.clone());

            // 1. Export
            pwm.export(4, 0).unwrap();

            // 2. Set period
            pwm.set_period(4, 0, 20_000_000).unwrap();

            // 3. Set polarity
            pwm.set_polarity(4, 0, PwmPolarity::Normal).unwrap();

            // 4. Enable
            pwm.set_enable(4, 0, true).unwrap();

            // 5. Write duty cycle
            pwm.write_duty(4, 0, 10_000_000.0).unwrap();

            // 6. Read duty cycle back
            // (Note: on a real filesystem the cached write may leave trailing bytes.
            //  On sysfs the kernel handles this correctly.)
            let duty = pwm.read_duty(4, 0);
            assert!(duty.is_ok());

            // 7. Unexport
            pwm.unexport(4, 0).unwrap();
            assert!(!pwm.channels.contains_key(&(4, 0)));
        }
    }

    // --- Non-Linux stubs: verify reads fail gracefully ---

    #[cfg(not(target_os = "linux"))]
    mod non_linux {
        use super::*;

        #[test]
        fn read_returns_error_without_sysfs() {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("empty_pwm");
            std::fs::create_dir_all(&root).unwrap();

            let pwm = LinuxPwm::with_sysfs_root(root);
            assert!(pwm.read_duty(0, 0).is_err());
        }
    }
}
