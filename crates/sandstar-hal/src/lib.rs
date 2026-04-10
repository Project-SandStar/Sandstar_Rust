use thiserror::Error;

// ── Hardware validation ─────────────────────────────────────

/// Result of probing a single hardware subsystem.
#[derive(Debug, Clone)]
pub struct SubsystemProbe {
    /// Subsystem name (e.g., "i2c", "adc", "gpio", "pwm", "uart").
    pub name: String,
    /// Whether the subsystem is available and accessible.
    pub available: bool,
    /// Human-readable status message.
    pub message: String,
}

/// Aggregated hardware validation results from `validate()`.
#[derive(Debug, Clone, Default)]
pub struct HalValidation {
    pub subsystems: Vec<SubsystemProbe>,
}

impl HalValidation {
    pub fn add(&mut self, name: &str, available: bool, message: &str) {
        self.subsystems.push(SubsystemProbe {
            name: name.to_string(),
            available,
            message: message.to_string(),
        });
    }

    /// True if any subsystem is unavailable.
    pub fn has_failures(&self) -> bool {
        self.subsystems.iter().any(|s| !s.available)
    }

    /// Check if a specific subsystem is available.
    pub fn is_available(&self, name: &str) -> bool {
        self.subsystems
            .iter()
            .any(|s| s.name == name && s.available)
    }
}

// ── Error types ─────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum HalError {
    #[error("device {device} address {address}: {message}")]
    DeviceError {
        device: u32,
        address: u32,
        message: String,
    },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("bus error on device {0}: {1}")]
    BusError(u32, String),

    #[error("timeout on device {device} address {address}")]
    Timeout { device: u32, address: u32 },
}

pub trait HalRead {
    fn read_analog(&self, device: u32, address: u32) -> Result<f64, HalError>;
    fn read_digital(&self, address: u32) -> Result<bool, HalError>;
    fn read_i2c(&self, device: u32, address: u32, label: &str) -> Result<f64, HalError>;
    fn read_pwm(&self, chip: u32, channel: u32) -> Result<f64, HalError>;
    fn read_uart(&self, device: u32, label: &str) -> Result<f64, HalError>;
}

pub trait HalWrite {
    fn write_digital(&self, address: u32, value: bool) -> Result<(), HalError>;
    fn write_pwm(&self, chip: u32, channel: u32, duty: f64) -> Result<(), HalError>;
}

/// Hardware lifecycle management.
///
/// Called by the engine during channel open/close to prepare hardware
/// (export GPIO pins, configure PWM period/enable, open I2C bus, etc.).
/// All methods have default no-op implementations so MockHal works unchanged.
#[allow(unused_variables)]
pub trait HalControl {
    fn init(&mut self) -> Result<(), HalError> {
        Ok(())
    }
    fn shutdown(&mut self) -> Result<(), HalError> {
        Ok(())
    }
    /// Probe hardware subsystems and return structured validation results.
    /// Default implementation returns empty (all-OK) validation.
    fn validate(&self) -> HalValidation {
        HalValidation::default()
    }
    fn gpio_export(&mut self, address: u32, output: bool) -> Result<(), HalError> {
        Ok(())
    }
    fn gpio_unexport(&mut self, address: u32) -> Result<(), HalError> {
        Ok(())
    }
    fn pwm_export(&mut self, chip: u32, channel: u32) -> Result<(), HalError> {
        Ok(())
    }
    fn pwm_configure(
        &mut self,
        chip: u32,
        channel: u32,
        period_ns: u32,
        polarity_normal: bool,
    ) -> Result<(), HalError> {
        Ok(())
    }
    fn pwm_enable(&mut self, chip: u32, channel: u32, enabled: bool) -> Result<(), HalError> {
        Ok(())
    }
    fn pwm_unexport(&mut self, chip: u32, channel: u32) -> Result<(), HalError> {
        Ok(())
    }
}

/// Diagnostics and recovery for HAL implementations.
///
/// Used by the engine to recover from repeated failures (e.g., I2C bus reset).
/// All methods have default no-op implementations.
#[allow(unused_variables)]
pub trait HalDiagnostics {
    fn reset_i2c_bus(&self, device: u32) -> Result<(), HalError> {
        Ok(())
    }
    fn reinit_i2c_sensor(&self, device: u32, address: u32, label: &str) -> Result<(), HalError> {
        Ok(())
    }
    fn probe_i2c(&self, device: u32, address: u32) -> Result<bool, HalError> {
        Ok(true)
    }
}

#[cfg(feature = "simulator")]
pub mod simulator {
    //! Simulator HAL for testing. Uses shared memory (Arc<RwLock<HashMap>>)
    //! so external tools can inject simulated sensor data via REST endpoints.

    use super::*;
    use serde::Serialize;
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};

    /// Key for dispatching simulator read results.
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub enum ReadKey {
        Analog {
            device: u32,
            address: u32,
        },
        Digital {
            address: u32,
        },
        I2c {
            device: u32,
            address: u32,
            label: String,
        },
        Pwm {
            chip: u32,
            channel: u32,
        },
        Uart {
            device: u32,
            label: String,
        },
    }

    /// Key for capturing simulator write outputs.
    #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
    pub enum WriteKey {
        Digital { address: u32 },
        Pwm { chip: u32, channel: u32 },
    }

    /// Shared simulator state: injected reads and captured writes.
    ///
    /// Also available as `SimState` (short alias used by REST endpoints).
    #[derive(Debug, Default)]
    pub struct SimulatorState {
        /// Injected analog/I2C/PWM/UART read values.
        pub reads: HashMap<ReadKey, f64>,
        /// Injected digital read values.
        pub digital_reads: HashMap<u32, bool>,
        /// Captured write outputs (digital + PWM).
        pub writes: HashMap<WriteKey, f64>,
    }

    /// Short alias for `SimulatorState` (used by REST endpoint code).
    pub type SimState = SimulatorState;

    impl SimulatorState {
        /// Create a new empty simulator state (all reads default to 0.0/false).
        pub fn new() -> Self {
            Self::default()
        }
    }

    /// Thread-safe handle to simulator state.
    pub type SharedSimState = Arc<RwLock<SimulatorState>>;

    /// Create a new shared simulator state with default (zero/false) values.
    pub fn new_shared_state() -> SharedSimState {
        Arc::new(RwLock::new(SimulatorState::default()))
    }

    /// Simulator HAL: reads from shared memory, captures writes.
    ///
    /// All reads return 0.0 / false by default. Inject values via the
    /// shared state before polling. All writes are captured in the state
    /// for inspection.
    pub struct SimulatorHal {
        state: SharedSimState,
    }

    impl SimulatorHal {
        pub fn new(state: SharedSimState) -> Self {
            Self { state }
        }

        /// Get a clone of the shared state handle (for passing to REST endpoints).
        pub fn shared_state(&self) -> SharedSimState {
            self.state.clone()
        }
    }

    impl HalRead for SimulatorHal {
        fn read_analog(&self, device: u32, address: u32) -> Result<f64, HalError> {
            let s = self.state.read().expect("SimulatorState RwLock poisoned");
            Ok(*s
                .reads
                .get(&ReadKey::Analog { device, address })
                .unwrap_or(&0.0))
        }

        fn read_digital(&self, address: u32) -> Result<bool, HalError> {
            let s = self.state.read().expect("SimulatorState RwLock poisoned");
            Ok(*s.digital_reads.get(&address).unwrap_or(&false))
        }

        fn read_i2c(&self, device: u32, address: u32, label: &str) -> Result<f64, HalError> {
            let s = self.state.read().expect("SimulatorState RwLock poisoned");
            Ok(*s
                .reads
                .get(&ReadKey::I2c {
                    device,
                    address,
                    label: label.to_string(),
                })
                .unwrap_or(&0.0))
        }

        fn read_pwm(&self, chip: u32, channel: u32) -> Result<f64, HalError> {
            let s = self.state.read().expect("SimulatorState RwLock poisoned");
            Ok(*s.reads.get(&ReadKey::Pwm { chip, channel }).unwrap_or(&0.0))
        }

        fn read_uart(&self, device: u32, label: &str) -> Result<f64, HalError> {
            let s = self.state.read().expect("SimulatorState RwLock poisoned");
            Ok(*s
                .reads
                .get(&ReadKey::Uart {
                    device,
                    label: label.to_string(),
                })
                .unwrap_or(&0.0))
        }
    }

    impl HalWrite for SimulatorHal {
        fn write_digital(&self, address: u32, value: bool) -> Result<(), HalError> {
            let mut s = self.state.write().expect("SimulatorState RwLock poisoned");
            s.writes
                .insert(WriteKey::Digital { address }, if value { 1.0 } else { 0.0 });
            // Mirror write to digital_reads so read_digital reflects the output state
            // (matches real GPIO behavior where reading an output pin returns its driven value)
            s.digital_reads.insert(address, value);
            Ok(())
        }

        fn write_pwm(&self, chip: u32, channel: u32, duty: f64) -> Result<(), HalError> {
            let mut s = self.state.write().expect("SimulatorState RwLock poisoned");
            s.writes.insert(WriteKey::Pwm { chip, channel }, duty);
            Ok(())
        }
    }

    impl HalControl for SimulatorHal {}
    impl HalDiagnostics for SimulatorHal {}

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::thread;

        #[test]
        fn test_read_analog_default_zero() {
            let state = new_shared_state();
            let hal = SimulatorHal::new(state);
            assert_eq!(hal.read_analog(0, 0).unwrap(), 0.0);
            assert_eq!(hal.read_analog(1, 5).unwrap(), 0.0);
        }

        #[test]
        fn test_read_analog_injected() {
            let state = new_shared_state();
            state.write().unwrap().reads.insert(
                ReadKey::Analog {
                    device: 0,
                    address: 3,
                },
                2048.0,
            );
            let hal = SimulatorHal::new(state);
            assert_eq!(hal.read_analog(0, 3).unwrap(), 2048.0);
        }

        #[test]
        fn test_read_digital_default_false() {
            let state = new_shared_state();
            let hal = SimulatorHal::new(state);
            assert!(!hal.read_digital(0).unwrap());
            assert!(!hal.read_digital(99).unwrap());
        }

        #[test]
        fn test_read_digital_injected() {
            let state = new_shared_state();
            state.write().unwrap().digital_reads.insert(5, true);
            let hal = SimulatorHal::new(state);
            assert!(hal.read_digital(5).unwrap());
            assert!(!hal.read_digital(6).unwrap());
        }

        #[test]
        fn test_read_i2c_injected() {
            let state = new_shared_state();
            state.write().unwrap().reads.insert(
                ReadKey::I2c {
                    device: 2,
                    address: 0x40,
                    label: "sdp810".to_string(),
                },
                120.5,
            );
            let hal = SimulatorHal::new(state);
            assert_eq!(hal.read_i2c(2, 0x40, "sdp810").unwrap(), 120.5);
            // Different label returns default
            assert_eq!(hal.read_i2c(2, 0x40, "other").unwrap(), 0.0);
        }

        #[test]
        fn test_write_digital_captured() {
            let state = new_shared_state();
            let hal = SimulatorHal::new(state.clone());
            hal.write_digital(5, true).unwrap();
            hal.write_digital(6, false).unwrap();

            let s = state.read().unwrap();
            assert_eq!(
                *s.writes.get(&WriteKey::Digital { address: 5 }).unwrap(),
                1.0
            );
            assert_eq!(
                *s.writes.get(&WriteKey::Digital { address: 6 }).unwrap(),
                0.0
            );
        }

        #[test]
        fn test_write_pwm_captured() {
            let state = new_shared_state();
            let hal = SimulatorHal::new(state.clone());
            hal.write_pwm(0, 1, 0.75).unwrap();

            let s = state.read().unwrap();
            assert_eq!(
                *s.writes
                    .get(&WriteKey::Pwm {
                        chip: 0,
                        channel: 1
                    })
                    .unwrap(),
                0.75
            );
        }

        #[test]
        fn test_concurrent_access() {
            let state = new_shared_state();
            // Pre-inject a value
            state.write().unwrap().reads.insert(
                ReadKey::Analog {
                    device: 0,
                    address: 0,
                },
                42.0,
            );

            let mut handles = Vec::new();
            for i in 0..10u32 {
                let s = state.clone();
                handles.push(thread::spawn(move || {
                    let hal = SimulatorHal::new(s);
                    // Read (should always succeed)
                    let val = hal.read_analog(0, 0).unwrap();
                    assert_eq!(val, 42.0);
                    // Write (concurrent writes to different keys)
                    hal.write_pwm(0, i, i as f64 * 0.1).unwrap();
                }));
            }
            for h in handles {
                h.join().unwrap();
            }

            // Verify all 10 PWM writes were captured
            let s = state.read().unwrap();
            assert_eq!(s.writes.len(), 10);
        }
    }
}

pub mod mock {
    //! Mock HAL for testing. Uses RefCell queues so tests can pre-load
    //! read results and verify write calls.

    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Key for dispatching mock read results.
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    enum ReadKey {
        Analog {
            device: u32,
            address: u32,
        },
        I2c {
            device: u32,
            address: u32,
            label: String,
        },
        Pwm {
            chip: u32,
            channel: u32,
        },
        Uart {
            device: u32,
            label: String,
        },
    }

    /// Recorded digital write.
    #[derive(Debug, Clone, PartialEq)]
    pub struct DigitalWrite {
        pub address: u32,
        pub value: bool,
    }

    /// Recorded PWM write.
    #[derive(Debug, Clone, PartialEq)]
    pub struct PwmWrite {
        pub chip: u32,
        pub channel: u32,
        pub duty: f64,
    }

    /// Mock HAL implementation for unit tests and demo server.
    ///
    /// Queue read results with `set_*` methods, then call engine functions.
    /// Verify writes with `digital_writes()` / `pwm_writes()`.
    ///
    /// **Sticky mode**: When a queue is drained, the last successful value is
    /// returned on subsequent reads (useful for the demo server's poll loop).
    /// Queued error results are consumed normally and do not become sticky.
    pub struct MockHal {
        reads: RefCell<HashMap<ReadKey, Vec<Result<f64, HalError>>>>,
        /// Sticky last-good values per read key (returned when queue is empty).
        sticky: RefCell<HashMap<ReadKey, f64>>,
        digital_reads: RefCell<HashMap<u32, Vec<Result<bool, HalError>>>>,
        /// Sticky last-good digital values per address.
        digital_sticky: RefCell<HashMap<u32, bool>>,
        digital_writes: RefCell<Vec<DigitalWrite>>,
        pwm_writes: RefCell<Vec<PwmWrite>>,
        /// Queued results for `reinit_i2c_sensor` calls. If empty, returns Ok(()).
        reinit_results: RefCell<Vec<Result<(), HalError>>>,
    }

    impl MockHal {
        pub fn new() -> Self {
            Self {
                reads: RefCell::new(HashMap::new()),
                sticky: RefCell::new(HashMap::new()),
                digital_reads: RefCell::new(HashMap::new()),
                digital_sticky: RefCell::new(HashMap::new()),
                digital_writes: RefCell::new(Vec::new()),
                pwm_writes: RefCell::new(Vec::new()),
                reinit_results: RefCell::new(Vec::new()),
            }
        }

        /// Queue an analog read result for (device, address).
        pub fn set_analog(&self, device: u32, address: u32, result: Result<f64, HalError>) {
            self.reads
                .borrow_mut()
                .entry(ReadKey::Analog { device, address })
                .or_default()
                .push(result);
        }

        /// Queue a digital read result for address.
        pub fn set_digital(&self, address: u32, result: Result<bool, HalError>) {
            self.digital_reads
                .borrow_mut()
                .entry(address)
                .or_default()
                .push(result);
        }

        /// Queue an I2C read result for (device, address, label).
        pub fn set_i2c(
            &self,
            device: u32,
            address: u32,
            label: &str,
            result: Result<f64, HalError>,
        ) {
            self.reads
                .borrow_mut()
                .entry(ReadKey::I2c {
                    device,
                    address,
                    label: label.to_string(),
                })
                .or_default()
                .push(result);
        }

        /// Queue a PWM read result for (chip, channel).
        pub fn set_pwm(&self, chip: u32, channel: u32, result: Result<f64, HalError>) {
            self.reads
                .borrow_mut()
                .entry(ReadKey::Pwm { chip, channel })
                .or_default()
                .push(result);
        }

        /// Queue a UART read result for (device, label).
        pub fn set_uart(&self, device: u32, label: &str, result: Result<f64, HalError>) {
            self.reads
                .borrow_mut()
                .entry(ReadKey::Uart {
                    device,
                    label: label.to_string(),
                })
                .or_default()
                .push(result);
        }

        /// Get all recorded digital writes.
        pub fn digital_writes(&self) -> Vec<DigitalWrite> {
            self.digital_writes.borrow().clone()
        }

        /// Get all recorded PWM writes.
        pub fn pwm_writes(&self) -> Vec<PwmWrite> {
            self.pwm_writes.borrow().clone()
        }

        /// Queue a reinit_i2c_sensor result (consumed in FIFO order).
        /// If the queue is empty when reinit is called, Ok(()) is returned.
        pub fn queue_reinit_result(&self, result: Result<(), HalError>) {
            self.reinit_results.borrow_mut().push(result);
        }

        /// Pop a read result from the queue, fall back to sticky value.
        ///
        /// If the queue has values, consume the first one. On success, update
        /// the sticky cache so future reads (after queue drain) return the
        /// last good value instead of erroring.
        fn pop_read(&self, key: &ReadKey) -> Result<f64, HalError> {
            let mut reads = self.reads.borrow_mut();
            if let Some(queue) = reads.get_mut(key) {
                if !queue.is_empty() {
                    let result = queue.remove(0);
                    if let Ok(val) = &result {
                        self.sticky.borrow_mut().insert(key.clone(), *val);
                    }
                    return result;
                }
            }
            // Queue empty — return sticky value if we have one
            if let Some(&val) = self.sticky.borrow().get(key) {
                return Ok(val);
            }
            Err(HalError::DeviceError {
                device: 0,
                address: 0,
                message: "mock: no queued read result".to_string(),
            })
        }
    }

    impl Default for MockHal {
        fn default() -> Self {
            Self::new()
        }
    }

    impl HalRead for MockHal {
        fn read_analog(&self, device: u32, address: u32) -> Result<f64, HalError> {
            self.pop_read(&ReadKey::Analog { device, address })
        }

        fn read_digital(&self, address: u32) -> Result<bool, HalError> {
            let mut reads = self.digital_reads.borrow_mut();
            if let Some(queue) = reads.get_mut(&address) {
                if !queue.is_empty() {
                    let result = queue.remove(0);
                    if let Ok(val) = &result {
                        self.digital_sticky.borrow_mut().insert(address, *val);
                    }
                    return result;
                }
            }
            if let Some(&val) = self.digital_sticky.borrow().get(&address) {
                return Ok(val);
            }
            Err(HalError::DeviceError {
                device: 0,
                address,
                message: "mock: no queued digital read result".to_string(),
            })
        }

        fn read_i2c(&self, device: u32, address: u32, label: &str) -> Result<f64, HalError> {
            self.pop_read(&ReadKey::I2c {
                device,
                address,
                label: label.to_string(),
            })
        }

        fn read_pwm(&self, chip: u32, channel: u32) -> Result<f64, HalError> {
            self.pop_read(&ReadKey::Pwm { chip, channel })
        }

        fn read_uart(&self, device: u32, label: &str) -> Result<f64, HalError> {
            self.pop_read(&ReadKey::Uart {
                device,
                label: label.to_string(),
            })
        }
    }

    impl HalWrite for MockHal {
        fn write_digital(&self, address: u32, value: bool) -> Result<(), HalError> {
            self.digital_writes
                .borrow_mut()
                .push(DigitalWrite { address, value });
            Ok(())
        }

        fn write_pwm(&self, chip: u32, channel: u32, duty: f64) -> Result<(), HalError> {
            self.pwm_writes.borrow_mut().push(PwmWrite {
                chip,
                channel,
                duty,
            });
            Ok(())
        }
    }

    impl HalControl for MockHal {}
    impl HalDiagnostics for MockHal {
        fn reinit_i2c_sensor(
            &self,
            _device: u32,
            _address: u32,
            _label: &str,
        ) -> Result<(), HalError> {
            let mut queue = self.reinit_results.borrow_mut();
            if queue.is_empty() {
                Ok(())
            } else {
                queue.remove(0)
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_mock_analog_read() {
            let hal = MockHal::new();
            hal.set_analog(0, 0, Ok(2048.0));
            assert_eq!(hal.read_analog(0, 0).unwrap(), 2048.0);
        }

        #[test]
        fn test_mock_digital_read() {
            let hal = MockHal::new();
            hal.set_digital(5, Ok(true));
            assert!(hal.read_digital(5).unwrap());
        }

        #[test]
        fn test_mock_empty_queue_errors() {
            let hal = MockHal::new();
            assert!(hal.read_analog(0, 0).is_err());
            assert!(hal.read_digital(0).is_err());
        }

        #[test]
        fn test_mock_multiple_reads() {
            let hal = MockHal::new();
            hal.set_analog(0, 0, Ok(100.0));
            hal.set_analog(0, 0, Ok(200.0));
            assert_eq!(hal.read_analog(0, 0).unwrap(), 100.0);
            assert_eq!(hal.read_analog(0, 0).unwrap(), 200.0);
            // Queue empty — returns sticky (last good value)
            assert_eq!(hal.read_analog(0, 0).unwrap(), 200.0);
        }

        #[test]
        fn test_mock_digital_writes() {
            let hal = MockHal::new();
            hal.write_digital(5, true).unwrap();
            hal.write_digital(6, false).unwrap();

            let writes = hal.digital_writes();
            assert_eq!(writes.len(), 2);
            assert_eq!(
                writes[0],
                DigitalWrite {
                    address: 5,
                    value: true
                }
            );
            assert_eq!(
                writes[1],
                DigitalWrite {
                    address: 6,
                    value: false
                }
            );
        }

        #[test]
        fn test_mock_pwm_writes() {
            let hal = MockHal::new();
            hal.write_pwm(0, 1, 0.75).unwrap();

            let writes = hal.pwm_writes();
            assert_eq!(writes.len(), 1);
            assert_eq!(
                writes[0],
                PwmWrite {
                    chip: 0,
                    channel: 1,
                    duty: 0.75
                }
            );
        }

        #[test]
        fn test_mock_i2c_read() {
            let hal = MockHal::new();
            hal.set_i2c(2, 0x40, "sdp810", Ok(120.0));
            assert_eq!(hal.read_i2c(2, 0x40, "sdp810").unwrap(), 120.0);
        }

        #[test]
        fn test_mock_uart_read() {
            let hal = MockHal::new();
            hal.set_uart(1, "co2", Ok(400.0));
            assert_eq!(hal.read_uart(1, "co2").unwrap(), 400.0);
        }

        #[test]
        fn test_mock_error_result() {
            let hal = MockHal::new();
            hal.set_analog(
                0,
                0,
                Err(HalError::Timeout {
                    device: 0,
                    address: 0,
                }),
            );
            assert!(hal.read_analog(0, 0).is_err());
        }
    }
}
