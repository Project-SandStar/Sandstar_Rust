//! Linux HAL implementations for BeagleBone hardware.
//!
//! Provides real hardware drivers that implement the `HalRead`, `HalWrite`,
//! `HalControl`, and `HalDiagnostics` traits from `sandstar-hal`.

use std::cell::RefCell;

use sandstar_hal::{HalControl, HalDiagnostics, HalError, HalRead, HalValidation, HalWrite};

pub mod adc;
pub mod crc;
pub mod gpio;
pub mod i2c;
pub mod pwm;
pub mod sysfs;
pub mod uart;

// Re-export sub-drivers for direct access
pub use adc::LinuxAdc;
pub use gpio::LinuxGpio;
pub use i2c::LinuxI2c;
pub use pwm::LinuxPwm;
pub use uart::LinuxUart;

/// Composite Linux HAL combining all hardware drivers.
///
/// Implements `HalRead + HalWrite` by delegating to the appropriate sub-driver.
/// Uses `RefCell` for interior mutability since trait methods take `&self`.
/// This is safe because the engine is single-threaded.
pub struct LinuxHal {
    gpio: RefCell<LinuxGpio>,
    adc: LinuxAdc,
    i2c: RefCell<LinuxI2c>,
    pwm: RefCell<LinuxPwm>,
    uart: RefCell<LinuxUart>,
}

impl LinuxHal {
    pub fn new() -> Self {
        Self {
            gpio: RefCell::new(LinuxGpio::new()),
            adc: LinuxAdc::new(),
            i2c: RefCell::new(LinuxI2c::new()),
            pwm: RefCell::new(LinuxPwm::new()),
            uart: RefCell::new(LinuxUart::new("/dev/ttyO")),
        }
    }
}

impl Default for LinuxHal {
    fn default() -> Self {
        Self::new()
    }
}

impl HalRead for LinuxHal {
    fn read_analog(&self, device: u32, address: u32) -> Result<f64, HalError> {
        self.adc.read(device, address)
    }

    fn read_digital(&self, address: u32) -> Result<bool, HalError> {
        self.gpio.borrow_mut().read(address)
    }

    fn read_i2c(&self, device: u32, address: u32, label: &str) -> Result<f64, HalError> {
        self.i2c.borrow().read_measurement(device, address, label)
    }

    fn read_pwm(&self, chip: u32, channel: u32) -> Result<f64, HalError> {
        self.pwm.borrow().read_duty(chip, channel)
    }

    fn read_uart(&self, device: u32, label: &str) -> Result<f64, HalError> {
        self.uart.borrow().read_measurement(device, label)
    }
}

impl HalWrite for LinuxHal {
    fn write_digital(&self, address: u32, value: bool) -> Result<(), HalError> {
        self.gpio.borrow_mut().write(address, value)
    }

    fn write_pwm(&self, chip: u32, channel: u32, duty: f64) -> Result<(), HalError> {
        self.pwm.borrow_mut().write_duty(chip, channel, duty)
    }
}

impl HalControl for LinuxHal {
    fn init(&mut self) -> Result<(), HalError> {
        // Best-effort: PWM pinmux failure is non-fatal (server may only need ADC/I2C)
        if let Err(e) = pwm::config_pins() {
            eprintln!("warning: PWM pinmux config failed (non-fatal): {e}");
        }
        Ok(())
    }

    fn validate(&self) -> HalValidation {
        let mut v = HalValidation::default();

        // I2C: check /dev/i2c-{0,1,2}
        let i2c_found = (0..=2).any(|n| std::path::Path::new(&format!("/dev/i2c-{n}")).exists());
        v.add(
            "i2c",
            i2c_found,
            if i2c_found {
                "I2C bus detected"
            } else {
                "no I2C buses found (/dev/i2c-*)"
            },
        );

        // ADC: check /sys/bus/iio/devices/iio:device*
        let iio_root = std::path::Path::new("/sys/bus/iio/devices");
        let adc_found = iio_root.exists()
            && std::fs::read_dir(iio_root)
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .any(|e| e.file_name().to_string_lossy().starts_with("iio:device"))
                })
                .unwrap_or(false);
        v.add(
            "adc",
            adc_found,
            if adc_found {
                "IIO ADC device detected"
            } else {
                "no IIO ADC devices found"
            },
        );

        // GPIO: check /sys/class/gpio/export
        let gpio_found = std::path::Path::new("/sys/class/gpio/export").exists();
        v.add(
            "gpio",
            gpio_found,
            if gpio_found {
                "GPIO sysfs available"
            } else {
                "/sys/class/gpio not found"
            },
        );

        // PWM: check /sys/class/pwm/pwmchip*
        let pwm_root = std::path::Path::new("/sys/class/pwm");
        let pwm_found = pwm_root.exists()
            && std::fs::read_dir(pwm_root)
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .any(|e| e.file_name().to_string_lossy().starts_with("pwmchip"))
                })
                .unwrap_or(false);
        v.add(
            "pwm",
            pwm_found,
            if pwm_found {
                "PWM chips available"
            } else {
                "no PWM chips found"
            },
        );

        // UART: check /dev/ttyO*
        let uart_found = (0..8).any(|n| std::path::Path::new(&format!("/dev/ttyO{n}")).exists());
        v.add(
            "uart",
            uart_found,
            if uart_found {
                "UART devices detected"
            } else {
                "no UART devices found (/dev/ttyO*)"
            },
        );

        v
    }

    fn gpio_export(&mut self, address: u32, output: bool) -> Result<(), HalError> {
        self.gpio.get_mut().export(address, output)
    }

    fn gpio_unexport(&mut self, address: u32) -> Result<(), HalError> {
        self.gpio.get_mut().unexport(address)
    }

    fn pwm_export(&mut self, chip: u32, channel: u32) -> Result<(), HalError> {
        self.pwm.get_mut().export(chip, channel)
    }

    fn pwm_configure(
        &mut self,
        chip: u32,
        channel: u32,
        period_ns: u32,
        polarity_normal: bool,
    ) -> Result<(), HalError> {
        let pwm = self.pwm.get_mut();
        pwm.set_period(chip, channel, period_ns)?;
        let pol = if polarity_normal {
            pwm::PwmPolarity::Normal
        } else {
            pwm::PwmPolarity::Inversed
        };
        pwm.set_polarity(chip, channel, pol)
    }

    fn pwm_enable(&mut self, chip: u32, channel: u32, enabled: bool) -> Result<(), HalError> {
        self.pwm.get_mut().set_enable(chip, channel, enabled)
    }

    fn pwm_unexport(&mut self, chip: u32, channel: u32) -> Result<(), HalError> {
        self.pwm.get_mut().unexport(chip, channel)
    }
}

impl HalDiagnostics for LinuxHal {
    fn reset_i2c_bus(&self, device: u32) -> Result<(), HalError> {
        self.i2c.borrow_mut().reset_bus(device)
    }

    fn reinit_i2c_sensor(&self, device: u32, address: u32, label: &str) -> Result<(), HalError> {
        self.i2c.borrow_mut().reinit_sensor(device, address, label)
    }

    fn probe_i2c(&self, device: u32, address: u32) -> Result<bool, HalError> {
        self.i2c.borrow().probe(device, address)
    }
}
