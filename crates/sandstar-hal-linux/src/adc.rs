//! Linux IIO ADC driver.
//!
//! Reads raw ADC values from `/sys/bus/iio/devices/iio:deviceN/in_voltageM_raw`.
//!
//! # Sysfs IIO Interface
//!
//! The Linux Industrial I/O (IIO) subsystem exposes ADC channels under
//! `/sys/bus/iio/devices/`. Each ADC device is represented as `iio:deviceN`,
//! and each input channel is available as `in_voltageM_raw`.
//!
//! For example, reading AI1 (voltage channel 0 on device 0):
//! ```text
//! cat /sys/bus/iio/devices/iio:device0/in_voltage0_raw
//! 2048
//! ```
//!
//! The raw value is a 12-bit integer (0-4095) for the BeagleBone's ADC.
//! Conversion to engineering units happens in the engine's value module.
//!
//! # Design
//!
//! `LinuxAdc` is stateless — it just reads the sysfs file each time.
//! ADC reads are not as frequent as PWM writes, so no FD caching is needed.
//! Matches C `anio_get_value()`.

use std::path::PathBuf;

use sandstar_hal::HalError;

use crate::sysfs;

/// Linux IIO ADC driver.
///
/// Reads raw ADC values from the IIO sysfs interface.
pub struct LinuxAdc {
    /// Root path to the IIO devices directory.
    /// Default: `/sys/bus/iio/devices`. Override with `with_sysfs_root` for testing.
    sysfs_root: PathBuf,
}

impl LinuxAdc {
    /// Create a new ADC driver using the default sysfs path.
    pub fn new() -> Self {
        Self {
            sysfs_root: PathBuf::from("/sys/bus/iio/devices"),
        }
    }

    /// Create a new ADC driver with a custom sysfs root (for testing).
    pub fn with_sysfs_root(root: PathBuf) -> Self {
        Self { sysfs_root: root }
    }

    /// Build the sysfs path for a specific ADC channel.
    ///
    /// Returns: `{root}/iio:device{device}/in_voltage{address}_raw`
    fn channel_path(&self, device: u32, address: u32) -> PathBuf {
        self.sysfs_root
            .join(format!("iio:device{}", device))
            .join(format!("in_voltage{}_raw", address))
    }

    /// Read the raw ADC value for a given device and channel.
    ///
    /// Returns the raw count as `f64` (matching the C code which uses `strtod`).
    /// The engine's value conversion layer handles scaling to engineering units.
    ///
    /// Matches C: `anio_get_value(device, address, &value)`
    pub fn read(&self, device: u32, address: u32) -> Result<f64, HalError> {
        let path = self.channel_path(device, address);

        sysfs::read_parse::<f64>(&path).map_err(|e| HalError::DeviceError {
            device,
            address,
            message: format!("adc read failed: {}", e),
        })
    }
}

impl Default for LinuxAdc {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // --- Platform-agnostic tests (no filesystem with colons) ---

    #[test]
    fn test_default_creates_with_sys_bus_iio() {
        let adc = LinuxAdc::default();
        assert_eq!(adc.sysfs_root, PathBuf::from("/sys/bus/iio/devices"));
    }

    #[test]
    fn test_channel_path_format() {
        let adc = LinuxAdc::with_sysfs_root(PathBuf::from("/mock/iio"));
        let path = adc.channel_path(2, 5);
        assert_eq!(
            path,
            PathBuf::from("/mock/iio")
                .join("iio:device2")
                .join("in_voltage5_raw")
        );
    }

    #[test]
    fn test_read_nonexistent_device_returns_error() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("fake_iio");
        std::fs::create_dir_all(&root).unwrap();

        let adc = LinuxAdc::with_sysfs_root(root);
        assert!(adc.read(99, 0).is_err());
    }

    // --- Sysfs tests requiring colons in directory names (Linux only) ---
    //
    // IIO device directories use `iio:deviceN` which contains a colon.
    // Colons are invalid in Windows/NTFS filenames.

    #[cfg(target_os = "linux")]
    mod sysfs_tests {
        use super::*;
        use std::fs;

        fn create_mock_iio_channel(
            root: &std::path::Path,
            device: u32,
            address: u32,
            raw_value: &str,
        ) {
            let device_dir = root.join(format!("iio:device{}", device));
            fs::create_dir_all(&device_dir).unwrap();
            fs::write(
                device_dir.join(format!("in_voltage{}_raw", address)),
                raw_value,
            )
            .unwrap();
        }

        #[test]
        fn test_read_adc_value() {
            let dir = TempDir::new().unwrap();
            let root = dir
                .path()
                .join("sys")
                .join("bus")
                .join("iio")
                .join("devices");
            fs::create_dir_all(&root).unwrap();
            create_mock_iio_channel(&root, 0, 0, "2048\n");

            let adc = LinuxAdc::with_sysfs_root(root);
            let val = adc.read(0, 0).unwrap();
            assert!((val - 2048.0).abs() < f64::EPSILON);
        }

        #[test]
        fn test_read_adc_different_channels() {
            let dir = TempDir::new().unwrap();
            let root = dir
                .path()
                .join("sys")
                .join("bus")
                .join("iio")
                .join("devices");
            fs::create_dir_all(&root).unwrap();

            create_mock_iio_channel(&root, 0, 0, "1000\n");
            create_mock_iio_channel(&root, 0, 1, "2000\n");
            create_mock_iio_channel(&root, 0, 2, "3000\n");

            let adc = LinuxAdc::with_sysfs_root(root);
            assert!((adc.read(0, 0).unwrap() - 1000.0).abs() < f64::EPSILON);
            assert!((adc.read(0, 1).unwrap() - 2000.0).abs() < f64::EPSILON);
            assert!((adc.read(0, 2).unwrap() - 3000.0).abs() < f64::EPSILON);
        }

        #[test]
        fn test_read_adc_different_devices() {
            let dir = TempDir::new().unwrap();
            let root = dir
                .path()
                .join("sys")
                .join("bus")
                .join("iio")
                .join("devices");
            fs::create_dir_all(&root).unwrap();

            create_mock_iio_channel(&root, 0, 0, "100\n");
            create_mock_iio_channel(&root, 1, 0, "200\n");

            let adc = LinuxAdc::with_sysfs_root(root);
            assert!((adc.read(0, 0).unwrap() - 100.0).abs() < f64::EPSILON);
            assert!((adc.read(1, 0).unwrap() - 200.0).abs() < f64::EPSILON);
        }

        #[test]
        fn test_read_adc_nonexistent_channel_returns_error() {
            let dir = TempDir::new().unwrap();
            let root = dir
                .path()
                .join("sys")
                .join("bus")
                .join("iio")
                .join("devices");
            fs::create_dir_all(&root).unwrap();

            let device_dir = root.join("iio:device0");
            fs::create_dir_all(&device_dir).unwrap();

            let adc = LinuxAdc::with_sysfs_root(root);
            assert!(adc.read(0, 7).is_err());
        }

        #[test]
        fn test_read_adc_zero_value() {
            let dir = TempDir::new().unwrap();
            let root = dir
                .path()
                .join("sys")
                .join("bus")
                .join("iio")
                .join("devices");
            fs::create_dir_all(&root).unwrap();
            create_mock_iio_channel(&root, 0, 0, "0\n");

            let adc = LinuxAdc::with_sysfs_root(root);
            assert!(adc.read(0, 0).unwrap().abs() < f64::EPSILON);
        }

        #[test]
        fn test_read_adc_max_12bit_value() {
            let dir = TempDir::new().unwrap();
            let root = dir
                .path()
                .join("sys")
                .join("bus")
                .join("iio")
                .join("devices");
            fs::create_dir_all(&root).unwrap();
            create_mock_iio_channel(&root, 0, 0, "4095\n");

            let adc = LinuxAdc::with_sysfs_root(root);
            assert!((adc.read(0, 0).unwrap() - 4095.0).abs() < f64::EPSILON);
        }

        #[test]
        fn test_read_adc_invalid_content_returns_error() {
            let dir = TempDir::new().unwrap();
            let root = dir
                .path()
                .join("sys")
                .join("bus")
                .join("iio")
                .join("devices");
            fs::create_dir_all(&root).unwrap();
            create_mock_iio_channel(&root, 0, 0, "not_a_number\n");

            let adc = LinuxAdc::with_sysfs_root(root);
            assert!(adc.read(0, 0).is_err());
        }
    }

    // --- Non-Linux stubs: verify reads fail gracefully ---

    #[cfg(not(target_os = "linux"))]
    mod non_linux {
        use super::*;

        #[test]
        fn read_returns_error_without_sysfs() {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("empty_iio");
            std::fs::create_dir_all(&root).unwrap();

            let adc = LinuxAdc::with_sysfs_root(root);
            assert!(adc.read(0, 0).is_err());
        }
    }
}
