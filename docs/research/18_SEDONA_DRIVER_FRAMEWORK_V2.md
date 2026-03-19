# Driver Framework v2: Pure Rust Haxall-Inspired Architecture

## Overview

This document proposes **Driver Framework v2**, a pure Rust driver architecture that incorporates the best methodologies from Haxall's connector framework. This replaces all C/C++ components with native Rust implementations while **preserving full compatibility with existing Sedona applications**.

### Design Goals

1. **Pure Rust** — No C/C++ dependencies, no FFI, complete rewrite
2. **Sedona Compatibility** — Existing .sax applications run unchanged on Rust VM
3. **Callback-Based Lifecycle** — Structured lifecycle with well-defined callbacks
4. **Learn/Discovery** — Automatic point discovery from remote systems and local hardware
5. **Polling Buckets** — Efficient batched polling with automatic staggering
6. **Watch/Subscription Model** — Real-time subscriptions for change-of-value
7. **Status Inheritance** — Driver status cascades to child points
8. **Typed Error Handling** — Distinguish configuration faults from communication errors
9. **Actor-Based Concurrency** — Tokio tasks with message passing

---

## Architecture Comparison

| Aspect | C/C++ (Current) | Haxall (Fantom) | Pure Rust (Proposed) |
|--------|-----------------|-----------------|----------------------|
| **Language** | C engine + C++ REST | Fantom | 100% Rust |
| **Lifecycle** | `start()`, `execute()` | `onOpen()`, `onClose()`, `onPing()` | `Driver` trait callbacks |
| **Point Discovery** | Manual Zinc config | `onLearn()` tree walk | `Driver::on_learn()` |
| **Polling** | Engine auto-poll all | Buckets + manual mode | `PollScheduler` |
| **Watch Model** | Limited | `onWatch()`/`onUnwatch()` | `WatchManager` |
| **Status** | Per-point bitmask | Inherited from connector | `DriverStatus` cascading |
| **Error Types** | Generic fault bit | `FaultErr`, `RemoteStatusErr` | `DriverError` enum |
| **Threading** | IPC message queues | Fantom actor pool | Tokio tasks + channels |
| **Hardware I/O** | sysfs via C | N/A | Rust HAL crates |
| **Control Logic** | Sedona VM (C) | Axon scripts | Sedona VM (Rust port) |
| **App Compatibility** | .sax/.sab native | N/A | .sax/.sab compatible |

---

## Architecture

### Layer Diagram

```
┌─────────────────────────────────────────────────────────────────────────┐
│                     Haystack REST API (Axum + Trio)                     │
│                     ROX WebSocket (Trio encoding)                       │
└───────────────────────────────────┬─────────────────────────────────────┘
                                    │
┌───────────────────────────────────▼─────────────────────────────────────┐
│                      Driver Framework v2 (Pure Rust)                    │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐    │
│  │ LocalIoDriver│ │ ModbusDriver│  │ BacnetDriver│  │ MqttDriver  │    │
│  │ (gpio,i2c,  │  │ (TCP/RTU)   │  │ (IP/MSTP)   │  │ (pub/sub)   │    │
│  │  adc,pwm)   │  │             │  │             │  │             │    │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘    │
│         │                │                │                │            │
│  ┌──────▼─────────────────▼────────────────▼────────────────▼──────┐   │
│  │                    DriverManager (Tokio Actor)                   │   │
│  │  • Lifecycle orchestration                                       │   │
│  │  • Polling bucket scheduler                                      │   │
│  │  • Watch subscription manager                                    │   │
│  │  • Status inheritance                                            │   │
│  │  • Point database (replaces Zinc grid)                           │   │
│  └──────────────────────────────────────────────────────────────────┘   │
│                                    │                                    │
│  ┌─────────────────────────────────▼────────────────────────────────┐   │
│  │              Sedona VM (Rust Port - see doc 13)                   │   │
│  │  • Bytecode interpreter rewritten in Rust                        │   │
│  │  • Runs existing .sax/.sab applications unchanged                │   │
│  │  • Native methods call into Driver Framework                     │   │
│  │  • PIDs, schedules, sequences via Sedona components              │   │
│  └──────────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────┘
                                    │
┌───────────────────────────────────▼─────────────────────────────────────┐
│                         Hardware (BeagleBone)                           │
│  • gpio-cdev (GPIO chardev)    • i2cdev (I2C bus)                      │
│  • industrial-io (ADC via IIO) • sysfs-pwm (PWM outputs)               │
│  • serialport (UART/RS-485)                                            │
└─────────────────────────────────────────────────────────────────────────┘
```

### Component Model

```
DriverManager (singleton tokio actor)
    │
    ├── Driver instances (one per protocol/connection)
    │   ├── LocalIoDriver { board: "beaglebone", pins: [...] }
    │   ├── ModbusDriver { host: "192.168.1.100:502", unit_id: 1 }
    │   ├── BacnetDriver { network: 1, device: 12345 }
    │   └── MqttDriver { broker: "mqtt://broker:1883" }
    │
    ├── DriverPoint instances (managed by parent driver)
    │   ├── LocalIoPoint { pin: "AIN0", kind: Number, unit: "°F" }
    │   ├── ModbusPoint { address: "40001", kind: Number }
    │   └── MqttPoint { topic: "sensors/temp", kind: Number }
    │
    └── Sedona VM (Rust) ─── runs existing .sax applications
        ├── Loads .sab (compiled from .sax)
        ├── Interprets .scode (kit bytecode)
        └── Native methods → DriverManager.read_point() / write_point()
```

---

## Driver Trait

### Core Trait Definition

```rust
use async_trait::async_trait;
use libhaystack::val::Value;

/// Driver lifecycle and I/O callbacks (Haxall-inspired)
#[async_trait]
pub trait Driver: Send + Sync {
    /// Unique driver type identifier (e.g., "localio", "modbus", "bacnet", "mqtt")
    fn driver_type(&self) -> &'static str;

    /// Driver instance ID (unique within DriverManager)
    fn id(&self) -> DriverId;

    /// Current driver status
    fn status(&self) -> DriverStatus;

    // ═══════════════════════════════════════════════════════════════════
    // LIFECYCLE CALLBACKS
    // ═══════════════════════════════════════════════════════════════════

    /// Called when driver transitions from pending to active.
    /// Initialize hardware, establish connections.
    /// Return Ok(metadata) on success, Err on failure.
    async fn on_open(&mut self) -> Result<DriverMeta, DriverError>;

    /// Called when driver is being shut down.
    /// Release hardware resources, close connections.
    async fn on_close(&mut self);

    /// Called periodically for health check.
    /// Return Ok(metadata) if hardware/connection is healthy.
    async fn on_ping(&mut self) -> Result<DriverMeta, DriverError>;

    // ═══════════════════════════════════════════════════════════════════
    // DISCOVERY / LEARN
    // ═══════════════════════════════════════════════════════════════════

    /// Walk hardware/remote system's data model for point discovery.
    /// - `path == None` → return root-level items
    /// - `path == Some("analog/inputs")` → return children of that item
    /// Returns grid of discoverable items with tags for mapping to points.
    async fn on_learn(&mut self, path: Option<&str>) -> Result<LearnGrid, DriverError> {
        Err(DriverError::NotSupported("learn"))
    }

    // ═══════════════════════════════════════════════════════════════════
    // CURRENT VALUE SYNC
    // ═══════════════════════════════════════════════════════════════════

    /// Synchronize current values for a batch of points.
    /// Called by polling bucket scheduler or manual sync request.
    /// Implementation should call `ctx.update_cur_ok()` or `ctx.update_cur_err()`.
    async fn on_sync_cur(&mut self, points: &[DriverPointRef], ctx: &mut SyncContext);

    // ═══════════════════════════════════════════════════════════════════
    // WATCH / SUBSCRIPTION (for COV-capable protocols)
    // ═══════════════════════════════════════════════════════════════════

    /// Called when points transition from unwatched to watched.
    /// For COV-capable protocols, establish subscriptions.
    async fn on_watch(&mut self, points: &[DriverPointRef]) -> Result<(), DriverError> {
        Ok(()) // Default: polling-only, no COV
    }

    /// Called when points are removed from watch set.
    async fn on_unwatch(&mut self, points: &[DriverPointRef]) -> Result<(), DriverError> {
        Ok(())
    }

    // ═══════════════════════════════════════════════════════════════════
    // WRITE OPERATIONS
    // ═══════════════════════════════════════════════════════════════════

    /// Write values to a batch of points.
    /// Called when effective write value changes (priority array resolution).
    async fn on_write(&mut self, writes: &[WriteRequest], ctx: &mut WriteContext);

    // ═══════════════════════════════════════════════════════════════════
    // POLLING MODE
    // ═══════════════════════════════════════════════════════════════════

    /// Override for manual polling mode (default: buckets)
    fn poll_mode(&self) -> PollMode {
        PollMode::Buckets
    }

    /// Called at configured frequency in Manual mode
    async fn on_poll_manual(&mut self) {}

    // ═══════════════════════════════════════════════════════════════════
    // CUSTOM MESSAGES
    // ═══════════════════════════════════════════════════════════════════

    /// Handle custom driver-specific messages.
    async fn on_receive(&mut self, msg: DriverMessage) -> Result<DriverMessage, DriverError> {
        Err(DriverError::NotSupported("custom message"))
    }
}
```

### Driver Metadata

```rust
/// Metadata returned from on_open and on_ping
#[derive(Debug, Clone, Default)]
pub struct DriverMeta {
    /// Product name (e.g., "BeagleBone Black", "Modbus TCP")
    pub product: String,
    /// Firmware/protocol version
    pub version: String,
    /// Serial number or device ID
    pub serial: Option<String>,
    /// Additional protocol-specific info
    pub info: HashMap<String, Value>,
}
```

### Status Model

```rust
/// Driver status (cascades to child points)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverStatus {
    /// Initial state before on_open
    Pending,
    /// Successfully initialized and communicating
    Ok,
    /// No updates received within stale_time
    Stale,
    /// Hardware/connection unavailable
    Down,
    /// Local configuration error (won't recover without fix)
    Fault,
    /// Manually disabled
    Disabled,
    /// Currently synchronizing/initializing
    Syncing,
}

/// Point status (may be inherited from driver or point-specific)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointStatus {
    Ok,
    Stale,
    Down,
    Fault,
    Disabled,
    /// Remote system reports point disabled
    RemoteDisabled,
    /// Remote system reports point down
    RemoteDown,
    /// Remote system reports point fault
    RemoteFault,
}

impl PointStatus {
    /// Inherit status from parent driver
    pub fn inherit_from_driver(driver: DriverStatus) -> Self {
        match driver {
            DriverStatus::Ok | DriverStatus::Syncing => PointStatus::Ok,
            DriverStatus::Stale => PointStatus::Stale,
            DriverStatus::Down => PointStatus::Down,
            DriverStatus::Fault => PointStatus::Fault,
            DriverStatus::Disabled | DriverStatus::Pending => PointStatus::Disabled,
        }
    }
}
```

### Error Handling

```rust
/// Driver error types (Haxall-inspired categorization)
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    /// Configuration error - won't recover without manual fix
    #[error("configuration fault: {0}")]
    Fault(String),

    /// Remote/hardware reports error status
    #[error("remote status: {status:?} - {message}")]
    RemoteStatus {
        status: PointStatus,
        message: String,
    },

    /// Temporary communication/hardware failure - will retry
    #[error("communication error: {0}")]
    Communication(#[from] std::io::Error),

    /// Timeout waiting for response
    #[error("timeout: {0}")]
    Timeout(String),

    /// Operation not supported by this driver
    #[error("not supported: {0}")]
    NotSupported(&'static str),

    /// Internal driver error
    #[error("internal error: {0}")]
    Internal(String),

    /// Hardware not found or inaccessible
    #[error("hardware not found: {0}")]
    HardwareNotFound(String),
}
```

---

## LocalIoDriver: Pure Rust Hardware I/O

### Design

The `LocalIoDriver` replaces the C engine with pure Rust hardware access using Linux HAL crates.

```rust
use gpio_cdev::{Chip, LineHandle, LineRequestFlags};
use i2cdev::core::I2CDevice;
use i2cdev::linux::LinuxI2CDevice;
use industrial_io as iio;

/// Pure Rust local hardware I/O driver
pub struct LocalIoDriver {
    id: DriverId,
    status: DriverStatus,
    config: LocalIoConfig,

    // Hardware handles
    gpio_chip: Option<Chip>,
    gpio_lines: HashMap<u32, GpioLine>,
    i2c_devices: HashMap<u8, LinuxI2CDevice>,
    iio_context: Option<iio::Context>,
    iio_channels: HashMap<String, IioChannel>,
    pwm_chips: HashMap<String, PwmChip>,
    serial_ports: HashMap<String, Box<dyn serialport::SerialPort>>,

    // Value conversion tables (loaded from config)
    conversion_tables: HashMap<String, ConversionTable>,
}

#[derive(Debug, Clone)]
pub struct LocalIoConfig {
    /// GPIO chip path (e.g., "/dev/gpiochip0")
    pub gpio_chip: String,
    /// I2C bus paths (e.g., {2: "/dev/i2c-2"})
    pub i2c_buses: HashMap<u8, String>,
    /// IIO device path (e.g., "/sys/bus/iio/devices/iio:device0")
    pub iio_device: Option<String>,
    /// PWM chip paths
    pub pwm_chips: Vec<String>,
    /// Serial ports for RS-485
    pub serial_ports: HashMap<String, SerialConfig>,
    /// Conversion table directory
    pub table_dir: PathBuf,
}

struct GpioLine {
    handle: LineHandle,
    direction: GpioDirection,
    active_low: bool,
}

struct IioChannel {
    channel: iio::Channel,
    scale: f64,
    offset: f64,
}

struct PwmChip {
    chip_path: PathBuf,
    channels: HashMap<u32, PwmChannel>,
}
```

### Implementation

```rust
#[async_trait]
impl Driver for LocalIoDriver {
    fn driver_type(&self) -> &'static str {
        "localio"
    }

    fn id(&self) -> DriverId {
        self.id
    }

    fn status(&self) -> DriverStatus {
        self.status
    }

    async fn on_open(&mut self) -> Result<DriverMeta, DriverError> {
        // Initialize GPIO chip
        self.gpio_chip = Some(
            Chip::new(&self.config.gpio_chip)
                .map_err(|e| DriverError::HardwareNotFound(format!("GPIO: {}", e)))?
        );

        // Initialize I2C buses
        for (bus, path) in &self.config.i2c_buses {
            let device = LinuxI2CDevice::new(path, 0)
                .map_err(|e| DriverError::HardwareNotFound(format!("I2C bus {}: {}", bus, e)))?;
            self.i2c_devices.insert(*bus, device);
        }

        // Initialize IIO context for ADC
        if let Some(ref iio_path) = self.config.iio_device {
            self.iio_context = Some(
                iio::Context::new()
                    .map_err(|e| DriverError::HardwareNotFound(format!("IIO: {}", e)))?
            );
            self.discover_iio_channels()?;
        }

        // Load conversion tables
        self.load_conversion_tables()?;

        self.status = DriverStatus::Ok;

        Ok(DriverMeta {
            product: "BeagleBone Local I/O".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        })
    }

    async fn on_close(&mut self) {
        // Release GPIO lines
        self.gpio_lines.clear();

        // Close I2C devices
        self.i2c_devices.clear();

        // Close serial ports
        self.serial_ports.clear();

        self.status = DriverStatus::Disabled;
    }

    async fn on_ping(&mut self) -> Result<DriverMeta, DriverError> {
        // Verify GPIO chip is accessible
        if self.gpio_chip.is_none() {
            return Err(DriverError::HardwareNotFound("GPIO chip closed".into()));
        }

        // Test read from IIO if available
        if let Some(ref ctx) = self.iio_context {
            // Quick health check
        }

        Ok(DriverMeta::default())
    }

    async fn on_learn(&mut self, path: Option<&str>) -> Result<LearnGrid, DriverError> {
        let mut grid = LearnGrid::new();

        match path {
            None => {
                // Root: list I/O types
                grid.add(LearnItem {
                    dis: "Analog Inputs (ADC)".into(),
                    learn: Some("adc".into()),
                    point: false,
                    ..Default::default()
                });
                grid.add(LearnItem {
                    dis: "Digital Inputs (GPIO)".into(),
                    learn: Some("gpio/input".into()),
                    point: false,
                    ..Default::default()
                });
                grid.add(LearnItem {
                    dis: "Digital Outputs (GPIO)".into(),
                    learn: Some("gpio/output".into()),
                    point: false,
                    ..Default::default()
                });
                grid.add(LearnItem {
                    dis: "Analog Outputs (PWM)".into(),
                    learn: Some("pwm".into()),
                    point: false,
                    ..Default::default()
                });
                grid.add(LearnItem {
                    dis: "I2C Sensors".into(),
                    learn: Some("i2c".into()),
                    point: false,
                    ..Default::default()
                });
            }
            Some("adc") => {
                // Discover IIO ADC channels
                if let Some(ref ctx) = self.iio_context {
                    for (name, ch) in &self.iio_channels {
                        grid.add(LearnItem {
                            dis: format!("ADC {}", name),
                            learn: None,
                            point: true,
                            address: Some(format!("adc:{}", name)),
                            kind: Some(PointKind::Number),
                            writable: false,
                            ..Default::default()
                        });
                    }
                }
            }
            Some("gpio/input") => {
                // List available GPIO lines for input
                if let Some(ref chip) = self.gpio_chip {
                    for line in 0..chip.num_lines() {
                        let info = chip.get_line(line).ok().and_then(|l| l.info().ok());
                        let name = info.as_ref()
                            .map(|i| i.name().unwrap_or(&format!("GPIO{}", line)).to_string())
                            .unwrap_or_else(|| format!("GPIO{}", line));

                        grid.add(LearnItem {
                            dis: name,
                            learn: None,
                            point: true,
                            address: Some(format!("gpio:{}:in", line)),
                            kind: Some(PointKind::Bool),
                            writable: false,
                            ..Default::default()
                        });
                    }
                }
            }
            Some("gpio/output") => {
                // List available GPIO lines for output
                if let Some(ref chip) = self.gpio_chip {
                    for line in 0..chip.num_lines() {
                        grid.add(LearnItem {
                            dis: format!("GPIO{} Output", line),
                            learn: None,
                            point: true,
                            address: Some(format!("gpio:{}:out", line)),
                            kind: Some(PointKind::Bool),
                            writable: true,
                            ..Default::default()
                        });
                    }
                }
            }
            Some("i2c") => {
                // Scan I2C bus for devices
                for (bus, device) in &mut self.i2c_devices {
                    for addr in 0x03..=0x77 {
                        if self.probe_i2c_device(*bus, addr).await {
                            let sensor_type = self.identify_i2c_sensor(addr);
                            grid.add(LearnItem {
                                dis: format!("I2C {}: {} (0x{:02X})", bus, sensor_type, addr),
                                learn: Some(format!("i2c/{}/{:02x}", bus, addr)),
                                point: false,
                                ..Default::default()
                            });
                        }
                    }
                }
            }
            Some(path) if path.starts_with("i2c/") => {
                // List points for specific I2C sensor
                let parts: Vec<&str> = path.split('/').collect();
                if parts.len() >= 3 {
                    let bus: u8 = parts[1].parse().unwrap_or(0);
                    let addr = u8::from_str_radix(parts[2], 16).unwrap_or(0);

                    // Return sensor-specific points based on device type
                    let points = self.get_i2c_sensor_points(bus, addr);
                    for p in points {
                        grid.add(p);
                    }
                }
            }
            _ => {}
        }

        Ok(grid)
    }

    async fn on_sync_cur(&mut self, points: &[DriverPointRef], ctx: &mut SyncContext) {
        for point in points {
            let result = self.read_point(point.address()).await;
            match result {
                Ok(value) => ctx.update_cur_ok(point.id(), value),
                Err(e) => ctx.update_cur_err(point.id(), e),
            }
        }
    }

    async fn on_write(&mut self, writes: &[WriteRequest], ctx: &mut WriteContext) {
        for write in writes {
            if let Some(ref val) = write.val {
                let result = self.write_point(write.point.address(), val).await;
                match result {
                    Ok(()) => ctx.update_write_ok(write.point.id()),
                    Err(e) => ctx.update_write_err(write.point.id(), e),
                }
            }
        }
    }
}
```

### Hardware Read/Write Methods

```rust
impl LocalIoDriver {
    /// Read a point by address (e.g., "adc:AIN0", "gpio:45:in", "i2c:2:76:temp")
    async fn read_point(&mut self, address: &str) -> Result<Value, DriverError> {
        let parts: Vec<&str> = address.split(':').collect();

        match parts.get(0).copied() {
            Some("adc") => {
                let channel_name = parts.get(1).ok_or_else(||
                    DriverError::Fault("missing ADC channel".into()))?;
                self.read_adc(channel_name).await
            }
            Some("gpio") => {
                let line: u32 = parts.get(1)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| DriverError::Fault("invalid GPIO line".into()))?;
                self.read_gpio(line).await
            }
            Some("i2c") => {
                let bus: u8 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(2);
                let addr: u8 = parts.get(2).and_then(|s| u8::from_str_radix(s, 16).ok()).unwrap_or(0);
                let register = parts.get(3).copied().unwrap_or("value");
                self.read_i2c(bus, addr, register).await
            }
            Some("pwm") => {
                let chip = parts.get(1).unwrap_or(&"0");
                let channel: u32 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
                self.read_pwm_duty(chip, channel).await
            }
            _ => Err(DriverError::Fault(format!("unknown address type: {}", address))),
        }
    }

    /// Write a point by address
    async fn write_point(&mut self, address: &str, value: &Value) -> Result<(), DriverError> {
        let parts: Vec<&str> = address.split(':').collect();

        match parts.get(0).copied() {
            Some("gpio") => {
                let line: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                let bool_val = match value {
                    Value::Bool(b) => *b,
                    Value::Number(n) => n.value != 0.0,
                    _ => return Err(DriverError::Fault("expected bool or number".into())),
                };
                self.write_gpio(line, bool_val).await
            }
            Some("pwm") => {
                let chip = parts.get(1).unwrap_or(&"0");
                let channel: u32 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
                let duty = match value {
                    Value::Number(n) => n.value,
                    _ => return Err(DriverError::Fault("expected number".into())),
                };
                self.write_pwm_duty(chip, channel, duty).await
            }
            Some("i2c") => {
                let bus: u8 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(2);
                let addr: u8 = parts.get(2).and_then(|s| u8::from_str_radix(s, 16).ok()).unwrap_or(0);
                let register = parts.get(3).copied().unwrap_or("value");
                self.write_i2c(bus, addr, register, value).await
            }
            _ => Err(DriverError::Fault(format!("cannot write to: {}", address))),
        }
    }

    /// Read ADC channel via IIO
    async fn read_adc(&self, channel_name: &str) -> Result<Value, DriverError> {
        let ch = self.iio_channels.get(channel_name)
            .ok_or_else(|| DriverError::HardwareNotFound(format!("ADC channel {}", channel_name)))?;

        // Read raw value
        let raw: i64 = ch.channel.attr_read_int("raw")
            .map_err(|e| DriverError::Communication(std::io::Error::new(
                std::io::ErrorKind::Other, e.to_string()
            )))?;

        // Apply scale and offset
        let value = (raw as f64) * ch.scale + ch.offset;

        Ok(Value::make_number(value))
    }

    /// Read GPIO input
    async fn read_gpio(&self, line: u32) -> Result<Value, DriverError> {
        let gpio = self.gpio_lines.get(&line)
            .ok_or_else(|| DriverError::HardwareNotFound(format!("GPIO line {}", line)))?;

        let value = gpio.handle.get_value()
            .map_err(|e| DriverError::Communication(std::io::Error::new(
                std::io::ErrorKind::Other, e.to_string()
            )))?;

        let bool_val = if gpio.active_low { value == 0 } else { value != 0 };
        Ok(Value::Bool(bool_val))
    }

    /// Write GPIO output
    async fn write_gpio(&mut self, line: u32, value: bool) -> Result<(), DriverError> {
        // Ensure line is configured for output
        if !self.gpio_lines.contains_key(&line) {
            self.configure_gpio_output(line)?;
        }

        let gpio = self.gpio_lines.get(&line)
            .ok_or_else(|| DriverError::HardwareNotFound(format!("GPIO line {}", line)))?;

        let hw_value = if gpio.active_low { !value } else { value };
        gpio.handle.set_value(hw_value as u8)
            .map_err(|e| DriverError::Communication(std::io::Error::new(
                std::io::ErrorKind::Other, e.to_string()
            )))?;

        Ok(())
    }

    /// Read I2C sensor register
    async fn read_i2c(&mut self, bus: u8, addr: u8, register: &str) -> Result<Value, DriverError> {
        let device = self.i2c_devices.get_mut(&bus)
            .ok_or_else(|| DriverError::HardwareNotFound(format!("I2C bus {}", bus)))?;

        // Set slave address
        device.set_slave_address(addr as u16)
            .map_err(|e| DriverError::Communication(std::io::Error::new(
                std::io::ErrorKind::Other, e.to_string()
            )))?;

        // Sensor-specific read logic
        let value = match self.identify_i2c_sensor(addr) {
            "SDP810" => self.read_sdp810(device, register).await?,
            "SHT31" => self.read_sht31(device, register).await?,
            "BME280" => self.read_bme280(device, register).await?,
            _ => {
                // Generic register read
                let mut buf = [0u8; 2];
                device.read(&mut buf)
                    .map_err(|e| DriverError::Communication(std::io::Error::new(
                        std::io::ErrorKind::Other, e.to_string()
                    )))?;
                i16::from_be_bytes(buf) as f64
            }
        };

        Ok(Value::make_number(value))
    }

    /// Read SDP810 differential pressure sensor
    async fn read_sdp810(&self, device: &mut LinuxI2CDevice, register: &str) -> Result<f64, DriverError> {
        // Trigger measurement
        device.write(&[0x36, 0x15])
            .map_err(|e| DriverError::Communication(e.into()))?;

        // Wait for measurement
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Read result
        let mut buf = [0u8; 9];
        device.read(&mut buf)
            .map_err(|e| DriverError::Communication(e.into()))?;

        let dp_raw = i16::from_be_bytes([buf[0], buf[1]]);
        let temp_raw = i16::from_be_bytes([buf[3], buf[4]]);
        let scale = i16::from_be_bytes([buf[6], buf[7]]);

        match register {
            "pressure" | "value" => Ok(dp_raw as f64 / scale as f64),
            "temp" => Ok(temp_raw as f64 / 200.0),
            _ => Ok(dp_raw as f64 / scale as f64),
        }
    }
}
```

### Value Conversion System

```rust
/// Conversion table for non-linear sensors (thermistors, RTDs, etc.)
#[derive(Debug, Clone)]
pub struct ConversionTable {
    /// Table name (e.g., "thermistor10k")
    pub name: String,
    /// Raw ADC values (index 0 = min raw, index N = max raw)
    pub values: Vec<f64>,
    /// Minimum raw value
    pub raw_min: u16,
    /// Maximum raw value
    pub raw_max: u16,
    /// Engineering unit
    pub unit: String,
}

impl ConversionTable {
    /// Load from file (one value per line)
    pub fn from_file(path: &Path) -> Result<Self, std::io::Error> {
        let content = std::fs::read_to_string(path)?;
        let values: Vec<f64> = content
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();

        let name = path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        Ok(Self {
            name,
            raw_min: 0,
            raw_max: values.len().saturating_sub(1) as u16,
            values,
            unit: "°F".into(), // Default, can be overridden
        })
    }

    /// Interpolate value from raw ADC reading
    pub fn interpolate(&self, raw: u16) -> f64 {
        if self.values.is_empty() {
            return f64::NAN;
        }

        let clamped = raw.clamp(self.raw_min, self.raw_max);
        let idx = (clamped - self.raw_min) as usize;

        if idx >= self.values.len() - 1 {
            return self.values[self.values.len() - 1];
        }

        // Linear interpolation between table entries
        let frac = (clamped - self.raw_min) as f64 - idx as f64;
        let v0 = self.values[idx];
        let v1 = self.values[idx + 1];

        v0 + frac * (v1 - v0)
    }
}

/// Point with conversion configuration
#[derive(Debug, Clone)]
pub struct LocalIoPointConfig {
    pub address: String,
    pub kind: PointKind,
    pub unit: Option<String>,
    pub writable: bool,

    /// Conversion method
    pub conversion: ConversionMethod,
}

#[derive(Debug, Clone)]
pub enum ConversionMethod {
    /// No conversion (raw value)
    None,
    /// Linear: output = raw * scale + offset
    Linear { scale: f64, offset: f64 },
    /// Table lookup with interpolation
    Table { table_name: String },
    /// Range scaling: map [raw_min, raw_max] to [eng_min, eng_max]
    Range {
        raw_min: f64,
        raw_max: f64,
        eng_min: f64,
        eng_max: f64,
    },
}
```

---

## Sedona VM Integration (Rust Port)

### File Format Compatibility

The Rust Sedona VM maintains **full compatibility** with existing Sedona applications. Understanding the file formats is key:

| File | Type | Description | VM Dependent? |
|------|------|-------------|---------------|
| `.sedona` | Source | Component class source code | **No** (compiled by sedonac) |
| `.sax` | XML | Application configuration (component tree, links, properties) | **No** (just XML) |
| `.scode` | Binary | Compiled kit bytecode | **Yes** (VM interprets) |
| `.sab` | Binary | Compiled application (from .sax) | **Yes** (VM loads) |

### Compilation Flow (Unchanged)

```
                    sedonac (Java) - NO CHANGES
                           │
    .sedona files ─────────┼──────────► .scode (kit bytecode)
                           │
    .sax file ─────────────┼──────────► .sab (app binary)
                           │
                           ▼
              ┌────────────────────────┐
              │   Sedona VM (Rust)     │  ◄── Interprets same bytecode
              │   • Loads .sab         │
              │   • Executes .scode    │
              │   • Same opcodes       │
              └────────────────────────┘
```

**Key Point:** Your existing `.sax` files work unchanged. The sedonac compiler (Java) produces the same `.sab` and `.scode` files. Only the VM interpreter changes from C to Rust.

### Sedona VM Rust Architecture

The Rust VM (detailed in [doc 13](13_SEDONA_VM_RUST_PORTING_STRATEGY.md)) implements:

```rust
/// Sedona VM state
pub struct SedonaVm {
    /// Bytecode memory segments
    code_seg: Box<[u8]>,
    const_seg: Box<[u8]>,
    static_seg: Box<[u8]>,

    /// Execution state
    stack: Vec<Cell>,
    sp: usize,
    fp: usize,

    /// Component tree (loaded from .sab)
    app: SedonaApp,

    /// Reference to Driver Framework for I/O
    driver_manager: DriverManagerHandle,
}

/// The fundamental stack unit (matches C layout)
#[repr(C)]
#[derive(Clone, Copy)]
pub union Cell {
    pub ival: i32,
    pub fval: f32,
    pub aval: *mut u8,
}
```

### Native Method Bridge to Driver Framework

Sedona components access hardware via native methods. In the Rust VM, these call directly into the Driver Framework:

```rust
/// Native method: sys::Sys.read(int channel) -> float
pub fn sys_read(vm: &mut SedonaVm, channel: i32) -> f32 {
    // Convert channel to PointId (uses point database)
    let point_id = vm.resolve_channel_to_point(channel as u16);

    // Read from Driver Framework (blocking for VM compatibility)
    let handle = vm.driver_manager.clone();
    let result = tokio::runtime::Handle::current()
        .block_on(handle.read_point(point_id));

    match result {
        Ok(Value::Number(n)) => n.value as f32,
        _ => f32::NAN,
    }
}

/// Native method: sys::Sys.write(int channel, float value)
pub fn sys_write(vm: &mut SedonaVm, channel: i32, value: f32) {
    let point_id = vm.resolve_channel_to_point(channel as u16);
    let val = Value::make_number(value as f64);

    let handle = vm.driver_manager.clone();
    let _ = tokio::runtime::Handle::current()
        .block_on(handle.write_point(point_id, val, 16)); // Default priority 16
}
```

### Existing Sedona Components Still Work

Your existing Sedona control logic components continue to function:

| Component | Description | Works in Rust VM? |
|-----------|-------------|-------------------|
| `control::Pid` | PID controller | **Yes** |
| `control::Ramp` | Ramp generator | **Yes** |
| `hvac::*` | HVAC sequences | **Yes** |
| `schedule::*` | Time schedules | **Yes** |
| `logic::*` | Boolean logic | **Yes** |
| `math::*` | Math operations | **Yes** |
| Custom kits | Your custom components | **Yes** |

### VM Execution Model

```rust
impl SedonaVm {
    /// Main execution loop (same as C version)
    pub fn execute_cycle(&mut self) -> Result<(), VmError> {
        // 1. Execute App.start() on first cycle
        if !self.started {
            self.call_method("App", "start")?;
            self.started = true;
        }

        // 2. Execute each component in tree order
        for comp_id in self.app.execution_order() {
            self.execute_component(comp_id)?;
        }

        // 3. Process links (propagate slot values)
        self.process_links()?;

        Ok(())
    }

    /// Execute a single component
    fn execute_component(&mut self, comp_id: CompId) -> Result<(), VmError> {
        let comp = self.app.get_component(comp_id)?;

        // Call component's execute() method
        self.call_virtual(comp, "execute")?;

        Ok(())
    }
}
```

### Integration with Driver Framework

```
┌─────────────────────────────────────────────────────────────────────┐
│                     Sedona Application (.sax)                       │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐           │
│  │ AiPoint  │  │   Pid    │  │  Ramp    │  │ AoPoint  │           │
│  │ (input)  │──│(control) │──│ (output) │──│ (output) │           │
│  └────┬─────┘  └──────────┘  └──────────┘  └────┬─────┘           │
│       │                                          │                 │
│       │  Native: sys_read()      Native: sys_write()               │
└───────┼──────────────────────────────────────────┼─────────────────┘
        │                                          │
        ▼                                          ▼
┌───────────────────────────────────────────────────────────────────┐
│                    Driver Framework v2 (Rust)                      │
│                                                                    │
│  DriverManager.read_point(id)    DriverManager.write_point(id)    │
│         │                                   │                      │
│         ▼                                   ▼                      │
│  ┌─────────────────────────────────────────────────────────┐      │
│  │                    LocalIoDriver                         │      │
│  │  gpio-cdev │ i2cdev │ industrial-io │ serialport        │      │
│  └─────────────────────────────────────────────────────────┘      │
└───────────────────────────────────────────────────────────────────┘
```

### Benefits of Rust VM

| Aspect | C VM | Rust VM |
|--------|------|---------|
| Stack overflow | Debug-only check | Always bounds-checked |
| Null pointers | Runtime crash | Compile-time Option<T> |
| Memory leaks | Manual tracking | RAII Drop |
| Buffer overruns | Possible | Bounds-checked slices |
| Thread safety | Manual mutex | Send/Sync traits |
| Native methods | FFI to Rust | Direct Rust calls |

See [13_SEDONA_VM_RUST_PORTING_STRATEGY.md](13_SEDONA_VM_RUST_PORTING_STRATEGY.md) for complete VM porting details.

---

## DriverManager Actor

### Complete Implementation

```rust
use tokio::sync::{mpsc, oneshot, RwLock};
use std::sync::Arc;

/// Messages to DriverManager actor
pub enum DriverManagerMsg {
    // Lifecycle
    RegisterDriver(Box<dyn Driver>, oneshot::Sender<DriverId>),
    UnregisterDriver(DriverId),
    OpenDriver(DriverId, oneshot::Sender<Result<DriverMeta, DriverError>>),
    CloseDriver(DriverId),

    // Points
    AddPoint(DriverId, PointConfig, oneshot::Sender<Result<PointId, DriverError>>),
    RemovePoint(PointId),
    GetPoint(PointId, oneshot::Sender<Option<PointInfo>>),

    // Sync
    SyncCur(Vec<PointId>, oneshot::Sender<Vec<SyncResult>>),
    ReadPoint(PointId, oneshot::Sender<Result<Value, DriverError>>),

    // Write
    Write(Vec<WriteRequest>, oneshot::Sender<Vec<WriteResult>>),
    WritePoint(PointId, Value, u8, oneshot::Sender<Result<(), DriverError>>),

    // Watch
    AddWatch(WatchId, ClientId, Vec<PointId>, oneshot::Sender<()>),
    RemoveWatch(WatchId),
    ClientDisconnect(ClientId),

    // Learn
    Learn(DriverId, Option<String>, oneshot::Sender<Result<LearnGrid, DriverError>>),

    // Status
    GetDriverStatus(DriverId, oneshot::Sender<Option<DriverStatus>>),
    GetPointStatus(PointId, oneshot::Sender<Option<PointStatus>>),
    ListDrivers(oneshot::Sender<Vec<DriverInfo>>),
    ListPoints(Option<DriverId>, oneshot::Sender<Vec<PointInfo>>),
}

/// DriverManager - orchestrates all drivers and points
pub struct DriverManager {
    drivers: HashMap<DriverId, DriverHandle>,
    points: HashMap<PointId, PointHandle>,
    poll_scheduler: PollScheduler,
    watch_manager: WatchManager,
    rx: mpsc::Receiver<DriverManagerMsg>,

    // COV notification channel
    cov_tx: broadcast::Sender<CovEvent>,
}

/// Handle for interacting with DriverManager
#[derive(Clone)]
pub struct DriverManagerHandle {
    tx: mpsc::Sender<DriverManagerMsg>,
    cov_rx: broadcast::Receiver<CovEvent>,
}

impl DriverManagerHandle {
    pub async fn register_driver(&self, driver: Box<dyn Driver>) -> Result<DriverId, DriverError> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.tx.send(DriverManagerMsg::RegisterDriver(driver, resp_tx)).await
            .map_err(|_| DriverError::Internal("channel closed".into()))?;
        resp_rx.await.map_err(|_| DriverError::Internal("response dropped".into()))
    }

    pub async fn open_driver(&self, id: DriverId) -> Result<DriverMeta, DriverError> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.tx.send(DriverManagerMsg::OpenDriver(id, resp_tx)).await
            .map_err(|_| DriverError::Internal("channel closed".into()))?;
        resp_rx.await.map_err(|_| DriverError::Internal("response dropped".into()))?
    }

    pub async fn learn(&self, id: DriverId, path: Option<String>) -> Result<LearnGrid, DriverError> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.tx.send(DriverManagerMsg::Learn(id, path, resp_tx)).await
            .map_err(|_| DriverError::Internal("channel closed".into()))?;
        resp_rx.await.map_err(|_| DriverError::Internal("response dropped".into()))?
    }

    pub async fn read_point(&self, id: PointId) -> Result<Value, DriverError> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.tx.send(DriverManagerMsg::ReadPoint(id, resp_tx)).await
            .map_err(|_| DriverError::Internal("channel closed".into()))?;
        resp_rx.await.map_err(|_| DriverError::Internal("response dropped".into()))?
    }

    pub async fn write_point(&self, id: PointId, val: Value, level: u8) -> Result<(), DriverError> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.tx.send(DriverManagerMsg::WritePoint(id, val, level, resp_tx)).await
            .map_err(|_| DriverError::Internal("channel closed".into()))?;
        resp_rx.await.map_err(|_| DriverError::Internal("response dropped".into()))?
    }

    /// Subscribe to COV events
    pub fn subscribe_cov(&self) -> broadcast::Receiver<CovEvent> {
        self.cov_rx.resubscribe()
    }
}

impl DriverManager {
    pub fn spawn() -> DriverManagerHandle {
        let (tx, rx) = mpsc::channel(1000);
        let (cov_tx, cov_rx) = broadcast::channel(1000);

        let mut manager = Self {
            drivers: HashMap::new(),
            points: HashMap::new(),
            poll_scheduler: PollScheduler::new(),
            watch_manager: WatchManager::new(),
            rx,
            cov_tx,
        };

        tokio::spawn(async move {
            manager.run().await;
        });

        DriverManagerHandle { tx, cov_rx }
    }

    async fn run(&mut self) {
        let mut poll_interval = tokio::time::interval(Duration::from_millis(100));
        let mut stale_check_interval = tokio::time::interval(Duration::from_secs(10));
        let mut ping_interval = tokio::time::interval(Duration::from_secs(60));

        loop {
            tokio::select! {
                Some(msg) = self.rx.recv() => {
                    self.handle_message(msg).await;
                }
                _ = poll_interval.tick() => {
                    self.process_polling().await;
                }
                _ = stale_check_interval.tick() => {
                    self.check_stale_points();
                }
                _ = ping_interval.tick() => {
                    self.ping_all_drivers().await;
                }
            }
        }
    }

    async fn handle_message(&mut self, msg: DriverManagerMsg) {
        match msg {
            DriverManagerMsg::RegisterDriver(driver, resp) => {
                let id = driver.id();
                self.drivers.insert(id, DriverHandle {
                    driver,
                    status: DriverStatus::Pending,
                    points: HashSet::new(),
                });
                let _ = resp.send(id);
            }

            DriverManagerMsg::OpenDriver(id, resp) => {
                let result = if let Some(handle) = self.drivers.get_mut(&id) {
                    handle.status = DriverStatus::Syncing;
                    match handle.driver.on_open().await {
                        Ok(meta) => {
                            handle.status = DriverStatus::Ok;
                            Ok(meta)
                        }
                        Err(e) => {
                            handle.status = DriverStatus::Down;
                            Err(e)
                        }
                    }
                } else {
                    Err(DriverError::Fault(format!("driver {} not found", id.0)))
                };
                let _ = resp.send(result);
            }

            DriverManagerMsg::AddPoint(driver_id, config, resp) => {
                let result = self.add_point(driver_id, config);
                let _ = resp.send(result);
            }

            DriverManagerMsg::ReadPoint(point_id, resp) => {
                let result = self.read_single_point(point_id).await;
                let _ = resp.send(result);
            }

            DriverManagerMsg::Learn(driver_id, path, resp) => {
                let result = if let Some(handle) = self.drivers.get_mut(&driver_id) {
                    handle.driver.on_learn(path.as_deref()).await
                } else {
                    Err(DriverError::Fault(format!("driver {} not found", driver_id.0)))
                };
                let _ = resp.send(result);
            }

            // ... handle other messages
            _ => {}
        }
    }

    async fn process_polling(&mut self) {
        while let Some(bucket) = self.poll_scheduler.next_due() {
            let bucket_id = bucket.id;
            let point_refs: Vec<_> = bucket.points.iter()
                .filter_map(|id| self.points.get(id))
                .map(|p| DriverPointRef {
                    id: p.config.id,
                    address: p.config.address.clone(),
                })
                .collect();

            // Group by driver
            let by_driver = self.group_points_by_driver(&point_refs);

            for (driver_id, driver_points) in by_driver {
                if let Some(handle) = self.drivers.get_mut(&driver_id) {
                    if handle.status == DriverStatus::Ok {
                        let mut ctx = SyncContext {
                            updates: vec![],
                        };
                        handle.driver.on_sync_cur(&driver_points, &mut ctx).await;

                        // Apply updates
                        for update in ctx.updates {
                            self.apply_cur_update(update);
                        }
                    }
                }
            }

            self.poll_scheduler.mark_polled(bucket_id);
        }
    }

    fn apply_cur_update(&mut self, update: CurUpdate) {
        if let Some(point) = self.points.get_mut(&update.point_id) {
            let old_val = point.cur_val.clone();

            match update.result {
                Ok(val) => {
                    point.cur_val = Some(val.clone());
                    point.cur_status = PointStatus::Ok;
                    point.cur_timestamp = Some(Instant::now());

                    // Emit COV if value changed
                    if old_val.as_ref() != Some(&val) {
                        let _ = self.cov_tx.send(CovEvent {
                            point_id: update.point_id,
                            value: val,
                            status: PointStatus::Ok,
                            timestamp: chrono::Utc::now(),
                        });
                    }
                }
                Err(e) => {
                    point.cur_status = e.to_point_status();
                    point.debug_data.error_count += 1;
                    point.debug_data.last_error = Some(e.to_string());
                }
            }

            point.debug_data.read_count += 1;
            point.debug_data.last_read_time = Some(Instant::now());
        }
    }
}
```

---

## REST API Integration

### Driver Ops (Axum)

```rust
use axum::{
    Router, routing::{get, post, delete},
    Json, extract::{Path, State, Query},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

pub fn driver_routes(dm: DriverManagerHandle) -> Router {
    Router::new()
        // Driver management
        .route("/api/drivers", get(list_drivers).post(create_driver))
        .route("/api/drivers/:id", get(get_driver).delete(delete_driver))
        .route("/api/drivers/:id/open", post(open_driver))
        .route("/api/drivers/:id/close", post(close_driver))
        .route("/api/drivers/:id/ping", post(ping_driver))

        // Discovery
        .route("/api/drivers/:id/learn", get(learn_root))
        .route("/api/drivers/:id/learn/*path", get(learn_path))

        // Points
        .route("/api/points", get(list_points).post(create_point))
        .route("/api/points/:id", get(get_point).delete(delete_point))
        .route("/api/points/:id/read", post(read_point))
        .route("/api/points/:id/write", post(write_point))

        // Batch operations
        .route("/api/syncCur", post(sync_cur))
        .route("/api/write", post(batch_write))

        // Watches (for WebSocket upgrade)
        .route("/api/watches", post(create_watch))
        .route("/api/watches/:id", delete(close_watch))

        .with_state(dm)
}

#[derive(Deserialize)]
struct CreateDriverRequest {
    driver_type: String,
    config: serde_json::Value,
}

async fn create_driver(
    State(dm): State<DriverManagerHandle>,
    Json(req): Json<CreateDriverRequest>,
) -> Result<Json<DriverInfo>, (StatusCode, String)> {
    let driver: Box<dyn Driver> = match req.driver_type.as_str() {
        "localio" => {
            let config: LocalIoConfig = serde_json::from_value(req.config)
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
            Box::new(LocalIoDriver::new(config))
        }
        "modbus" => {
            let config: ModbusConfig = serde_json::from_value(req.config)
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
            Box::new(ModbusDriver::new(config))
        }
        _ => return Err((StatusCode::BAD_REQUEST, "unknown driver type".into())),
    };

    let id = dm.register_driver(driver).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(DriverInfo {
        id,
        driver_type: req.driver_type,
        status: DriverStatus::Pending,
    }))
}

async fn learn_root(
    State(dm): State<DriverManagerHandle>,
    Path(id): Path<String>,
) -> Result<Json<LearnGrid>, (StatusCode, String)> {
    let driver_id = DriverId::parse(&id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let grid = dm.learn(driver_id, None).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(grid))
}

async fn learn_path(
    State(dm): State<DriverManagerHandle>,
    Path((id, path)): Path<(String, String)>,
) -> Result<Json<LearnGrid>, (StatusCode, String)> {
    let driver_id = DriverId::parse(&id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let grid = dm.learn(driver_id, Some(path)).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(grid))
}

#[derive(Deserialize)]
struct WritePointRequest {
    val: Value,
    level: Option<u8>,
    duration: Option<u64>, // seconds
}

async fn write_point(
    State(dm): State<DriverManagerHandle>,
    Path(id): Path<String>,
    Json(req): Json<WritePointRequest>,
) -> Result<Json<()>, (StatusCode, String)> {
    let point_id = PointId::parse(&id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    dm.write_point(point_id, req.val, req.level.unwrap_or(16)).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(()))
}
```

---

## Migration Path

### Phase 1: Core Framework (Week 1-2)

1. Implement `Driver` trait and `DriverManager` actor
2. Implement `PollScheduler` with bucket support
3. Implement `WatchManager`
4. Implement `WritePriorityArray`
5. Unit tests for all core components

### Phase 2: LocalIoDriver (Week 3-4)

1. Implement GPIO via `gpio-cdev`
2. Implement ADC via `industrial-io`
3. Implement I2C via `i2cdev`
4. Implement PWM via sysfs
5. Implement value conversion tables
6. Test on BeagleBone hardware

### Phase 3: Sedona VM Port (Week 5-8)

See [13_SEDONA_VM_RUST_PORTING_STRATEGY.md](13_SEDONA_VM_RUST_PORTING_STRATEGY.md) for detailed plan:

1. Implement Cell type and stack
2. Implement opcode interpreter
3. Port `sys` kit native methods → Driver Framework
4. Port `inet` kit native methods → tokio networking
5. Implement scode loader
6. Test with existing .sab applications

### Phase 4: Protocol Drivers (Week 9-10)

1. Implement `ModbusDriver` (TCP + RTU)
2. Implement `BacnetDriver` (if needed)
3. Implement `MqttDriver` (if needed)

### Phase 5: REST API & Integration (Week 11-12)

1. Implement REST endpoints with Axum
2. Implement WebSocket COV streaming
3. Integration testing with existing .sax applications
4. Documentation

---

## Comparison: Before and After

### Before (C/C++)

```
Manual Zinc grid configuration
    ↓
Engine (C) loads grid, creates channels
    ↓
Engine polls sensors via sysfs
    ↓
IPC message queue to Sedona VM
    ↓
Sedona components read via native FFI
    ↓
C++ Haystack REST serves values
```

### After (Pure Rust)

```
User opens Learn UI
    ↓
DriverManager calls LocalIoDriver.on_learn()
    ↓
Driver discovers hardware (GPIO, ADC, I2C, PWM)
    ↓
User selects points, framework creates PointConfig
    ↓
Points added to PollBucket by poll_time
    ↓
PollScheduler calls driver.on_sync_cur(batch)
    ↓
Driver reads hardware via Rust HAL crates
    ↓
Sedona VM (Rust) executes existing .sax applications
    ↓
Axum REST API + ROX WebSocket serve values
```

---

## Benefits Summary

| Benefit | How Achieved |
|---------|--------------|
| **No C/C++ dependencies** | Pure Rust with HAL crates |
| **Sedona app compatibility** | Existing .sax/.sab run unchanged |
| **No FFI overhead** | Direct Rust calls, no unsafe boundaries |
| **Memory safety** | Rust ownership model throughout |
| **Automatic discovery** | Learn callbacks for hardware/protocols |
| **Efficient polling** | Buckets batch points with same frequency |
| **Real-time COV** | Tokio broadcast channels |
| **Clear error handling** | Typed `DriverError` enum |
| **Status inheritance** | Driver status cascades to points |
| **Safer VM** | Rust VM with bounds-checked stack |
| **Modern REST API** | Axum with async handlers |
| **Smaller binary** | ~2-4 MB vs ~8-12 MB with POCO |
| **Faster startup** | Single Rust runtime |

---

## Crate Dependencies

```toml
[dependencies]
# Async runtime
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"

# Hardware access
gpio-cdev = "0.5"
i2cdev = "0.5"
industrial-io = "0.5"
serialport = "4"

# Web framework
axum = "0.7"
tower = "0.4"
tower-http = { version = "0.5", features = ["cors", "trace"] }

# Haystack
libhaystack = "2.0"

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Time
chrono = { version = "0.4", features = ["serde"] }
chrono-tz = "0.8"

# Error handling
thiserror = "1"
anyhow = "1"

# Utilities
uuid = { version = "1", features = ["v4", "serde"] }
tracing = "0.1"
tracing-subscriber = "0.3"

# Protocol drivers (optional)
tokio-modbus = { version = "0.9", optional = true }

[features]
default = ["localio"]
localio = []
modbus = ["tokio-modbus"]
```

---

## References

### Related Research Documents

- [12_SEDONA_VM_ARCHITECTURE_ANALYSIS.md](12_SEDONA_VM_ARCHITECTURE_ANALYSIS.md) - Deep analysis of Sedona VM internals
- [13_SEDONA_VM_RUST_PORTING_STRATEGY.md](13_SEDONA_VM_RUST_PORTING_STRATEGY.md) - Complete VM porting strategy

### External References

- [Haxall Connector Framework](https://haxall.io/doc/docHaxall/Conns)
- [Haxall Custom Connectors](https://haxall.io/doc/docHaxall/CustomConns)
- [Project Haystack Points](https://project-haystack.org/doc/lib-phIoT/points)
- [Sedona Framework](https://www.sedona-alliance.org/)
- [gpio-cdev crate](https://docs.rs/gpio-cdev)
- [i2cdev crate](https://docs.rs/i2cdev)
- [industrial-io crate](https://docs.rs/industrial-io)
- [tokio-modbus](https://github.com/slowtec/tokio-modbus)
- [Axum web framework](https://docs.rs/axum)
