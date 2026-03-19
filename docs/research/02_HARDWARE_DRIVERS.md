# 02: Hardware Drivers -- C to Rust Migration

## Overview

The Sandstar engine communicates with BeagleBone hardware through eight C source files that interface with Linux sysfs, ioctl, and device nodes. This document provides a function-by-function analysis of each driver, maps every C construct to its Rust equivalent, identifies the exact Rust crates to use, and catalogs the memory safety issues that Rust eliminates.

**Source files (in engine/src/):**

| File | Lines | Interface | Kernel API |
|------|-------|-----------|------------|
| `anio.c` | 39 | Analog-to-Digital Converter | sysfs `/sys/bus/iio/devices/iio:deviceN/in_voltageN_raw` |
| `gpio.c` | 146 | Digital I/O | sysfs `/sys/class/gpio/` (export, direction, value, edge, active_low) |
| `i2cio.c` | 473 | I2C sensors (SDP510, SDP810) | ioctl `I2C_SLAVE` on `/dev/i2c-N` |
| `i2c_worker.c` | 617 | Async I2C thread pool | pthreads + eventfd + ioctl on `/dev/i2c-N` |
| `pwmio.c` | 289 | PWM output | sysfs `/sys/class/pwm/pwmchipN/pwm-N:M/` |
| `uartio.c` | 341 | Serial/UART sensors | termios on `/dev/ttySN` |
| `uart_async.c` | 423 | Async UART via epoll | epoll + termios on `/dev/ttySN` |
| `io.c` | 432 | Sysfs helpers + FD cache | `open`/`read`/`write`/`close`/`lseek` with FD caching |

**Total: 2,760 lines C --> estimated 1,600 lines Rust**

---

## 1. ADC Driver (anio.c) -- 39 Lines

### 1.1 What the C Code Does

The ADC driver is the simplest in the system. It reads raw 12-bit values (0-4095) from the BeagleBone's onboard AM335x ADC via the Linux IIO (Industrial I/O) sysfs interface.

**Types:**

```c
typedef unsigned int ANIO_DEVICE;   // IIO device index (typically 0)
typedef unsigned int ANIO_ADDRESS;  // ADC channel (0-6 on BeagleBone)
typedef double ANIO_VALUE;          // Raw ADC count as double
```

**Single function:**

```c
int anio_get_value(ANIO_DEVICE device, ANIO_ADDRESS address, ANIO_VALUE *value);
```

**Sysfs path constructed:**
```
/sys/bus/iio/devices/iio:device{device}/in_voltage{address}_raw
```

**Operation:**
1. Format sysfs path into stack buffer (`char sDevice[IO_MAXPATH+1]`)
2. Call `io_read(sDevice, sBuffer)` which opens, reads, closes the file
3. Parse string to `double` via `strtod(sBuffer, NULL)` -- note: ignores parse errors entirely
4. Write result through output pointer `*value`
5. Returns 0 on success, -1 on failure

**Data format:** The sysfs file contains a plain ASCII decimal number followed by a newline, e.g., `2048\n`. The `io_read` helper strips control characters (anything < space) to null bytes.

### 1.2 Rust Equivalent -- `std::fs` Direct Read

**Recommended crate:** None required. Use `std::fs::read_to_string` directly. The `industrial-io` crate is overkill for reading a single sysfs file, and it depends on `libiio` which adds a native library dependency to the cross-compilation. Direct sysfs access is the right choice here.

**Alternative:** For cached/high-frequency reads, use `std::fs::File` held open with `seek(SeekFrom::Start(0))` + `read` pattern (see Section 8 on the FD cache).

```rust
use std::fs;
use std::io;

/// IIO device index (typically 0 for onboard ADC)
type AnioDevice = u32;
/// ADC channel number (0-6 on BeagleBone)
type AnioAddress = u32;

/// Read raw 12-bit ADC value from IIO sysfs interface.
///
/// Returns the raw count (0-4095) for the AM335x 12-bit ADC.
/// Path: /sys/bus/iio/devices/iio:device{device}/in_voltage{address}_raw
pub fn anio_get_value(device: AnioDevice, address: AnioAddress) -> io::Result<f64> {
    let path = format!(
        "/sys/bus/iio/devices/iio:device{}/in_voltage{}_raw",
        device, address
    );
    let contents = fs::read_to_string(&path)?;
    contents.trim().parse::<f64>().map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidData, format!("ADC parse error: {}", e))
    })
}
```

### 1.3 Code Comparison

| Aspect | C | Rust |
|--------|---|------|
| Function signature | `int anio_get_value(ANIO_DEVICE, ANIO_ADDRESS, ANIO_VALUE*)` | `fn anio_get_value(AnioDevice, AnioAddress) -> io::Result<f64>` |
| Path construction | `snprintf` into stack buffer (128 bytes) | `format!` macro, heap-allocated `String` |
| File I/O | `io_read()` -> `open`/`read`/`close` per call | `fs::read_to_string()` -> same syscalls but RAII close |
| String parsing | `strtod(sBuffer, NULL)` -- ignores errors | `.parse::<f64>()` returns `Result`, errors propagated |
| Error handling | Returns -1, value unmodified | Returns `Err(io::Error)`, no output pointer |
| Buffer overflow risk | `sDevice[129]` with `snprintf` bounded | `String` grows dynamically, no overflow possible |

### 1.4 Memory Safety Issues Eliminated

1. **Silent parse failure:** The C code calls `strtod(sBuffer, NULL)`, discarding the end pointer. If the sysfs file contains garbage, `strtod` returns 0.0 silently. Rust's `parse()` returns `Err` which must be handled.

2. **Output pointer validity:** The C caller can pass `NULL` for `value`, causing undefined behavior. Rust returns the value directly; no pointer dereference needed.

3. **Stack buffer size assumption:** The C code uses `IO_MAXPATH` (128) for path formatting. While sufficient today, any future path longer than 128 bytes silently truncates via `snprintf`. Rust's `format!` produces a correctly-sized `String`.

### 1.5 Performance Considerations

The ADC is read once per poll cycle (~100ms-1s). The overhead difference between C `snprintf`+`open`+`read`+`close` and Rust `format!`+`read_to_string` is negligible at this frequency. The heap allocation for the `String` in Rust costs ~50ns, invisible compared to the kernel sysfs round-trip (~10-50us).

For high-frequency reads, the FD caching strategy described in Section 8 applies: hold a `File` open and use `seek(0)` + `read` instead of open/close per read.

---

## 2. GPIO Driver (gpio.c) -- 146 Lines

### 2.1 What the C Code Does

The GPIO driver controls digital I/O pins via the **deprecated** Linux sysfs GPIO interface (`/sys/class/gpio/`). This interface was deprecated in Linux 4.8 in favor of the character device (`/dev/gpiochipN`) API.

**Types:**

```c
typedef unsigned int GPIO_ADDRESS;   // GPIO pin number (kernel numbering)
typedef unsigned int GPIO_VALUE;     // 0 or 1

enum _GPIO_DIRECTION { GP_IN, GP_OUT, GP_HIGH, GP_LOW };
enum _GPIO_EDGE      { GP_NONE, GP_RISING, GP_FALLING, GP_BOTH };
enum _GPIO_ACTIVELOW  { GP_DISABLE, GP_ENABLE };
```

**Static string lookup tables:**

```c
static char *g_sDirection[] = {"in", "out", "high", "low", NULL};
static char *g_sEdge[]      = {"none", "rising", "falling", "both", NULL};
static char *g_sBoolean[]   = {"0", "1", NULL};
```

**Functions (10 total):**

| Function | Sysfs Path | Operation |
|----------|-----------|-----------|
| `gpio_exists(addr)` | `/sys/class/gpio/gpio{addr}` | `stat()` check |
| `gpio_export(addr)` | `/sys/class/gpio/export` | Write pin number |
| `gpio_unexport(addr)` | `/sys/class/gpio/unexport` | Write pin number |
| `gpio_get_direction(addr, *val)` | `/sys/class/gpio/gpio{addr}/direction` | Read & decode string |
| `gpio_set_direction(addr, val)` | `/sys/class/gpio/gpio{addr}/direction` | Write string |
| `gpio_get_edge(addr, *val)` | `/sys/class/gpio/gpio{addr}/edge` | Read & decode string |
| `gpio_set_edge(addr, val)` | `/sys/class/gpio/gpio{addr}/edge` | Write string |
| `gpio_get_activelow(addr, *val)` | `/sys/class/gpio/gpio{addr}/active_low` | Read & decode "0"/"1" |
| `gpio_set_activelow(addr, val)` | `/sys/class/gpio/gpio{addr}/active_low` | Write "0"/"1" |
| `gpio_get_value(addr, *val)` | `/sys/class/gpio/gpio{addr}/value` | Read & decode "0"/"1" |
| `gpio_set_value(addr, val)` | `/sys/class/gpio/gpio{addr}/value` | Write "0"/"1" |

The `io_decode` function reads a sysfs file and matches the string against a NULL-terminated array of possible responses, returning the matching index through the output pointer. This is used for direction, edge, and boolean values.

### 2.2 Rust Equivalent -- `gpio-cdev` v0.6

**Recommended crate:** `gpio-cdev` v0.6

The `gpio-cdev` crate uses the modern Linux GPIO character device API (`/dev/gpiochipN`) via ioctl, which is the recommended kernel interface since Linux 4.8. This is a direct improvement over the C code which uses the deprecated sysfs interface.

**Cargo.toml:**
```toml
[dependencies]
gpio-cdev = "0.6"
```

**Key differences from sysfs:**
- No export/unexport needed (chardev handles this internally)
- Lines are requested from a chip, not by global pin number
- Edge detection uses proper file descriptor polling (epoll-ready) instead of sysfs `edge` file
- Active low is a request flag, not a separate sysfs write

```rust
use gpio_cdev::{Chip, LineRequestFlags, EventRequestFlags, LineEventHandle};
use std::io;

/// GPIO direction
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GpioDirection {
    In,
    Out,
    High,  // Output, initial high
    Low,   // Output, initial low
}

/// GPIO edge detection mode
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GpioEdge {
    None,
    Rising,
    Falling,
    Both,
}

/// A configured GPIO line (replaces export/direction/value dance)
pub struct GpioLine {
    // gpio-cdev manages the line handle internally
    // When this struct is dropped, the line is automatically released
    handle: gpio_cdev::LineHandle,
    offset: u32,
}

impl GpioLine {
    /// Request a GPIO line for output
    pub fn output(chip_path: &str, offset: u32, initial: bool) -> io::Result<Self> {
        let mut chip = Chip::new(chip_path)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let handle = chip
            .get_line(offset)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
            .request(LineRequestFlags::OUTPUT, initial as u8, "sandstar")
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        Ok(GpioLine { handle, offset })
    }

    /// Request a GPIO line for input
    pub fn input(chip_path: &str, offset: u32) -> io::Result<Self> {
        let mut chip = Chip::new(chip_path)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let handle = chip
            .get_line(offset)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
            .request(LineRequestFlags::INPUT, 0, "sandstar")
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        Ok(GpioLine { handle, offset })
    }

    /// Read GPIO value (0 or 1)
    pub fn get_value(&self) -> io::Result<u8> {
        self.handle
            .get_value()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    /// Write GPIO value (0 or 1)
    pub fn set_value(&self, value: u8) -> io::Result<()> {
        self.handle
            .set_value(value)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }
}
// LineHandle is dropped automatically when GpioLine goes out of scope,
// which unexports the GPIO line. No manual unexport needed.
```

### 2.3 Code Comparison

| Aspect | C (sysfs) | Rust (gpio-cdev chardev) |
|--------|-----------|--------------------------|
| `gpio_export(addr)` | Write to `/sys/class/gpio/export` | `chip.get_line(offset).request(...)` |
| `gpio_unexport(addr)` | Write to `/sys/class/gpio/unexport` | Automatic on `Drop` |
| `gpio_set_direction(addr, GP_OUT)` | Write "out" to `direction` file | `LineRequestFlags::OUTPUT` at request time |
| `gpio_get_value(addr, *val)` | Open/read/close `value` file, decode "0"/"1" | `handle.get_value()` via ioctl |
| `gpio_set_value(addr, val)` | Open/write/close `value` file | `handle.set_value(val)` via ioctl |
| `gpio_set_edge(addr, GP_RISING)` | Write "rising" to `edge` file | `EventRequestFlags::RISING_EDGE` |
| `gpio_set_activelow(addr, GP_ENABLE)` | Write "1" to `active_low` file | `LineRequestFlags::ACTIVE_LOW` flag |

### 2.4 Memory Safety Issues Eliminated

1. **Enum-to-string array out-of-bounds:** The C code casts the enum to `int` and indexes directly into the string array: `g_sDirection[(int) value]`. If `value` is out of range (e.g., cast from an uninitialized variable or corrupted memory), this reads past the array bounds. In Rust, the enum is exhaustive and pattern-matched.

2. **Unchecked `io_decode` pointer cast:** The call `io_decode(sDevice, g_sBoolean, (int *) value)` casts a `GPIO_VALUE *` (unsigned int) to `int *`. On two's complement systems this works, but it is technically undefined behavior. Rust uses proper enum types.

3. **Forgotten unexport:** If the process crashes between `gpio_export` and `gpio_unexport`, the GPIO pin remains exported. Rust's `Drop` trait on `GpioLine` (via the `gpio-cdev` `LineHandle`) automatically releases the pin.

4. **String comparison with user-supplied data:** `io_decode` does `strcmp` against fixed strings. While safe here (strings are from the kernel), the Rust version avoids string-based APIs entirely, using ioctl values directly.

5. **Deprecated API migration:** The sysfs GPIO interface is deprecated since Linux 4.8. The Rust migration also upgrades to the supported chardev API, future-proofing against kernel removal of sysfs GPIO.

### 2.5 Performance Considerations

The `gpio-cdev` ioctl calls are faster than sysfs open/read/close for each GPIO operation. In the C code, `gpio_get_value` opens, reads, and closes the sysfs file each time. The chardev API uses a single ioctl call on an already-open file descriptor.

**Benchmark expectation:** GPIO reads go from ~30us (sysfs open/read/close) to ~5us (ioctl on cached fd). For the BeagleBone's 100ms poll cycle, both are negligible, but the improvement matters if GPIO polling frequency increases.

---

## 3. I2C Driver (i2cio.c) -- 473 Lines

### 3.1 What the C Code Does

The I2C driver communicates with sensors (SDP510 differential pressure, SDP810 differential pressure + temperature) on the BeagleBone's I2C bus. It supports two protocols and includes retry with exponential backoff.

**Types:**

```c
typedef unsigned int I2CIO_DEVICE;   // I2C bus number (0, 1, 2)
typedef unsigned int I2CIO_ADDRESS;  // 7-bit slave address (e.g., 0x40)
typedef unsigned short I2CIO_VALUE;  // 16-bit sensor raw value
```

**Device path:** `/dev/i2c-{device}` (direct mapping: device number = bus number)

**Kernel API:**
```c
int fd = open("/dev/i2c-2", O_RDWR);
ioctl(fd, I2C_SLAVE, address);    // Set slave address
write(fd, &cmd, cmd_len);          // Send command bytes
read(fd, response, response_len);  // Read response
close(fd);
```

**Functions:**

| Function | Protocol | Description |
|----------|----------|-------------|
| `i2cio_exists(device, address)` | N/A | Check if `/dev/i2c-N` exists via `stat` |
| `i2cio_get_measurement(device, addr, *val)` | SDP510 | Send 0xF1, read 3 bytes, CRC check (init=0) |
| `i2cio_get_measurement_labeled(device, addr, label, *val)` | SDP510/SDP810 | Label-based dispatch to appropriate protocol |
| `i2cio_get_measurement_with_retry(device, addr, label, *val)` | Both | Retry up to 3 times with exponential backoff (10ms, 20ms, 40ms) |
| `i2cio_submit_async_read(channel, device, addr, label, cb, ud)` | Both | Submit to i2c_worker thread pool |
| `i2cio_parse_response(label, data, len, *val)` | Both | Extract value from raw response bytes |
| `i2cio_reinit_sensor(device, addr, label)` | SDP810 | Sensor recovery: stop, reset, restart continuous mode |

**SDP510 Protocol:**
1. Send command byte `0xF1`
2. Read 3 bytes: `[MSB, LSB, CRC]`
3. CRC-8 with polynomial 0x131, init=0x00
4. Value = `(MSB << 8) | LSB` (unsigned 16-bit)

**SDP810 Protocol:**
1. Send trigger command: `[0x36, 0x2F]` (single-shot mode)
2. Wait 45ms for measurement
3. Read 9 bytes: `[DP_MSB, DP_LSB, CRC1, T_MSB, T_LSB, CRC2, SF_MSB, SF_LSB, CRC3]`
4. CRC-8 with polynomial 0x131, init=0xFF (Sensirion standard)
5. Differential pressure = signed `(DP_MSB << 8) | DP_LSB`
6. Temperature = signed `(T_MSB << 8) | T_LSB`, engineering value = raw / 200.0 degC

**Label-based dispatch:** The `label` string (from the points.csv configuration) is searched case-insensitively for "sdp810" and "_temp" to select protocol and value extraction.

**CRC implementation:** Custom CRC-8 with configurable init value (0x00 for SDP510, 0xFF for SDP810):

```c
static unsigned char i2cio_crc_init(unsigned char cData[], int nLength,
                                     unsigned int nPoly, unsigned char init)
{
    unsigned char cCrc = init;
    for (int n = 0; n < nLength; n++) {
        cCrc ^= cData[n];
        for (int b = 0; b < 8; b++) {
            unsigned char c = cCrc;
            cCrc <<= 1;
            if (c & 0x80) cCrc ^= nPoly;
        }
    }
    return cCrc;
}
```

### 3.2 Rust Equivalent -- `i2cdev` v0.6

**Recommended crate:** `i2cdev` v0.6

The `i2cdev` crate provides safe Rust wrappers around the Linux I2C device interface, implementing the `embedded-hal` I2C traits. It handles `open`, `ioctl(I2C_SLAVE)`, `read`, and `write` internally.

**Cargo.toml:**
```toml
[dependencies]
i2cdev = "0.6"
```

```rust
use i2cdev::core::I2CDevice;
use i2cdev::linux::LinuxI2CDevice;
use std::io;
use std::thread;
use std::time::Duration;

/// I2C sensor value (raw 16-bit, signed for SDP810 or unsigned for SDP510)
type I2cValue = i16;

/// Sensirion sensor type detected from channel label
#[derive(Debug, Clone, Copy, PartialEq)]
enum SensorType {
    Sdp510,
    Sdp810 { want_temp: bool },
}

/// CRC-8 with polynomial 0x131 and configurable init value
fn crc8(data: &[u8], init: u8) -> u8 {
    let mut crc = init;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            let msb = crc & 0x80;
            crc <<= 1;
            if msb != 0 {
                crc ^= 0x31; // Lower byte of 0x131
            }
        }
    }
    crc
}

/// Detect sensor type from label string
fn detect_sensor(label: &str) -> SensorType {
    let lower = label.to_ascii_lowercase();
    if lower.contains("sdp810") {
        SensorType::Sdp810 {
            want_temp: lower.contains("_temp"),
        }
    } else {
        SensorType::Sdp510
    }
}

/// Read measurement from I2C sensor with label-based protocol selection.
///
/// This is the equivalent of i2cio_get_measurement_labeled().
pub fn i2c_get_measurement(
    bus: u32,
    address: u16,
    label: &str,
) -> io::Result<I2cValue> {
    let path = format!("/dev/i2c-{}", bus);
    let mut dev = LinuxI2CDevice::new(&path, address)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    // LinuxI2CDevice::new() calls open() + ioctl(I2C_SLAVE) internally

    match detect_sensor(label) {
        SensorType::Sdp510 => {
            // Send trigger command
            dev.write(&[0xF1])
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

            // Read 3-byte response
            let mut buf = [0u8; 3];
            dev.read(&mut buf)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

            // Verify CRC (init=0x00 for SDP510)
            let expected_crc = buf[2];
            let computed_crc = crc8(&buf[0..2], 0x00);
            if computed_crc != expected_crc {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("SDP510 CRC mismatch: expected 0x{:02X}, got 0x{:02X}",
                            expected_crc, computed_crc),
                ));
            }

            Ok(((buf[0] as i16) << 8) | buf[1] as i16)
        }
        SensorType::Sdp810 { want_temp } => {
            // Send triggered measurement command
            dev.write(&[0x36, 0x2F])
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

            // Wait for measurement (45ms per Sensirion datasheet)
            thread::sleep(Duration::from_millis(45));

            // Read 9-byte response
            let mut buf = [0u8; 9];
            dev.read(&mut buf)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

            // Verify CRC on differential pressure word (init=0xFF for Sensirion)
            if crc8(&buf[0..2], 0xFF) != buf[2] {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SDP810 DP CRC mismatch",
                ));
            }
            // Verify CRC on temperature word
            if crc8(&buf[3..5], 0xFF) != buf[5] {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SDP810 temp CRC mismatch",
                ));
            }

            if want_temp {
                Ok(((buf[3] as i16) << 8) | buf[4] as i16)
            } else {
                Ok(((buf[0] as i16) << 8) | buf[1] as i16)
            }
        }
    }
    // LinuxI2CDevice is dropped here, closing the file descriptor automatically
}

/// Read measurement with retry and exponential backoff.
///
/// Equivalent of i2cio_get_measurement_with_retry().
/// Retries up to 3 times with 10ms, 20ms, 40ms delays.
pub fn i2c_get_measurement_retry(
    bus: u32,
    address: u16,
    label: &str,
) -> io::Result<I2cValue> {
    const MAX_RETRIES: u32 = 3;
    const BASE_DELAY_MS: u64 = 10;

    let mut last_err = None;

    for attempt in 0..MAX_RETRIES {
        match i2c_get_measurement(bus, address, label) {
            Ok(value) => return Ok(value),
            Err(e) => {
                last_err = Some(e);
                if attempt < MAX_RETRIES - 1 {
                    let delay = BASE_DELAY_MS * (1 << attempt);
                    thread::sleep(Duration::from_millis(delay));
                }
            }
        }
    }

    Err(last_err.unwrap())
}
```

### 3.3 Code Comparison

| Aspect | C | Rust |
|--------|---|------|
| Open device | `io_open("/dev/i2c-2", O_RDWR)` returns raw `int` fd | `LinuxI2CDevice::new("/dev/i2c-2", addr)` returns `Result<LinuxI2CDevice>` |
| Set slave address | `ioctl(hDevice, I2C_SLAVE, address)` -- can fail silently | Done inside `LinuxI2CDevice::new()`, failure propagated as `Err` |
| Send command | `write(hDevice, &cCmd, 1)` -- returns bytes written or -1 | `dev.write(&[0xF1])` -- returns `Result<()>` |
| Read response | `read(hDevice, cResponse, 3)` -- partial reads possible | `dev.read(&mut buf)` -- returns `Result<()>` |
| Close device | `io_close(hDevice)` -- manual, can be forgotten on error paths | Automatic via `Drop` when `LinuxI2CDevice` goes out of scope |
| CRC check | `i2cio_crc(cResponse, 2, I2CIO_CRCPOLY)` | `crc8(&buf[0..2], 0x00)` -- slice bounds checked |
| Label dispatch | `i2cio_label_contains()` custom case-insensitive search | `label.to_ascii_lowercase().contains("sdp810")` |
| Error handling | Returns -1, must check on every call | Returns `io::Result`, composable with `?` operator |

### 3.4 Memory Safety Issues Eliminated

1. **File descriptor leak on error paths:** In the C code, `i2cio_get_measurement_labeled` opens the device, then has multiple `io_close(hDevice)` calls in error branches. If a code change adds a new error path and forgets `io_close`, the fd leaks. Rust's `Drop` trait makes this impossible.

2. **Unchecked `strncpy` in async job label:** The C code does `strncpy(job.label, label, sizeof(job.label) - 1)` which is correct but fragile. The Rust version uses `String` or bounded copy with explicit slice operations.

3. **Partial read silent data corruption:** The C code checks `read(hDevice, cResponse, 3) != 3` but does not clear the response buffer first. If `read` returns a partial result on retry, stale bytes from a previous read could be present. Rust's `[0u8; 3]` initialization ensures a clean buffer.

4. **Signed/unsigned confusion:** The C code extracts `short rawDP = (short)((cResponse[0]<<8)|cResponse[1])`. The intermediate expression is `unsigned int` (due to implicit promotion), then truncated to `short`. The Rust version explicitly uses `i16` casts throughout, making the signedness clear.

5. **`label` null pointer dereference:** In C, if `label` is `NULL`, the `i2cio_label_contains` function checks for it, but the earlier `ENGINE_LOG_DEBUG` call with `label ? label : "(null)"` still evaluates `label` in a conditional. In Rust, the label is `&str` which cannot be null; `Option<&str>` would be used if optionality is needed.

6. **Race condition in `io_open`/`io_close` with shared fd:** If multiple threads call `i2cio_get_measurement` concurrently on the same bus, they each open+close the device. But if one thread's close races with another's ioctl/write/read (through fd reuse by the OS), data corruption can occur. The Rust version uses owned `LinuxI2CDevice` per call, or the async version uses Mutex-protected shared handles (see Section 4).

### 3.5 Performance Considerations

The `i2cdev` crate has zero overhead beyond the syscalls -- it is a thin wrapper. The CRC computation is identical. The main performance concern is the 45-50ms sleep for SDP810 measurements, which is inherent to the sensor and identical in both versions.

For high-frequency I2C reads, the per-call `open`/`close` in the synchronous API adds ~20us overhead. The async worker pool (Section 4) caches bus FDs to eliminate this, and the Rust async version will do the same.

---

## 4. I2C Worker Thread Pool (i2c_worker.c) -- 617 Lines

### 4.1 What the C Code Does

The I2C worker implements a thread pool pattern for non-blocking sensor reads. The main engine loop submits jobs to worker threads, which execute the blocking I2C transactions (including the 50ms sensor measurement delay) without stalling the engine.

**Architecture:**

```
Main Thread                     Worker Threads (1-4)
    |                               |
    |-- i2c_submit_job(job) ------->|  (round-robin to worker N)
    |                               |-- pthread_mutex_lock(worker.mutex)
    |                               |-- enqueue job
    |                               |-- pthread_cond_signal(worker.cond)
    |                               |
    |                               |  Worker loop:
    |                               |-- pthread_cond_wait (blocks until job)
    |                               |-- dequeue job
    |                               |-- i2c_do_transaction(job):
    |                               |   |-- pthread_mutex_lock(bus_transaction_mutex[bus])
    |                               |   |-- ioctl(fd, I2C_SLAVE, addr)
    |                               |   |-- select() for writable with timeout
    |                               |   |-- write(fd, cmd, cmd_len)
    |                               |   |-- usleep(wait_us)  // 50ms for SDP810
    |                               |   |-- select() for readable with timeout
    |                               |   |-- read(fd, data, data_len)
    |                               |   |-- CRC verification
    |                               |   |-- pthread_mutex_unlock(bus_transaction_mutex[bus])
    |                               |-- job.callback(channel, result, data, len, user_data)
    |                               |-- write(completion_fd, 1)  // signal via eventfd
    |<-- eventfd readable ----------|
    |-- i2c_drain_completions()     |
```

**Key data structures:**

```c
// Worker thread state
typedef struct {
    pthread_t thread;
    pthread_mutex_t mutex;
    pthread_cond_t cond;
    i2c_job_t jobs[ASYNC_I2C_JOB_QUEUE_SIZE];  // 32-slot circular queue
    int head;
    int tail;
    int running;
    int worker_id;
} i2c_worker_t;
```

**Global state:**
- `g_workers[4]` -- worker thread pool (up to 4 workers)
- `g_bus_fds[8]` -- persistent file descriptors per I2C bus (cached, not opened/closed per transaction)
- `g_bus_mutex` -- protects `g_bus_fds` array
- `g_bus_transaction_mutex[8]` -- per-bus locks serializing ioctl+write+read sequences
- `g_completion_fd` -- eventfd for signaling completions to the main loop
- `g_submit_mutex` -- protects worker selection during submission

**Critical concurrency detail:** The per-bus `g_bus_transaction_mutex` is essential. Without it, two worker threads could interleave on the same bus:

```
Thread A: ioctl(fd, I2C_SLAVE, 0x40)   // set slave to sensor A
Thread B: ioctl(fd, I2C_SLAVE, 0x25)   // set slave to sensor B (overwrites A!)
Thread A: write(fd, cmd_A)              // WRONG: talks to sensor B
Thread A: read(fd, data)                // WRONG: reads from sensor B
```

**Timed I/O:** The `select()` calls with timeout prevent workers from blocking forever on a hung sensor (SDP810 holding SDA low). Without this, blocked worker threads accumulate until the system becomes unresponsive (~1 hour with the SDP810 failure mode).

### 4.2 Rust Equivalent -- Tokio Tasks with Channels

**Recommended approach:** Replace the pthread worker pool with `tokio` async tasks and `tokio::sync::mpsc` channels. The per-bus mutex becomes a `tokio::sync::Mutex`, and the completion eventfd becomes a standard channel receive.

**Cargo.toml:**
```toml
[dependencies]
i2cdev = "0.6"
tokio = { version = "1", features = ["rt-multi-thread", "sync", "time", "macros"] }
```

```rust
use i2cdev::core::I2CDevice;
use i2cdev::linux::LinuxI2CDevice;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, oneshot};

/// Result codes matching the C async_result_t enum
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AsyncResult {
    Ok,
    ErrOpen,
    ErrSlave,
    ErrWrite,
    ErrRead,
    ErrCrc,
    ErrTimeout,
    ErrQueueFull,
    ErrNoWorker,
}

/// I2C job request (replaces i2c_job_t struct)
#[derive(Debug)]
pub struct I2cJob {
    pub channel: i32,
    pub bus: u32,
    pub addr: u16,
    pub cmd: Vec<u8>,              // Variable-length, no fixed [u8; 8]
    pub expected_len: usize,
    pub wait_ms: u64,
    pub label: String,             // Owned string, no fixed-size buffer
    pub reply: oneshot::Sender<I2cJobResult>,  // Type-safe completion
}

/// Job result (replaces callback + void* user_data)
#[derive(Debug)]
pub struct I2cJobResult {
    pub channel: i32,
    pub result: AsyncResult,
    pub data: Vec<u8>,
}

/// Per-bus state: cached FD + transaction lock
struct BusState {
    device: Option<LinuxI2CDevice>,  // None = not yet opened, Drop closes it
}

/// I2C worker pool (replaces all global statics in i2c_worker.c)
pub struct I2cWorkerPool {
    sender: mpsc::Sender<I2cJob>,
    // Handle to join the background task on shutdown
    _handle: tokio::task::JoinHandle<()>,
}

impl I2cWorkerPool {
    /// Create worker pool with N concurrent workers
    pub fn new(num_workers: usize) -> Self {
        let (sender, receiver) = mpsc::channel::<I2cJob>(128);
        let receiver = Arc::new(Mutex::new(receiver));

        // Per-bus locks (replaces g_bus_transaction_mutex array)
        let bus_locks: Arc<Vec<Mutex<BusState>>> = Arc::new(
            (0..8)
                .map(|_| Mutex::new(BusState { device: None }))
                .collect(),
        );

        let handle = tokio::spawn(async move {
            let mut workers = Vec::new();
            for worker_id in 0..num_workers {
                let rx = Arc::clone(&receiver);
                let locks = Arc::clone(&bus_locks);
                workers.push(tokio::spawn(async move {
                    loop {
                        // Dequeue job (mutex ensures only one worker gets each job)
                        let job = {
                            let mut rx = rx.lock().await;
                            match rx.recv().await {
                                Some(job) => job,
                                None => break, // Channel closed = shutdown
                            }
                        };

                        let result = Self::do_transaction(&locks, &job).await;

                        let _ = job.reply.send(result);
                    }
                }));
            }
            // Wait for all workers to exit
            for w in workers {
                let _ = w.await;
            }
        });

        I2cWorkerPool {
            sender,
            _handle: handle,
        }
    }

    /// Submit a job to the worker pool
    pub async fn submit(&self, job: I2cJob) -> Result<(), AsyncResult> {
        self.sender
            .send(job)
            .await
            .map_err(|_| AsyncResult::ErrNoWorker)
    }

    /// Execute I2C transaction (runs in worker task)
    async fn do_transaction(
        bus_locks: &[Mutex<BusState>],
        job: &I2cJob,
    ) -> I2cJobResult {
        let bus_idx = job.bus as usize;
        if bus_idx >= bus_locks.len() {
            return I2cJobResult {
                channel: job.channel,
                result: AsyncResult::ErrOpen,
                data: vec![],
            };
        }

        // Lock the bus for the entire transaction (prevents interleaving)
        let mut bus = bus_locks[bus_idx].lock().await;

        // Lazy-open the bus device
        if bus.device.is_none() {
            let path = format!("/dev/i2c-{}", job.bus);
            match LinuxI2CDevice::new(&path, job.addr) {
                Ok(dev) => bus.device = Some(dev),
                Err(_) => {
                    return I2cJobResult {
                        channel: job.channel,
                        result: AsyncResult::ErrOpen,
                        data: vec![],
                    };
                }
            }
        }

        let dev = bus.device.as_mut().unwrap();

        // Set slave address (changes for each job)
        if dev.set_slave_address(job.addr).is_err() {
            return I2cJobResult {
                channel: job.channel,
                result: AsyncResult::ErrSlave,
                data: vec![],
            };
        }

        // Send command
        if !job.cmd.is_empty() {
            if dev.write(&job.cmd).is_err() {
                return I2cJobResult {
                    channel: job.channel,
                    result: AsyncResult::ErrWrite,
                    data: vec![],
                };
            }
            if job.wait_ms > 0 {
                tokio::time::sleep(Duration::from_millis(job.wait_ms)).await;
            }
        }

        // Read response with timeout
        let mut data = vec![0u8; job.expected_len];
        let read_result = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::task::spawn_blocking({
                // NOTE: actual read must happen in spawn_blocking since
                // LinuxI2CDevice::read is a blocking syscall
                let expected_len = job.expected_len;
                // We need to pass the fd - in practice, we'd use the fd directly
                // This is a simplified illustration; see implementation notes below
                move || -> Result<Vec<u8>, ()> {
                    // In the actual implementation, this would call
                    // dev.read(&mut data) using the bus fd
                    Ok(vec![0u8; expected_len])
                }
            }),
        )
        .await;

        match read_result {
            Ok(Ok(Ok(d))) => {
                // CRC verification would happen here (same as Section 3)
                I2cJobResult {
                    channel: job.channel,
                    result: AsyncResult::Ok,
                    data: d,
                }
            }
            _ => I2cJobResult {
                channel: job.channel,
                result: AsyncResult::ErrTimeout,
                data: vec![],
            },
        }
    }
}

impl Drop for I2cWorkerPool {
    fn drop(&mut self) {
        // Dropping the sender closes the channel, which causes all workers
        // to exit their loop when recv() returns None.
        // The JoinHandle will be dropped, detaching the task.
        // All BusState devices will be dropped, closing their FDs.
    }
}
```

**Implementation note on blocking I/O:** The I2C `read()`/`write()` calls are blocking syscalls. In a tokio context, these must run inside `tokio::task::spawn_blocking` to avoid starving the async runtime. The actual implementation would hold the bus file descriptor in an `Arc<Mutex<File>>` and pass it into the blocking closure. Alternatively, use `tokio::fs::File` for truly async file I/O (which uses `spawn_blocking` internally on Linux).

### 4.3 Code Comparison

| Aspect | C (pthreads) | Rust (tokio) |
|--------|-------------|--------------|
| Worker pool | `pthread_create` x N, manual lifecycle | `tokio::spawn` x N, automatic task cancellation |
| Job queue | Fixed array circular buffer (32 slots) | `mpsc::channel(128)` -- backpressure via bounded channel |
| Queue full handling | Returns `ASYNC_ERR_QUEUE_FULL`, job lost | `send().await` blocks until space available, or use `try_send()` |
| Completion signal | `eventfd` + `write(fd, &val, 8)` | `oneshot::Sender` -- type-safe, zero-copy |
| Bus FD cache | `g_bus_fds[8]` global array + mutex | `Vec<Mutex<BusState>>` -- no global state |
| Transaction lock | `g_bus_transaction_mutex[8]` + manual lock/unlock | `bus_locks[idx].lock().await` -- RAII guard |
| Timeout | `select()` with `timeval` | `tokio::time::timeout(Duration)` |
| Shutdown | Set `running=0`, `pthread_cond_signal`, `pthread_join` x N | Drop channel sender, tasks exit on `recv() -> None` |
| All global state | 7 static variables + 5 mutexes | All encapsulated in `I2cWorkerPool` struct |

### 4.4 Memory Safety Issues Eliminated

1. **Shutdown race condition:** The C code has a `g_submit_mutex` to prevent submitting jobs during shutdown, but there is a window between checking `g_initialized` and accessing `g_workers[worker_id]` where shutdown could complete and destroy the worker. Rust's channel-based design makes this impossible: dropping the sender causes `recv()` to return `None`, and tasks exit cleanly.

2. **Job queue circular buffer overflow:** The C code's fixed-size queue (`ASYNC_I2C_JOB_QUEUE_SIZE = 32`) must be manually checked for fullness. The `head`/`tail` arithmetic can have off-by-one errors. Tokio's `mpsc::channel` handles this correctly.

3. **Bus FD cache use-after-close:** In C, `i2c_reset_bus` closes a cached fd. If a worker thread is currently using that fd (between ioctl and read), the close pulls the fd out from under it. In Rust, the `Mutex<BusState>` ensures exclusive access -- you cannot reset the bus while a transaction is in progress because the mutex is held.

4. **Callback pointer validity:** The C `i2c_callback_t` is a function pointer with `void *user_data`. If the user_data points to freed memory when the callback fires, it is undefined behavior. Rust's `oneshot::Sender<I2cJobResult>` is type-safe and ownership-tracked; it cannot reference freed memory.

5. **Per-bus mutex missed unlock:** The C code has multiple `pthread_mutex_unlock(&g_bus_transaction_mutex[actual_bus])` calls at each error return. Missing one causes deadlock. Rust's `MutexGuard` (from `.lock().await`) automatically unlocks on drop, making it impossible to forget.

6. **Global mutable state:** The C code has 7 static mutable variables (`g_workers`, `g_num_workers`, `g_completion_fd`, `g_next_worker`, `g_initialized`, `g_bus_fds`, `g_submit_mutex`). Rust's design encapsulates all state inside `I2cWorkerPool`, eliminating accidental global mutation.

### 4.5 Performance Considerations

The tokio task scheduler has slightly higher per-task overhead than raw pthreads (~200ns vs ~50ns for context switch). However, the I2C transactions dominate at 50ms each, making scheduler overhead invisible.

The `mpsc::channel` uses atomic operations for the fast path and only falls back to OS-level blocking when the channel is empty/full. This is comparable to the C `pthread_cond_wait` + `pthread_mutex_lock` combination.

Memory usage is lower: Rust tasks are ~256 bytes each vs ~8KB stack per pthread. On the BeagleBone's 512MB RAM, this difference is negligible for 2-4 workers, but matters if the design ever scales.

---

## 5. PWM Driver (pwmio.c) -- 289 Lines

### 5.1 What the C Code Does

The PWM driver controls hardware PWM outputs on BeagleBone pins (P9_14, P9_16, P8_19, P8_13) via the Linux sysfs PWM interface. It supports export/unexport, polarity, period, duty cycle, and enable.

**Types:**

```c
typedef unsigned int PWMIO_CHIP;     // PWM chip number (e.g., 4)
typedef unsigned int PWMIO_CHANNEL;  // Channel within chip (e.g., 0)
typedef unsigned int PWMIO_PERIOD;   // Period in nanoseconds
typedef unsigned int PWMIO_DUTY;     // Duty cycle in nanoseconds

enum _PWMIO_POLARITY { PWMIO_POLARITY_NORMAL, PWMIO_POLARITY_INVERSED };
enum _PWMIO_ENABLE   { PWMIO_ENABLE_DISABLED, PWMIO_ENABLE_ENABLED };
```

**Sysfs paths (new kernel format):**
```
/sys/class/pwm/pwmchip{chip}/export
/sys/class/pwm/pwmchip{chip}/unexport
/sys/class/pwm/pwmchip{chip}/pwm-{chip}:{channel}/polarity     ("normal" or "inversed")
/sys/class/pwm/pwmchip{chip}/pwm-{chip}:{channel}/period       (nanoseconds)
/sys/class/pwm/pwmchip{chip}/pwm-{chip}:{channel}/duty_cycle   (nanoseconds)
/sys/class/pwm/pwmchip{chip}/pwm-{chip}:{channel}/enable       ("0" or "1")
```

**Important performance detail:** The `set_polarity`, `set_period`, `set_duty`, and `set_enable` functions use `io_write_cached()` instead of `io_write()`. This caches the file descriptor and uses `lseek(0) + write` instead of `open + write + close` for each update. The comment in the code explains why:

> PWM writes happen every poll cycle (~100ms), and each write opens/closes 4 FDs. At 36,000 polls/hour, this was causing ~144,000 FD operations/hour. With caching, we only open each file once and reuse the FD.

**Chip resolution (`pwmio_resolve`):** This function resolves a BeagleBone pin address to its PWM chip number by traversing the sysfs device tree:
```
/sys/devices/platform/ocp/ -> look for *.epwmss -> look for *.pwm or *.ecap -> match address -> find pwmchipN
```
This three-level directory traversal uses `opendir`/`readdir`/`closedir` with nested loops and is by far the most complex function in the file (80 lines, 3 levels of nesting).

**PWM pin configuration (`pwmio_config_pwm`):** Iterates over hardcoded pin names and writes "pwm" to their pinmux state file to configure them as PWM outputs.

### 5.2 Rust Equivalent -- Direct sysfs via `std::fs`

**Recommended approach:** Direct sysfs access via `std::fs`. There is no mature, well-maintained Rust crate specifically for sysfs PWM. The `linux-embedded-hal` crate has PWM support through its `SysFsPwmChip` type, but for the specific needs here (including cached writes and chip resolution), direct sysfs access is cleaner and more maintainable.

```rust
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write as IoWrite, Read as IoRead};
use std::path::{Path, PathBuf};

/// PWM polarity
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PwmPolarity {
    Normal,
    Inversed,
}

impl PwmPolarity {
    fn as_str(&self) -> &'static str {
        match self {
            PwmPolarity::Normal => "normal",
            PwmPolarity::Inversed => "inversed",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s.trim() {
            "normal" => Some(PwmPolarity::Normal),
            "inversed" => Some(PwmPolarity::Inversed),
            _ => None,
        }
    }
}

/// A PWM channel with cached file descriptors for high-frequency writes.
///
/// This replaces the global `g_fdWriteCache` in io.c with per-channel
/// owned File handles that auto-close via Drop.
pub struct PwmChannel {
    chip: u32,
    channel: u32,
    base_path: PathBuf,
    // Cached file handles for high-frequency write operations
    polarity_fd: Option<File>,
    period_fd: Option<File>,
    duty_fd: Option<File>,
    enable_fd: Option<File>,
}

impl PwmChannel {
    /// Export and open a PWM channel
    pub fn new(chip: u32, channel: u32) -> io::Result<Self> {
        let base_path = PathBuf::from(format!(
            "/sys/class/pwm/pwmchip{}/pwm-{}:{}",
            chip, chip, channel
        ));

        // Export if not already exported
        if !base_path.exists() {
            let export_path = format!("/sys/class/pwm/pwmchip{}/export", chip);
            fs::write(&export_path, channel.to_string())?;
        }

        Ok(PwmChannel {
            chip,
            channel,
            base_path,
            polarity_fd: None,
            period_fd: None,
            duty_fd: None,
            enable_fd: None,
        })
    }

    /// Write to a sysfs file using cached FD (lseek + write pattern)
    fn write_cached(fd: &mut Option<File>, path: &Path, value: &str) -> io::Result<()> {
        let file = match fd {
            Some(ref mut f) => {
                f.seek(SeekFrom::Start(0))?;
                f
            }
            None => {
                *fd = Some(OpenOptions::new().write(true).open(path)?);
                fd.as_mut().unwrap()
            }
        };
        file.write_all(value.as_bytes())?;
        Ok(())
    }

    /// Read from a sysfs file
    fn read_sysfs(path: &Path) -> io::Result<String> {
        fs::read_to_string(path).map(|s| s.trim().to_string())
    }

    pub fn set_period(&mut self, nanoseconds: u32) -> io::Result<()> {
        let path = self.base_path.join("period");
        Self::write_cached(&mut self.period_fd, &path, &nanoseconds.to_string())
    }

    pub fn get_period(&self) -> io::Result<u32> {
        let path = self.base_path.join("period");
        Self::read_sysfs(&path)?
            .parse::<u32>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn set_duty_cycle(&mut self, nanoseconds: u32) -> io::Result<()> {
        let path = self.base_path.join("duty_cycle");
        Self::write_cached(&mut self.duty_fd, &path, &nanoseconds.to_string())
    }

    pub fn get_duty_cycle(&self) -> io::Result<u32> {
        let path = self.base_path.join("duty_cycle");
        Self::read_sysfs(&path)?
            .parse::<u32>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn set_polarity(&mut self, polarity: PwmPolarity) -> io::Result<()> {
        let path = self.base_path.join("polarity");
        Self::write_cached(&mut self.polarity_fd, &path, polarity.as_str())
    }

    pub fn set_enable(&mut self, enabled: bool) -> io::Result<()> {
        let path = self.base_path.join("enable");
        let value = if enabled { "1" } else { "0" };
        Self::write_cached(&mut self.enable_fd, &path, value)
    }
}

impl Drop for PwmChannel {
    fn drop(&mut self) {
        // Cached File handles are automatically closed by Drop
        // Optionally unexport the channel:
        let unexport_path = format!("/sys/class/pwm/pwmchip{}/unexport", self.chip);
        let _ = fs::write(&unexport_path, self.channel.to_string());
    }
}

/// Resolve BeagleBone pin address to PWM chip number.
///
/// Traverses /sys/devices/platform/ocp/ to find:
///   *.epwmss/ -> *.pwm or *.ecap/ -> pwm/ -> pwmchipN
///
/// Equivalent of pwmio_resolve() in C.
pub fn pwm_resolve(address: &str) -> io::Result<u32> {
    let ocp_path = Path::new("/sys/devices/platform/ocp");

    for entry1 in fs::read_dir(ocp_path)? {
        let entry1 = entry1?;
        let name1 = entry1.file_name();
        let name1_str = name1.to_string_lossy();

        // Look for *.epwmss directories
        if !name1_str.ends_with(".epwmss") {
            continue;
        }

        for entry2 in fs::read_dir(entry1.path())? {
            let entry2 = entry2?;
            let name2 = entry2.file_name();
            let name2_str = name2.to_string_lossy();

            // Look for *.pwm or *.ecap
            if !name2_str.ends_with(".pwm") && !name2_str.ends_with(".ecap") {
                continue;
            }

            // Extract device address (first 8 chars)
            let addr2: String = name2_str.chars().take(8).collect();
            if addr2 != address {
                continue;
            }

            // Look for pwmchipN in the pwm/ subdirectory
            let pwm_dir = entry2.path().join("pwm");
            if !pwm_dir.exists() {
                continue;
            }

            for entry3 in fs::read_dir(&pwm_dir)? {
                let entry3 = entry3?;
                let name3 = entry3.file_name();
                let name3_str = name3.to_string_lossy();

                if let Some(chip_str) = name3_str.strip_prefix("pwmchip") {
                    if let Ok(chip) = chip_str.parse::<u32>() {
                        return Ok(chip);
                    }
                }
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("PWM chip not found for address {}", address),
    ))
}
```

### 5.3 Code Comparison

| Aspect | C | Rust |
|--------|---|------|
| FD caching | Global `g_fdWriteCache[64]` with `strncpy` path keys | Per-`PwmChannel` `Option<File>` fields, no path lookup needed |
| Cache lookup | Linear scan of 64 entries comparing path strings | Direct field access -- O(1), no search |
| Unexport on exit | Manual call needed; forgotten if process crashes | `Drop` trait fires automatically |
| `pwmio_resolve` nested loops | 3 levels of `opendir`/`readdir`/`closedir` with manual `break`/`closedir` on error | `fs::read_dir` iterators with `?` operator; no manual cleanup |
| Format buffer | `sprintf(dir2, ...)` into `char[256]` -- no bounds check | `PathBuf::from(format!(...))` -- dynamically sized |
| Pin configuration | Hardcoded `char *sPwmPin[]` array | Could be a `const` array or loaded from config |

### 5.4 Memory Safety Issues Eliminated

1. **Unchecked `sprintf` in `pwmio_resolve`:** Line 207 uses `sprintf(dir2, PWMIO_INFFS "/%s", e1->d_name)` into a `char[256]` buffer. If a sysfs directory name exceeds ~220 characters, this overflows. Rust's `PathBuf` grows dynamically.

2. **Unchecked `sscanf` in `pwmio_resolve`:** Line 225 uses `sscanf(e2->d_name, "%8s", addr2)` into `char[16]`. The `%8s` format limits reading but leaves the result unterminated if exactly 8 non-space chars are read. Rust's string slicing and `parse` are safe.

3. **DIR pointer leak on nested error:** In the triple-nested loop, if `opendir(dir3)` returns NULL, `closedir(d2)` and `closedir(d1)` still need to be called. The code handles this with `break` to outer loops, which is correct but fragile. Rust's `read_dir` returns an iterator that cleans up automatically.

4. **FD cache path collision:** The global `g_fdWriteCache` uses string comparison to look up cached FDs. If two different sysfs paths happen to hash/compare the same (impossible in practice with correct paths, but a class of bug), writes go to the wrong device. Rust's per-channel `Option<File>` design eliminates path lookup entirely.

5. **FD cache exhaustion:** If more than `IO_FD_CACHE_SIZE` (64) sysfs paths are opened, the C code falls back to open/close per write with no warning. At high PWM update rates, this could cause FD exhaustion. Rust's per-channel design naturally bounds the number of open FDs to the number of active PWM channels.

### 5.5 Performance Considerations

The FD caching pattern is critical for PWM: at 100ms poll cycles, each PWM channel update writes to 4 sysfs files (polarity, period, duty, enable). Without caching, that is 4 `open`+`write`+`close` sequences per channel per poll = 12 syscalls. With caching, it is 4 `lseek`+`write` = 8 syscalls. The Rust version preserves this optimization by keeping `File` handles in the `PwmChannel` struct.

The `pwm_resolve` directory traversal happens once at startup and has no performance impact during normal operation.

---

## 6. UART Driver (uartio.c) -- 341 Lines

### 6.1 What the C Code Does

The UART driver provides synchronous serial port access for communicating with sensors that use RS-232/RS-485 protocols. It supports configurable baud rate, data bits, stop bits, and parity.

**Types:**

```c
typedef unsigned int UARTIO_DEVICE;   // UART device number (index into /dev/ttySN)
typedef unsigned int UARTIO_BAUD;     // Baud rate (300-230400)
typedef unsigned short UARTIO_VALUE;  // Parsed sensor value

typedef struct {
    UARTIO_BAUD baud;       // Baud rate
    int data_bits;          // 7 or 8
    int stop_bits;          // 1 or 2
    char parity;            // 'N', 'E', 'O'
} UARTIO_CONFIG;
```

**Device path:** `/dev/ttyS{device}`

**Kernel API:** POSIX termios for port configuration:

```c
struct termios tty;
tcgetattr(fd, &tty);           // Get current settings
cfsetispeed(&tty, B9600);      // Set input baud rate
cfsetospeed(&tty, B9600);      // Set output baud rate
tty.c_cflag |= (CLOCAL | CREAD);  // Enable receiver, ignore modem
tty.c_cflag &= ~CSIZE;
tty.c_cflag |= CS8;            // 8 data bits
tty.c_cflag &= ~PARENB;        // No parity
tty.c_cflag &= ~CSTOPB;        // 1 stop bit
tty.c_cflag &= ~CRTSCTS;       // No hardware flow control
tty.c_iflag &= ~(...);          // Raw input
tty.c_oflag &= ~OPOST;          // Raw output
tty.c_lflag &= ~(...);          // No echo, no canonical mode
tty.c_cc[VMIN] = 0;             // Non-blocking
tty.c_cc[VTIME] = 1;            // 100ms timeout
tcsetattr(fd, TCSANOW, &tty);  // Apply settings
tcflush(fd, TCIOFLUSH);         // Flush pending data
```

**Functions:**

| Function | Description |
|----------|-------------|
| `uartio_exists(device)` | Check if `/dev/ttySN` exists |
| `uartio_open(device, *config)` | Open port, configure termios, cache FD in `g_uart_fds[8]` |
| `uartio_close(fd)` | Close port, clear from cache |
| `uartio_read(fd, *buf, len)` | Non-blocking read (returns 0 if no data) |
| `uartio_write(fd, *buf, len)` | Write data |
| `uartio_get_measurement(device, label, *value)` | Full read cycle: open, wait for data, parse response |
| `uartio_parse_response(label, *data, len, *value)` | Parse ASCII numeric response |
| `uartio_register_async(channel, device, config, label, cb, ud)` | Register for async monitoring |
| `uartio_unregister_async(channel)` | Unregister (not implemented) |

**Measurement protocol:** The synchronous `uartio_get_measurement` implements a simple polling loop:
1. Open port (or use cached FD)
2. Read in a loop with retries (10 attempts, 100ms each = ~1 second total)
3. Accumulate bytes until newline (`\n` or `\r`) is found
4. Parse as ASCII decimal number via `strtod`
5. Clamp to 0-65535 and return as `unsigned short`

### 6.2 Rust Equivalent -- `serialport` v4

**Recommended crate:** `serialport` v4

The `serialport` crate provides a cross-platform serial port API that abstracts the termios configuration into a builder pattern. It supports both blocking and non-blocking modes.

**Cargo.toml:**
```toml
[dependencies]
serialport = "4"
```

```rust
use serialport::{self, SerialPort, DataBits, FlowControl, Parity, StopBits};
use std::io::{self, Read, Write as IoWrite};
use std::time::Duration;

/// UART configuration
#[derive(Debug, Clone)]
pub struct UartConfig {
    pub baud: u32,
    pub data_bits: DataBits,
    pub stop_bits: StopBits,
    pub parity: Parity,
}

impl Default for UartConfig {
    fn default() -> Self {
        UartConfig {
            baud: 9600,
            data_bits: DataBits::Eight,
            stop_bits: StopBits::One,
            parity: Parity::None,
        }
    }
}

/// An open UART port (replaces g_uart_fds cache and manual termios)
pub struct UartPort {
    port: Box<dyn SerialPort>,
    device: u32,
}

impl UartPort {
    /// Open a serial port with configuration
    pub fn open(device: u32, config: &UartConfig) -> io::Result<Self> {
        let path = format!("/dev/ttyS{}", device);
        let port = serialport::new(&path, config.baud)
            .data_bits(config.data_bits)
            .stop_bits(config.stop_bits)
            .parity(config.parity)
            .flow_control(FlowControl::None)
            .timeout(Duration::from_millis(100))
            .open()?;
        Ok(UartPort { port, device })
    }

    /// Read available data (non-blocking via timeout)
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.port.read(buf) {
            Ok(n) => Ok(n),
            Err(ref e) if e.kind() == io::ErrorKind::TimedOut => Ok(0),
            Err(e) => Err(e),
        }
    }

    /// Write data to port
    pub fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.port.write(data)
    }
}
// Drop: Box<dyn SerialPort> closes the file descriptor automatically

/// Read a measurement from a UART sensor.
///
/// Equivalent of uartio_get_measurement(): waits up to 1 second for a
/// newline-terminated ASCII numeric response.
pub fn uart_get_measurement(device: u32, label: &str) -> io::Result<u16> {
    let config = UartConfig::default();
    let mut port = UartPort::open(device, &config)?;

    let mut buffer = Vec::with_capacity(64);
    let mut temp = [0u8; 64];

    for _ in 0..10 {
        match port.read(&mut temp)? {
            0 => {
                std::thread::sleep(Duration::from_millis(100));
            }
            n => {
                buffer.extend_from_slice(&temp[..n]);
                // Check for line ending
                if buffer.contains(&b'\n') || buffer.contains(&b'\r') {
                    break;
                }
            }
        }
    }

    if buffer.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("No data from UART device {}", device),
        ));
    }

    // Parse ASCII numeric value
    let text = String::from_utf8_lossy(&buffer);
    let trimmed = text.trim();
    let value: f64 = trimmed.parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("UART parse error '{}': {}", trimmed, e),
        )
    })?;

    // Clamp to u16 range
    Ok(value.clamp(0.0, 65535.0) as u16)
}
```

### 6.3 Code Comparison

| Aspect | C (termios) | Rust (serialport) |
|--------|------------|-------------------|
| Port configuration | 30 lines of bitmask operations on `struct termios` | Builder pattern: `.data_bits(DataBits::Eight).parity(Parity::None)` |
| Baud rate conversion | Manual `switch` on 10 baud rates returning `B9600` etc. | Handled internally by crate |
| FD caching | `g_uart_fds[8]` global array | Owned `Box<dyn SerialPort>` in `UartPort` struct |
| Error handling | `strerror(errno)` in log messages | `io::Error` with full error chain |
| Buffer management | `uint8_t buffer[64]` on stack, manual position tracking | `Vec<u8>` grows as needed |
| Parse response | `strtod((char*)data, &endptr)` -- no error on invalid input | `.parse::<f64>()` returns `Result` |

### 6.4 Memory Safety Issues Eliminated

1. **Stack buffer overflow in measurement:** The C `uartio_get_measurement` reads into `uint8_t buffer[64]` with `sizeof(buffer) - 1 - total` bounds. If the loop logic has a bug and `total` exceeds 63, the subtraction wraps (since it is an `int` comparison against `sizeof` which is unsigned). Rust's `Vec` grows dynamically.

2. **`strtod` silent failure:** If the UART response is not a valid number, `strtod` returns 0.0 with `endptr == (char*)data`. The C code checks this but returns -1, which is correct. However, if the check were ever removed during refactoring, 0.0 would silently propagate. Rust's `parse()` forces explicit error handling.

3. **Config pointer null dereference:** `uartio_open` defaults to `{9600, 8, 1, 'N'}` if `config` is NULL. But it dereferences `config` first: `if (config) { cfg = *config; }`. If a developer changes the logic to use `config->baud` before the NULL check, it is UB. Rust uses `Option<&UartConfig>` which cannot be dereferenced without matching.

4. **FD leak in cache:** The `g_uart_fds` cache in C does not close FDs on process exit -- the OS reclaims them, but it is sloppy practice and can cause issues in test harnesses that create/destroy the driver multiple times. Rust's `Drop` on `UartPort` closes the port explicitly.

5. **parity char comparison:** The C code compares `cfg.parity == 'N'` using a char. If the parity field is uninitialized or set to lowercase `'n'`, the comparison fails silently. Rust's `Parity` enum makes this impossible.

### 6.5 Performance Considerations

The `serialport` crate is a thin wrapper over termios, with negligible overhead. The main performance concern is the same in both versions: the 1-second polling loop for data arrival. In the async version (Section 7), this becomes event-driven via epoll, eliminating the polling entirely.

---

## 7. Async UART (uart_async.c) -- 423 Lines

### 7.1 What the C Code Does

The async UART module uses Linux epoll to monitor multiple serial ports simultaneously in a single background thread. When data arrives on any port, a registered callback is invoked.

**Architecture:**

```
uart_epoll_thread (single background thread):
    epoll_create1() -> g_epoll_fd
    loop:
        epoll_wait(g_epoll_fd, events, 16, 100ms)
        for each EPOLLIN event:
            port_id = events[i].data.ptr
            bytes = read(g_ports[port_id].fd, buf + read_pos, space)
            g_ports[port_id].on_data(port_id, read_buf, read_pos, user_data)
            reset read_pos to 0
```

**Key data structures:**

```c
typedef struct {
    int fd;                             // File descriptor (-1 if unused)
    char device[32];                    // Device path
    int baud;                           // Baud rate
    uint8_t read_buf[256];              // Read buffer
    int read_pos;                       // Current position
    uart_callback_t on_data;            // Data callback
    void *user_data;                    // Callback data
    int channel;                        // Associated engine channel
} uart_port_t;
```

**Global state:**
- `g_ports[8]` -- port state array
- `g_epoll_fd` -- epoll instance
- `g_uart_thread` -- background thread handle
- `g_uart_running` -- volatile flag for thread shutdown
- `g_uart_mutex` -- protects port array modifications

**Edge-triggered epoll:** The `EPOLLET` flag means the callback fires once when data first becomes available. The thread must read all available data in one shot; otherwise, subsequent data on the same descriptor won't trigger another event until new data arrives. The code handles this by reading as much as possible into the 256-byte buffer.

**Buffer overflow handling:** When the 256-byte read buffer fills up, the code resets `read_pos` to 0, losing any accumulated data. This is a trade-off: the buffer is sized for typical sensor messages (< 256 bytes). If a sensor sends more, data is lost.

### 7.2 Rust Equivalent -- Tokio + `serialport`

**Recommended approach:** Use `tokio::io::AsyncRead` on the serial port file descriptor, wrapped in a `tokio::task::spawn_blocking` or using the `tokio-serial` crate (which bridges `serialport` with tokio).

**Cargo.toml:**
```toml
[dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "sync", "io-util", "net", "macros"] }
tokio-serial = "5"   # Async wrapper for serialport crate
```

```rust
use tokio::sync::mpsc;
use tokio_serial::{SerialPortBuilderExt, SerialStream};
use tokio::io::AsyncReadExt;
use std::collections::HashMap;

/// Data received from a UART port
#[derive(Debug)]
pub struct UartEvent {
    pub port_id: usize,
    pub channel: i32,
    pub data: Vec<u8>,
}

/// Async UART manager (replaces uart_async.c globals)
pub struct UartManager {
    /// Channel for receiving events from port tasks
    event_rx: mpsc::Receiver<UartEvent>,
    /// Sender cloned for each port task
    event_tx: mpsc::Sender<UartEvent>,
    /// Active port task handles (for shutdown)
    tasks: HashMap<usize, tokio::task::JoinHandle<()>>,
    next_port_id: usize,
}

impl UartManager {
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel(256);
        UartManager {
            event_rx,
            event_tx,
            tasks: HashMap::new(),
            next_port_id: 0,
        }
    }

    /// Add a UART port for async monitoring.
    ///
    /// Returns a port_id that can be used to remove the port later.
    /// Equivalent of uart_add_port() in C.
    pub fn add_port(
        &mut self,
        device: &str,
        baud: u32,
        channel: i32,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        let port_id = self.next_port_id;
        self.next_port_id += 1;

        let tx = self.event_tx.clone();
        let device = device.to_string();

        let handle = tokio::spawn(async move {
            // Open serial port with tokio async support
            let mut port = match tokio_serial::new(&device, baud)
                .open_native_async()
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!("Failed to open {}: {}", device, e);
                    return;
                }
            };

            let mut buf = [0u8; 256];

            loop {
                // Async read -- suspends the task until data arrives
                // This replaces the epoll_wait + EPOLLIN handling
                match port.read(&mut buf).await {
                    Ok(0) => {
                        tracing::warn!("UART port {} EOF", device);
                        break;
                    }
                    Ok(n) => {
                        let event = UartEvent {
                            port_id,
                            channel,
                            data: buf[..n].to_vec(),
                        };
                        if tx.send(event).await.is_err() {
                            break; // Manager dropped
                        }
                    }
                    Err(e) => {
                        tracing::error!("UART read error on {}: {}", device, e);
                        break;
                    }
                }
            }
        });

        self.tasks.insert(port_id, handle);
        Ok(port_id)
    }

    /// Remove a UART port from monitoring
    pub fn remove_port(&mut self, port_id: usize) {
        if let Some(handle) = self.tasks.remove(&port_id) {
            handle.abort(); // Cancel the task
            // The SerialStream inside the task is dropped, closing the FD
        }
    }

    /// Receive the next UART event (async)
    pub async fn recv(&mut self) -> Option<UartEvent> {
        self.event_rx.recv().await
    }
}

impl Drop for UartManager {
    fn drop(&mut self) {
        // Abort all port tasks; their SerialStreams are dropped, closing FDs
        for (_, handle) in self.tasks.drain() {
            handle.abort();
        }
    }
}
```

### 7.3 Code Comparison

| Aspect | C (epoll + pthread) | Rust (tokio) |
|--------|--------------------|----|
| Event loop | `epoll_wait()` in manual loop + `EPOLLET` | `port.read().await` -- tokio handles epoll internally |
| Per-port buffer | `uint8_t read_buf[256]` fixed in struct | `[0u8; 256]` local to task, or `Vec<u8>` if dynamic |
| Port addition | Open + termios config + `epoll_ctl(EPOLL_CTL_ADD)` | `tokio_serial::new().open_native_async()` -- builder handles everything |
| Port removal | `epoll_ctl(EPOLL_CTL_DEL)` + `close(fd)` | `handle.abort()` -- task stops, `SerialStream` dropped, fd closed |
| Callback mechanism | `uart_callback_t` function pointer + `void*` | `mpsc::Sender<UartEvent>` -- type-safe channel |
| Thread management | `pthread_create` + `volatile g_uart_running` + `pthread_join` | `tokio::spawn` + `handle.abort()` -- structured concurrency |
| Edge-trigger handling | Must read all data or miss events | Tokio handles re-arming internally |
| Shutdown signal | `g_uart_running = 0; pthread_join()` | Drop `UartManager` -- all tasks aborted |

### 7.4 Memory Safety Issues Eliminated

1. **Buffer overflow on burst data:** The C code has `int space = ASYNC_UART_BUF_SIZE - p->read_pos`. If `read_pos` somehow exceeds `ASYNC_UART_BUF_SIZE`, `space` goes negative, and `read()` gets a negative length (interpreted as a very large unsigned value), causing a buffer overflow. Rust's slice bounds checking prevents this.

2. **Data pointer cast in epoll:** `ev.data.ptr = (void *)(intptr_t)port_id` stores an integer as a pointer. On retrieval, `int port_id = (int)(intptr_t)events[i].data.ptr` casts back. This is technically implementation-defined. Rust's tokio approach avoids raw pointer aliasing entirely.

3. **Callback after port removal:** If `uart_remove_port` runs between `epoll_wait` returning an event and the thread processing it, the callback fires on a closed port. The C code checks `p->fd < 0`, but there is a TOCTOU race. Rust's task cancellation via `handle.abort()` eliminates the race.

4. **`volatile` correctness:** The C code uses `volatile int g_uart_running` for thread communication. While `volatile` prevents compiler optimization, it does not provide memory ordering guarantees needed for multi-threaded correctness (C11 `_Atomic` would be needed). In Rust, `AtomicBool` with proper ordering (or, better yet, structured cancellation via task abort) handles this correctly.

5. **Forgotten mutex unlock:** The `uart_add_port` function locks `g_uart_mutex` and has multiple error paths (open fails, tcgetattr fails, tcsetattr fails, epoll_ctl fails). Each error path must unlock and close the fd. Missing either causes deadlock or fd leak. Rust's `MutexGuard` automatically unlocks on drop.

### 7.5 Performance Considerations

Tokio's epoll integration uses the same underlying `epoll_wait` syscall as the C code. The difference is in task scheduling overhead: tokio adds ~200-500ns per wake-up for its task scheduler, which is negligible for serial data arriving at 9600-115200 baud (1 byte every 87-1042us).

The `tokio_serial` crate avoids the edge-trigger complexity of `EPOLLET` by using level-triggered epoll internally with proper wake-up handling. This is actually more correct than the C code's edge-triggered approach, which can miss events if the read does not drain all available data.

---

## 8. I/O Helpers and FD Cache (io.c) -- 432 Lines

### 8.1 What the C Code Does

The `io.c` file provides the foundation that all other drivers build upon. It has two layers:

**Layer 1: Basic sysfs operations (lines 1-158)**

| Function | Operation | Notes |
|----------|-----------|-------|
| `io_exists(sDevice)` | `stat(sDevice)` | Check if sysfs path exists |
| `io_open(sDevice, nOpen)` | `open(sDevice, nOpen)` | Open with flags, returns raw fd |
| `io_close(hDevice)` | `close(hDevice)` | Close raw fd |
| `io_read(sDevice, sBuffer)` | `open` + `read(fd, buf, IO_MAXBUFFER)` + `close` | Read up to 32 bytes, strip control chars |
| `io_write(sDevice, sBuffer)` | `open` + `write(fd, buf, len)` + `close` | Write string |
| `io_decode(sDevice, sResponses[], *value)` | `io_read` + `strcmp` against response table | Decode enum-like sysfs values |
| `io_ext(sDevice)` | `strrchr(sDevice, '.')` | Extract file extension |

The `io_read` function strips all characters with ASCII value < 32 (space) by replacing them with null bytes. This removes newlines and other control characters from sysfs reads.

**Layer 2: File descriptor cache (lines 160-432)**

The FD cache is a critical performance optimization. It keeps sysfs files open between reads/writes, using `lseek(0, SEEK_SET)` + `read`/`write` instead of `open` + `read`/`write` + `close` for each operation.

**Data structure:**

```c
typedef struct {
    char sDevice[IO_MAXPATH+1];  // Sysfs path (128 + 1 bytes)
    int fd;                       // Open file descriptor (-1 if empty)
} IO_FD_CACHE_ENTRY;

// Read cache (O_RDONLY)
static IO_FD_CACHE_ENTRY g_fdCache[IO_FD_CACHE_SIZE];       // 64 entries
static int g_fdCacheCount = 0;

// Write cache (O_WRONLY) - separate because same file may need both read and write FDs
static IO_FD_CACHE_ENTRY g_fdWriteCache[IO_FD_CACHE_SIZE];  // 64 entries
static int g_fdWriteCacheCount = 0;
```

**Cache operations:**

| Function | Description |
|----------|-------------|
| `io_cache_init()` | Initialize all 128 entries (64 read + 64 write) to empty |
| `io_cache_cleanup()` | Close all cached FDs |
| `io_cache_find(sDevice)` | Linear search read cache by path string comparison |
| `io_cache_add(sDevice, fd)` | Add to read cache if space available |
| `io_clear_cache(sDevice)` | Remove specific entry (on lseek failure) |
| `io_read_cached(sDevice, sBuffer)` | Find-or-open FD, lseek(0), read, strip control chars |
| `io_write_cache_find(sDevice)` | Linear search write cache |
| `io_write_cache_add(sDevice, fd)` | Add to write cache if space available |
| `io_write_cached(sDevice, sBuffer)` | Find-or-open FD, lseek(0), write |

**Performance impact documented in the code:**
> PWM writes happen every poll cycle (~100ms), and each write opens/closes 4 FDs. At 36,000 polls/hour, this was causing ~144,000 FD operations/hour. With caching, we only open each file once and reuse the FD.

### 8.2 Rust Equivalent -- Struct-Owned `File` Objects with RAII

The Rust approach replaces the global FD cache with file handles owned by the driver structs. Each driver (PwmChannel, etc.) holds `Option<File>` fields for the sysfs files it accesses frequently. This provides the same caching benefit without a global lookup table.

For the basic sysfs operations, Rust's `std::fs` functions are direct replacements.

```rust
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write as IoWrite, Seek, SeekFrom};
use std::path::Path;
use std::collections::HashMap;

/// Check if a sysfs device path exists.
/// Equivalent of io_exists().
pub fn sysfs_exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// Read a sysfs file and return its content as a trimmed string.
/// Equivalent of io_read() -- strips control characters automatically
/// since we use read_to_string + trim.
pub fn sysfs_read(path: &str) -> io::Result<String> {
    fs::read_to_string(path).map(|s| s.trim().to_string())
}

/// Write a string value to a sysfs file.
/// Equivalent of io_write().
pub fn sysfs_write(path: &str, value: &str) -> io::Result<()> {
    fs::write(path, value)
}

/// Read and decode a sysfs enum value against a list of possible responses.
/// Equivalent of io_decode().
///
/// Returns the index of the matching response, or an error.
pub fn sysfs_decode(path: &str, responses: &[&str]) -> io::Result<usize> {
    let content = sysfs_read(path)?;
    responses
        .iter()
        .position(|&r| r == content)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unknown sysfs value '{}' from {}, expected one of {:?}",
                        content, path, responses),
            )
        })
}

/// A cached sysfs file handle for high-frequency read operations.
/// Uses the lseek(0) + read pattern instead of open/close per read.
///
/// This replaces the global IO_FD_CACHE_ENTRY arrays in io.c.
/// Instead of a global cache with string-based lookup, each driver
/// owns its cached File handles directly.
pub struct CachedSysfsReader {
    file: File,
    buf: Vec<u8>,
}

impl CachedSysfsReader {
    /// Open a sysfs file for cached reading
    pub fn open(path: &str) -> io::Result<Self> {
        let file = File::open(path)?;
        Ok(CachedSysfsReader {
            file,
            buf: vec![0u8; 64],
        })
    }

    /// Read fresh value using lseek + read pattern
    pub fn read_value(&mut self) -> io::Result<String> {
        self.file.seek(SeekFrom::Start(0))?;
        let n = self.file.read(&mut self.buf)?;
        let content = std::str::from_utf8(&self.buf[..n])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(content.trim().to_string())
    }
}
// File is closed automatically when CachedSysfsReader is dropped

/// A cached sysfs file handle for high-frequency write operations.
pub struct CachedSysfsWriter {
    file: File,
}

impl CachedSysfsWriter {
    /// Open a sysfs file for cached writing
    pub fn open(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new().write(true).open(path)?;
        Ok(CachedSysfsWriter { file })
    }

    /// Write value using lseek + write pattern
    pub fn write_value(&mut self, value: &str) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(value.as_bytes())
    }
}
// File is closed automatically when CachedSysfsWriter is dropped

/// FD cache for use cases where a global cache is needed
/// (e.g., when the same sysfs path is accessed from multiple call sites).
///
/// This is a HashMap replacement for the C linear-search array.
pub struct SysfsFdCache {
    read_cache: HashMap<String, File>,
    write_cache: HashMap<String, File>,
}

impl SysfsFdCache {
    pub fn new() -> Self {
        SysfsFdCache {
            read_cache: HashMap::new(),
            write_cache: HashMap::new(),
        }
    }

    /// Read from sysfs with cached FD (lseek + read)
    pub fn read_cached(&mut self, path: &str) -> io::Result<String> {
        if !self.read_cache.contains_key(path) {
            let file = File::open(path)?;
            self.read_cache.insert(path.to_string(), file);
        }

        let file = self.read_cache.get_mut(path).unwrap();
        file.seek(SeekFrom::Start(0))?;

        let mut buf = [0u8; 64];
        let n = file.read(&mut buf)?;

        let content = std::str::from_utf8(&buf[..n])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(content.trim().to_string())
    }

    /// Write to sysfs with cached FD (lseek + write)
    pub fn write_cached(&mut self, path: &str, value: &str) -> io::Result<()> {
        if !self.write_cache.contains_key(path) {
            let file = OpenOptions::new().write(true).open(path)?;
            self.write_cache.insert(path.to_string(), file);
        }

        let file = self.write_cache.get_mut(path).unwrap();
        file.seek(SeekFrom::Start(0))?;
        file.write_all(value.as_bytes())
    }

    /// Clear a specific path from the cache (on error recovery)
    pub fn clear(&mut self, path: &str) {
        self.read_cache.remove(path);   // File closed by Drop
        self.write_cache.remove(path);  // File closed by Drop
    }

    /// Close all cached FDs
    pub fn cleanup(&mut self) {
        self.read_cache.clear();   // All Files closed by Drop
        self.write_cache.clear();  // All Files closed by Drop
    }
}

impl Drop for SysfsFdCache {
    fn drop(&mut self) {
        // HashMap drop calls File drop for each entry, closing all FDs.
        // This is automatic -- no manual cleanup needed.
    }
}
```

### 8.3 The FD Cache: C Global Array vs. Rust Owned Handles

This is the most important architectural difference in the migration. Here is a detailed comparison:

**C approach: Global FD cache with string-keyed lookup**

```
                  g_fdCache[64]                    g_fdWriteCache[64]
                  ┌─────────────────────┐           ┌─────────────────────┐
io_read_cached()  │ sDevice: "/sys/..." │           │ sDevice: "/sys/..." │
   ────────────►  │ fd: 7               │           │ fd: 12              │  ◄── io_write_cached()
                  ├─────────────────────┤           ├─────────────────────┤
                  │ sDevice: "/sys/..." │           │ sDevice: "/sys/..." │
                  │ fd: 8               │           │ fd: 13              │
                  ├─────────────────────┤           ├─────────────────────┤
                  │ ...                 │           │ ...                 │
                  │ (64 fixed slots)    │           │ (64 fixed slots)    │
                  └─────────────────────┘           └─────────────────────┘

Problems:
- Linear search O(N) for every access
- Fixed size: cache full = fallback to uncached
- Global mutable state: not thread-safe without external mutex
- FD lifetime not tied to any owner: can leak on crash
```

**Rust approach: Per-driver owned File handles**

```
  PwmChannel
  ┌──────────────────────────┐
  │ polarity_fd: Option<File>│  Opened on first write, closed on Drop
  │ period_fd:   Option<File>│  Opened on first write, closed on Drop
  │ duty_fd:     Option<File>│  Opened on first write, closed on Drop
  │ enable_fd:   Option<File>│  Opened on first write, closed on Drop
  └──────────────────────────┘

  CachedSysfsReader
  ┌──────────────────────────┐
  │ file: File               │  Opened on construction, closed on Drop
  └──────────────────────────┘

Benefits:
- O(1) access (direct field)
- No fixed capacity limit
- Ownership clear: File dropped when driver dropped
- Thread-safety enforced by Rust's type system (Send/Sync)
```

### 8.4 Memory Safety Issues Eliminated

1. **FD cache use-after-close:** In C, if `io_clear_cache` closes an fd but another thread is currently using it (having retrieved it from the cache), the fd is closed under the other thread's feet. Even worse, the OS may reassign that fd number to a new open call, causing the original thread to read/write to an unrelated file. Rust's `File` ownership prevents this: you cannot close a `File` while a reference exists.

2. **`strncpy` path truncation:** The C cache uses `strncpy(g_fdCache[i].sDevice, sDevice, IO_MAXPATH)` to copy the path. If the path is exactly `IO_MAXPATH` (128) characters, `strncpy` does not null-terminate. The code adds `g_fdCache[i].sDevice[IO_MAXPATH] = '\0'` explicitly, but this is a class of bug that is easy to get wrong. Rust's `String` and `HashMap<String, File>` have no length limit.

3. **io_read control character stripping:** The C `io_read` replaces bytes < 32 with null bytes in-place: `for(n=0;n<nSize;n++) if(sBuffer[n]<' ') sBuffer[n]=0;`. This creates a C string terminated at the first control character, but the bytes after remain in the buffer. If `strlen(sBuffer)` is later replaced with `nSize` (e.g., during refactoring), the control characters reappear. Rust's `trim()` returns a new string view without modifying the buffer.

4. **Buffer size mismatch:** `IO_MAXBUFFER` is defined as 32. The `io_read` function reads up to 32 bytes. If a sysfs file contains more than 32 bytes, the read is truncated with no indication to the caller. Rust's `read_to_string` reads the entire file.

5. **Missing `io_cache_init` call:** The C code auto-initializes on first use via `if(!g_fdCacheInit) io_cache_init()`. But this check is not thread-safe -- two threads could both see `g_fdCacheInit == 0` and both call `io_cache_init`, causing a double-init. Rust's approach of initializing in constructors avoids the problem.

6. **io_open return value confusion:** `io_open` returns -1 on failure or a non-negative fd on success. But file descriptors can legally be 0 (if stdin is closed). The C code treats any `hDevice < 0` as failure, which is correct for fds, but the convention of -1-as-error is a C idiom that Rust's `Result<File>` eliminates.

### 8.5 Performance Considerations

The Rust `HashMap` lookup is O(1) amortized vs the C linear-search which is O(N) where N = number of cached entries (up to 64). For 64 entries, the C search costs ~64 `strcmp` calls in the worst case. The HashMap lookup costs ~1 hash + ~1 comparison. At 100ms poll intervals, this difference is negligible, but it is a design improvement.

The owned-File approach has the same syscall cost as the C FD cache: `lseek(0)` + `read`/`write` per operation. The only difference is that Rust may allocate a small `String` for the return value, adding ~50ns. For sysfs files returning 1-4 byte values, this is invisible.

---

## 9. Consolidated Migration Summary

### 9.1 Crate Dependency Table

| Driver | C File | Rust Crate | Version | Crate Type |
|--------|--------|-----------|---------|------------|
| ADC | `anio.c` | `std::fs` (no crate) | N/A | stdlib |
| GPIO | `gpio.c` | `gpio-cdev` | 0.6 | chardev ioctl |
| I2C | `i2cio.c` | `i2cdev` | 0.6 | ioctl wrapper |
| I2C async | `i2c_worker.c` | `tokio` | 1.x | async runtime |
| PWM | `pwmio.c` | `std::fs` (no crate) | N/A | stdlib |
| UART | `uartio.c` | `serialport` | 4.x | termios wrapper |
| UART async | `uart_async.c` | `tokio-serial` | 5.x | async serial |
| I/O helpers | `io.c` | `std::fs` (no crate) | N/A | stdlib |

**Cargo.toml fragment:**

```toml
[dependencies]
gpio-cdev = "0.6"
i2cdev = "0.6"
serialport = "4"
tokio = { version = "1", features = ["rt-multi-thread", "sync", "time", "io-util", "macros"] }
tokio-serial = "5"
tracing = "0.1"       # Replaces ENGINE_LOG_* macros
```

### 9.2 Lines of Code Estimate

| Driver | C Lines | Rust Lines (est.) | Reduction |
|--------|---------|-------------------|-----------|
| `anio.c` | 39 | 15 | 62% |
| `gpio.c` | 146 | 80 | 45% |
| `i2cio.c` | 473 | 200 | 58% |
| `i2c_worker.c` | 617 | 150 | 76% |
| `pwmio.c` | 289 | 180 | 38% |
| `uartio.c` | 341 | 120 | 65% |
| `uart_async.c` | 423 | 100 | 76% |
| `io.c` | 432 | 120 | 72% |
| **Total** | **2,760** | **~965** | **~65%** |

The largest reductions are in the async modules, where tokio replaces hundreds of lines of pthread/epoll/eventfd boilerplate. The smallest reduction is PWM, where sysfs access is inherently verbose in both languages.

### 9.3 Memory Safety Bug Classes Eliminated

| Bug Class | Affected C Files | How Rust Prevents It |
|-----------|-----------------|---------------------|
| File descriptor leak | All (io_open/io_close pairs) | `Drop` trait on `File` auto-closes |
| Buffer overflow in path formatting | All (`snprintf` into fixed buffers) | `format!` returns heap `String` |
| Use-after-close on cached FD | `io.c`, `i2c_worker.c` | Ownership: cannot use `File` after move |
| Enum array out-of-bounds | `gpio.c` (string lookup tables) | Exhaustive `match` on Rust enums |
| Null pointer dereference on output params | All `*value` output pointers | Return `Result<T>`, no output pointers |
| Data race on global state | `i2c_worker.c`, `uart_async.c` | `Send`/`Sync` traits, no global mutables |
| Missed mutex unlock on error path | `i2c_worker.c` (5+ unlock points) | `MutexGuard` auto-unlocks on drop |
| Parse error silently returning 0 | `anio.c`, `uartio.c` (`strtod`) | `parse()` returns `Result` |
| Signed/unsigned confusion | `i2cio.c` (16-bit sensor values) | Explicit `i16`/`u16` types |
| `volatile` insufficient for thread sync | `uart_async.c` | `AtomicBool` or structured cancellation |
| Stack buffer truncation | `pwmio.c` (`sprintf` into `char[256]`) | `PathBuf` grows dynamically |
| Callback with dangling user_data | `i2c_worker.c`, `uart_async.c` | `oneshot::Sender` / `mpsc::Sender` |

### 9.4 Migration Order Recommendation

Based on dependency analysis and risk:

**Phase 1: Foundation (Week 1)**
1. `io.c` -> `sysfs.rs` -- The foundation all drivers use. Simplest to migrate, validates the basic approach.
2. `anio.c` -> `adc.rs` -- Simplest driver (39 lines), good proof-of-concept for sysfs reading.

**Phase 2: Simple Drivers (Week 2)**
3. `gpio.c` -> `gpio.rs` -- Straightforward migration, also upgrades from deprecated sysfs to chardev API.
4. `pwmio.c` -> `pwm.rs` -- Moderate complexity, tests the cached-write pattern.

**Phase 3: Complex Drivers (Week 3)**
5. `uartio.c` -> `uart.rs` -- Moderate complexity, introduces `serialport` crate.
6. `i2cio.c` -> `i2c.rs` -- Complex protocol handling (SDP510/SDP810), introduces `i2cdev` crate.

**Phase 4: Async Layer (Week 4)**
7. `uart_async.c` -> `uart_async.rs` -- Introduces tokio, event-driven design.
8. `i2c_worker.c` -> `i2c_worker.rs` -- Most complex, replaces pthread pool with tokio tasks.

Each phase can be tested independently by keeping the other drivers in C and linking via FFI. The `io.c` replacement should come first because all other drivers depend on it.

### 9.5 Testing Strategy

**Unit tests (no hardware required):**
- Mock sysfs paths in `/tmp/test_sysfs/` with known values
- Test CRC-8 computation against known vectors from Sensirion datasheets
- Test path formatting and parsing

**Integration tests (BeagleBone required):**
- Read ADC channel and verify value in expected range (0-4095)
- Export/read/write/unexport GPIO pins
- Read SDP810 sensor and verify CRC passes
- Configure PWM and verify duty cycle output with oscilloscope
- Send/receive data on loopback UART

**Property-based tests (no hardware):**
- Fuzz sensor response parsing with random byte sequences
- Verify that all `io::Error` paths are properly propagated (no unwrap panics)
- Test FD cache handles concurrent access (via tokio test runtime)

**Regression tests:**
- Compare Rust output against C output for identical sysfs inputs
- Run both engines simultaneously on the same BeagleBone, comparing readings
