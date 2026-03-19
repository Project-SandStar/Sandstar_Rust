//! Linux sysfs GPIO driver.
//!
//! Manages GPIO export/unexport lifecycle and reads/writes pin values
//! via `/sys/class/gpio/gpioN/value`.
//!
//! # Sysfs GPIO Interface
//!
//! The Linux kernel exposes GPIO pins under `/sys/class/gpio/`:
//!
//! - **Export**: Write pin number to `/sys/class/gpio/export` to create
//!   `/sys/class/gpio/gpioN/` directory.
//! - **Direction**: Write `"in"` or `"out"` to `.../gpioN/direction`.
//! - **Value**: Read/write `"0"` or `"1"` from `.../gpioN/value`.
//! - **Unexport**: Write pin number to `/sys/class/gpio/unexport` to remove.
//!
//! See: <https://www.kernel.org/doc/Documentation/gpio/sysfs.txt>
//!
//! # Design
//!
//! `LinuxGpio` tracks which pins have been exported in a `HashMap` and
//! auto-exports pins on first read/write if not already exported.

use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;

use sandstar_hal::HalError;

use crate::sysfs;

/// Tracks the state of an exported GPIO pin.
struct GpioPin {
    /// Whether this pin is configured as output (`true`) or input (`false`).
    is_output: bool,
    /// Cached file descriptor for value reads (lseek+read pattern).
    value_read_fd: Option<File>,
    /// Cached file descriptor for value writes (lseek+write pattern).
    value_write_fd: Option<File>,
}

/// Linux sysfs GPIO driver.
///
/// Provides read/write access to GPIO pins via the sysfs interface.
/// Tracks exported pins and auto-exports on first access.
pub struct LinuxGpio {
    /// Root path to the sysfs GPIO directory.
    /// Default: `/sys/class/gpio`. Override with `with_sysfs_root` for testing.
    sysfs_root: PathBuf,
    /// Map of exported pins: address -> pin state.
    pins: HashMap<u32, GpioPin>,
}

impl LinuxGpio {
    /// Create a new GPIO driver using the default sysfs path.
    pub fn new() -> Self {
        Self {
            sysfs_root: PathBuf::from("/sys/class/gpio"),
            pins: HashMap::new(),
        }
    }

    /// Create a new GPIO driver with a custom sysfs root (for testing).
    pub fn with_sysfs_root(root: PathBuf) -> Self {
        Self {
            sysfs_root: root,
            pins: HashMap::new(),
        }
    }

    /// Path to the gpio pin directory: `{root}/gpio{address}`
    fn pin_dir(&self, address: u32) -> PathBuf {
        self.sysfs_root.join(format!("gpio{}", address))
    }

    /// Path to the value file: `{root}/gpio{address}/value`
    fn value_path(&self, address: u32) -> PathBuf {
        self.pin_dir(address).join("value")
    }

    /// Path to the direction file: `{root}/gpio{address}/direction`
    fn direction_path(&self, address: u32) -> PathBuf {
        self.pin_dir(address).join("direction")
    }

    /// Ensure a pin is exported with the correct direction.
    ///
    /// If the pin is not tracked, exports it and sets direction.
    /// If it is tracked but direction differs, updates the direction.
    fn ensure_exported(&mut self, address: u32, output: bool) -> Result<(), HalError> {
        if let Some(pin) = self.pins.get(&address) {
            if pin.is_output == output {
                return Ok(());
            }
            // Direction changed, update it
            let direction = if output { "out" } else { "in" };
            sysfs::write(&self.direction_path(address), direction)?;
            // Update cached state — need to drop read/write FDs since direction changed
            let pin = self.pins.get_mut(&address).unwrap();
            pin.is_output = output;
            pin.value_read_fd = None;
            pin.value_write_fd = None;
            return Ok(());
        }

        // Pin not exported yet — export it
        self.export(address, output)
    }

    /// Read the digital value of a GPIO pin.
    ///
    /// Uses cached file descriptor for repeated reads (lseek+read pattern).
    /// Returns `true` for high (1), `false` for low (0).
    pub fn read(&mut self, address: u32) -> Result<bool, HalError> {
        let path = self.value_path(address);

        // Use cached fd if pin is tracked, otherwise fall back to uncached read
        let val: String = if let Some(pin) = self.pins.get_mut(&address) {
            sysfs::read_cached(&mut pin.value_read_fd, &path)
        } else {
            sysfs::read(&path)
        }
        .map_err(|e| HalError::DeviceError {
            device: 0,
            address,
            message: format!("gpio read failed: {}", e),
        })?;

        match val.as_str() {
            "1" => Ok(true),
            "0" => Ok(false),
            other => Err(HalError::DeviceError {
                device: 0,
                address,
                message: format!("unexpected gpio value: '{}'", other),
            }),
        }
    }

    /// Write a digital value to a GPIO pin.
    ///
    /// Auto-exports as output if not already exported.
    /// Writes `"1"` for `true` (high), `"0"` for `false` (low).
    pub fn write(&mut self, address: u32, value: bool) -> Result<(), HalError> {
        // Auto-export as output if not already exported
        self.ensure_exported(address, true)?;

        let path = self.value_path(address);
        let pin = self.pins.get_mut(&address).unwrap();
        let val = if value { "1" } else { "0" };

        sysfs::write_cached(&mut pin.value_write_fd, &path, val).map_err(|e| {
            HalError::DeviceError {
                device: 0,
                address,
                message: format!("gpio write failed: {}", e),
            }
        })
    }

    /// Export a GPIO pin and set its direction.
    ///
    /// Writes the pin number to `/sys/class/gpio/export`, then sets
    /// direction to `"out"` or `"in"` based on the `output` parameter.
    ///
    /// Matches C `gpio_export()` + `gpio_set_direction()`.
    pub fn export(&mut self, address: u32, output: bool) -> Result<(), HalError> {
        // If already tracked, just update direction
        if self.pins.contains_key(&address) {
            return self.ensure_exported(address, output);
        }

        let export_path = self.sysfs_root.join("export");
        let addr_str = address.to_string();

        // Export the pin (may fail if already exported by another process)
        if let Err(e) = sysfs::write(&export_path, &addr_str) {
            // Check if the pin directory already exists (exported externally)
            if !self.pin_dir(address).exists() {
                return Err(HalError::DeviceError {
                    device: 0,
                    address,
                    message: format!("gpio export failed: {}", e),
                });
            }
            // Pin already exported — proceed to set direction
        }

        // Set direction
        let direction = if output { "out" } else { "in" };
        sysfs::write(&self.direction_path(address), direction).map_err(|e| {
            HalError::DeviceError {
                device: 0,
                address,
                message: format!("gpio set direction failed: {}", e),
            }
        })?;

        // Track the pin
        self.pins.insert(
            address,
            GpioPin {
                is_output: output,
                value_read_fd: None,
                value_write_fd: None,
            },
        );

        Ok(())
    }

    /// Unexport a GPIO pin.
    ///
    /// Writes the pin number to `/sys/class/gpio/unexport` and removes
    /// it from the tracked pins map. Cached FDs are dropped automatically.
    pub fn unexport(&mut self, address: u32) -> Result<(), HalError> {
        // Remove from tracking (drops any cached FDs)
        self.pins.remove(&address);

        let unexport_path = self.sysfs_root.join("unexport");
        let addr_str = address.to_string();

        sysfs::write(&unexport_path, &addr_str).map_err(|e| HalError::DeviceError {
            device: 0,
            address,
            message: format!("gpio unexport failed: {}", e),
        })
    }
}

impl Default for LinuxGpio {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Build a mock sysfs GPIO tree under a temp directory.
    ///
    /// Creates:
    /// - `{root}/export` (writable file)
    /// - `{root}/unexport` (writable file)
    fn create_mock_gpio_root(dir: &TempDir) -> PathBuf {
        let root = dir.path().join("sys/class/gpio");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("export"), "").unwrap();
        fs::write(root.join("unexport"), "").unwrap();
        root
    }

    /// Create a mock exported pin directory with value and direction files.
    fn create_mock_pin(root: &PathBuf, address: u32, direction: &str, value: &str) {
        let pin_dir = root.join(format!("gpio{}", address));
        fs::create_dir_all(&pin_dir).unwrap();
        fs::write(pin_dir.join("direction"), direction).unwrap();
        fs::write(pin_dir.join("value"), value).unwrap();
    }

    #[test]
    fn test_read_value_high() {
        let dir = TempDir::new().unwrap();
        let root = create_mock_gpio_root(&dir);
        create_mock_pin(&root, 47, "in", "1\n");

        let mut gpio = LinuxGpio::with_sysfs_root(root);
        assert!(gpio.read(47).unwrap());
    }

    #[test]
    fn test_read_value_low() {
        let dir = TempDir::new().unwrap();
        let root = create_mock_gpio_root(&dir);
        create_mock_pin(&root, 47, "in", "0\n");

        let mut gpio = LinuxGpio::with_sysfs_root(root);
        assert!(!gpio.read(47).unwrap());
    }

    #[test]
    fn test_read_nonexistent_pin_returns_error() {
        let dir = TempDir::new().unwrap();
        let root = create_mock_gpio_root(&dir);

        let mut gpio = LinuxGpio::with_sysfs_root(root);
        assert!(gpio.read(999).is_err());
    }

    #[test]
    fn test_export_creates_direction() {
        let dir = TempDir::new().unwrap();
        let root = create_mock_gpio_root(&dir);
        // Pre-create the pin directory (simulating kernel creating it after export)
        create_mock_pin(&root, 47, "", "0");

        let mut gpio = LinuxGpio::with_sysfs_root(root.clone());
        gpio.export(47, true).unwrap();

        // Verify direction was set to "out"
        let direction = fs::read_to_string(root.join("gpio47/direction")).unwrap();
        assert_eq!(direction, "out");

        // Verify export file was written
        let export_content = fs::read_to_string(root.join("export")).unwrap();
        assert_eq!(export_content, "47");
    }

    #[test]
    fn test_export_input_pin() {
        let dir = TempDir::new().unwrap();
        let root = create_mock_gpio_root(&dir);
        create_mock_pin(&root, 60, "", "0");

        let mut gpio = LinuxGpio::with_sysfs_root(root.clone());
        gpio.export(60, false).unwrap();

        let direction = fs::read_to_string(root.join("gpio60/direction")).unwrap();
        assert_eq!(direction, "in");
    }

    #[test]
    fn test_write_sets_value() {
        let dir = TempDir::new().unwrap();
        let root = create_mock_gpio_root(&dir);
        create_mock_pin(&root, 47, "", "0");

        let mut gpio = LinuxGpio::with_sysfs_root(root.clone());
        // Export first (auto-export will happen, but pin dir must exist)
        gpio.export(47, true).unwrap();

        gpio.write(47, true).unwrap();

        let value = fs::read_to_string(root.join("gpio47/value")).unwrap();
        assert!(value.starts_with("1"));
    }

    #[test]
    fn test_write_auto_exports() {
        let dir = TempDir::new().unwrap();
        let root = create_mock_gpio_root(&dir);
        // Pre-create pin directory (kernel does this on export)
        create_mock_pin(&root, 47, "", "0");

        let mut gpio = LinuxGpio::with_sysfs_root(root.clone());
        // Write without explicit export — should auto-export as output
        gpio.write(47, true).unwrap();

        let direction = fs::read_to_string(root.join("gpio47/direction")).unwrap();
        assert_eq!(direction, "out");
    }

    #[test]
    fn test_unexport_writes_to_unexport_file() {
        let dir = TempDir::new().unwrap();
        let root = create_mock_gpio_root(&dir);
        create_mock_pin(&root, 47, "out", "1");

        let mut gpio = LinuxGpio::with_sysfs_root(root.clone());
        gpio.export(47, true).unwrap();
        gpio.unexport(47).unwrap();

        let unexport_content = fs::read_to_string(root.join("unexport")).unwrap();
        assert_eq!(unexport_content, "47");
    }

    #[test]
    fn test_unexport_removes_tracking() {
        let dir = TempDir::new().unwrap();
        let root = create_mock_gpio_root(&dir);
        create_mock_pin(&root, 47, "out", "1");

        let mut gpio = LinuxGpio::with_sysfs_root(root);
        gpio.export(47, true).unwrap();
        assert!(gpio.pins.contains_key(&47));

        gpio.unexport(47).unwrap();
        assert!(!gpio.pins.contains_key(&47));
    }

    #[test]
    fn test_multiple_pins() {
        let dir = TempDir::new().unwrap();
        let root = create_mock_gpio_root(&dir);
        create_mock_pin(&root, 47, "", "0");
        create_mock_pin(&root, 60, "", "0");
        create_mock_pin(&root, 115, "", "1");

        let mut gpio = LinuxGpio::with_sysfs_root(root.clone());
        gpio.export(47, true).unwrap();
        gpio.export(60, true).unwrap();
        gpio.export(115, false).unwrap();

        assert_eq!(gpio.pins.len(), 3);

        // Read an input pin
        assert!(gpio.read(115).unwrap());

        // Write output pins
        gpio.write(47, true).unwrap();
        gpio.write(60, false).unwrap();

        let v47 = fs::read_to_string(root.join("gpio47/value")).unwrap();
        assert!(v47.starts_with("1"));

        let v60 = fs::read_to_string(root.join("gpio60/value")).unwrap();
        assert!(v60.starts_with("0"));
    }

    #[test]
    fn test_default_creates_with_sys_class_gpio() {
        let gpio = LinuxGpio::default();
        assert_eq!(gpio.sysfs_root, PathBuf::from("/sys/class/gpio"));
    }
}
