//! Linux UART driver.
//!
//! Manages `/dev/ttyON` serial ports with termios configuration.
//!
//! The driver opens ports on demand, caches file descriptors, and reads
//! ASCII-formatted sensor values terminated by `\n` or `\r`.
//!
//! # Platform support
//!
//! Actual termios and `/dev/tty*` access only works on Linux.  On other
//! platforms the low-level operations return `HalError::BusError` so that the
//! crate still compiles for development on Windows / macOS.

use std::collections::HashMap;
use std::sync::Mutex;

#[cfg(target_os = "linux")]
use std::os::unix::io::RawFd;
#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::Duration;

use sandstar_hal::HalError;
use tracing::debug;
#[cfg(target_os = "linux")]
use tracing::info;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of read retries (each retry waits ~100 ms for data).
#[cfg(target_os = "linux")]
const READ_RETRIES: u32 = 10;

/// Delay between read retries in milliseconds.
#[cfg(target_os = "linux")]
const READ_RETRY_DELAY_MS: u64 = 100;

/// Maximum receive buffer size.
#[cfg(target_os = "linux")]
const RX_BUF_SIZE: usize = 64;

/// Maximum number of UART ports we support.
const MAX_PORTS: u32 = 8;

// ---------------------------------------------------------------------------
// UART configuration
// ---------------------------------------------------------------------------

/// UART port configuration (mirrors the C `UARTIO_CONFIG` struct).
#[derive(Debug, Clone, Copy)]
pub struct UartConfig {
    /// Baud rate (e.g. 9600, 115200).
    pub baud: u32,
    /// Data bits: 7 or 8.
    pub data_bits: u8,
    /// Parity: `'N'` (none), `'E'` (even), `'O'` (odd).
    pub parity: char,
    /// Stop bits: 1 or 2.
    pub stop_bits: u8,
}

impl Default for UartConfig {
    fn default() -> Self {
        Self {
            baud: 9600,
            data_bits: 8,
            parity: 'N',
            stop_bits: 1,
        }
    }
}

// ---------------------------------------------------------------------------
// Port state (cached file descriptors)
// ---------------------------------------------------------------------------

/// Per-port state: cached file descriptor.
struct PortState {
    #[cfg(target_os = "linux")]
    fd: RawFd,
    #[cfg(not(target_os = "linux"))]
    _fd_placeholder: i32,
}

// ---------------------------------------------------------------------------
// LinuxUart public API
// ---------------------------------------------------------------------------

/// Linux UART driver.
///
/// Manages one or more `/dev/ttyO{N}` (or other prefix) serial ports.
/// File descriptors are cached after the first open.
///
/// The `ports` map is wrapped in a `Mutex` so that `&self` methods can
/// lazily cache file descriptors without requiring `&mut self`.  This
/// replaces the previous raw-pointer interior-mutability pattern with
/// sound, thread-safe code.
pub struct LinuxUart {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    ports: Mutex<HashMap<u32, PortState>>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    device_prefix: String,
}

impl LinuxUart {
    /// Create a new driver with the given device path prefix.
    ///
    /// # Arguments
    /// * `device_prefix` - Path prefix such as `"/dev/ttyO"`.  The device
    ///   number is appended to form the full path (e.g. `"/dev/ttyO1"`).
    pub fn new(device_prefix: &str) -> Self {
        Self {
            ports: Mutex::new(HashMap::new()),
            device_prefix: device_prefix.to_string(),
        }
    }

    /// Read a measurement from a UART sensor.
    ///
    /// Opens the port (if not already cached), reads an ASCII line, and parses
    /// the numeric value.  The `label` parameter is reserved for future
    /// protocol-specific parsing (e.g. Modbus RTU) but currently unused.
    pub fn read_measurement(&self, device: u32, label: &str) -> Result<f64, HalError> {
        debug!(device, label, "uart: read_measurement");

        if device >= MAX_PORTS {
            return Err(HalError::BusError(
                device,
                format!("device {} exceeds max ports {}", device, MAX_PORTS),
            ));
        }

        self.read_measurement_impl(device, label)
    }
}

// ---------------------------------------------------------------------------
// ASCII response parsing (platform-independent)
// ---------------------------------------------------------------------------

/// Parse an ASCII numeric string from a receive buffer.
///
/// The parser:
/// 1. Looks for the first ASCII digit, sign character, or decimal point.
/// 2. Parses the floating-point value via the standard library.
/// 3. Clamps the result to `[0.0, 65535.0]` to match the C `UARTIO_VALUE`
///    (unsigned short) range.
///
/// Returns `None` if no valid number is found.
pub fn parse_ascii_value(buf: &[u8]) -> Option<f64> {
    // Convert to string, stopping at first \0, \r, or \n.
    let end = buf
        .iter()
        .position(|&b| b == 0 || b == b'\r' || b == b'\n')
        .unwrap_or(buf.len());
    let s = std::str::from_utf8(&buf[..end]).ok()?;
    let s = s.trim();

    if s.is_empty() {
        return None;
    }

    let val: f64 = s.parse().ok()?;

    // Clamp to unsigned-short range like the C code does.
    let clamped = val.clamp(0.0, 65535.0);
    Some(clamped)
}

// ---------------------------------------------------------------------------
// Linux-specific implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
impl LinuxUart {
    /// Lock the ports mutex, returning a HalError on poison.
    fn lock_ports(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<u32, PortState>>, HalError> {
        self.ports
            .lock()
            .map_err(|e| HalError::BusError(0, format!("ports mutex poisoned: {e}")))
    }

    /// Ensure port `device` has a cached FD, opening and configuring it if needed.
    fn ensure_port_fd(&self, device: u32) -> Result<RawFd, HalError> {
        let mut ports = self.lock_ports()?;
        if let Some(port) = ports.get(&device) {
            return Ok(port.fd);
        }

        let path = format!("{}{}", self.device_prefix, device);
        let c_path = std::ffi::CString::new(path.as_str())
            .map_err(|e| HalError::BusError(device, e.to_string()))?;

        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(HalError::BusError(
                device,
                format!("failed to open {}: errno {}", path, errno()),
            ));
        }

        // Configure termios: raw mode, 9600 8N1, no flow control.
        let config = UartConfig::default();
        if let Err(e) = configure_termios(fd, &config) {
            unsafe { libc::close(fd) };
            return Err(HalError::BusError(device, format!("termios config failed: {e}")));
        }

        // Flush pending data.
        unsafe { libc::tcflush(fd, libc::TCIOFLUSH) };

        info!(device, fd, path = path.as_str(), "uart: opened port");

        ports.insert(device, PortState { fd });

        Ok(fd)
    }

    fn read_measurement_impl(&self, device: u32, _label: &str) -> Result<f64, HalError> {
        let fd = self.ensure_port_fd(device)?;

        let mut buf = [0u8; RX_BUF_SIZE];
        let mut total: usize = 0;

        for retry in 0..READ_RETRIES {
            let remaining = RX_BUF_SIZE.saturating_sub(total + 1); // leave room for NUL
            if remaining == 0 {
                break;
            }

            let n = unsafe {
                libc::read(
                    fd,
                    buf[total..].as_mut_ptr() as *mut libc::c_void,
                    remaining,
                )
            };

            if n > 0 {
                total += n as usize;
                // Check for line ending.
                if buf[..total].contains(&b'\n') || buf[..total].contains(&b'\r') {
                    break;
                }
            } else if n == 0 || (n < 0 && is_eagain()) {
                // No data yet, wait and retry.
                if retry + 1 < READ_RETRIES {
                    thread::sleep(Duration::from_millis(READ_RETRY_DELAY_MS));
                }
            } else {
                return Err(HalError::BusError(
                    device,
                    format!("read error: errno {}", errno()),
                ));
            }
        }

        if total == 0 {
            return Err(HalError::Timeout {
                device,
                address: 0,
            });
        }

        debug!(device, total, "uart: received bytes");

        parse_ascii_value(&buf[..total]).ok_or_else(|| HalError::DeviceError {
            device,
            address: 0,
            message: format!(
                "failed to parse numeric value from {} bytes",
                total
            ),
        })
    }
}

/// Configure a serial port file descriptor via termios.
#[cfg(target_os = "linux")]
fn configure_termios(fd: RawFd, config: &UartConfig) -> Result<(), String> {
    use std::mem::MaybeUninit;

    let mut tty = unsafe {
        let mut t = MaybeUninit::<libc::termios>::zeroed();
        if libc::tcgetattr(fd, t.as_mut_ptr()) != 0 {
            return Err(format!("tcgetattr failed: errno {}", errno()));
        }
        t.assume_init()
    };

    // Baud rate.
    let speed = baud_to_speed(config.baud);
    unsafe {
        libc::cfsetispeed(&mut tty, speed);
        libc::cfsetospeed(&mut tty, speed);
    }

    // Control modes: enable receiver, ignore modem control lines.
    tty.c_cflag |= libc::CLOCAL | libc::CREAD;

    // Data bits.
    tty.c_cflag &= !libc::CSIZE;
    tty.c_cflag |= if config.data_bits == 7 {
        libc::CS7
    } else {
        libc::CS8
    };

    // Parity.
    match config.parity {
        'N' | 'n' => {
            tty.c_cflag &= !libc::PARENB;
        }
        'E' | 'e' => {
            tty.c_cflag |= libc::PARENB;
            tty.c_cflag &= !libc::PARODD;
        }
        'O' | 'o' => {
            tty.c_cflag |= libc::PARENB;
            tty.c_cflag |= libc::PARODD;
        }
        _ => {
            tty.c_cflag &= !libc::PARENB;
        }
    }

    // Stop bits.
    if config.stop_bits == 2 {
        tty.c_cflag |= libc::CSTOPB;
    } else {
        tty.c_cflag &= !libc::CSTOPB;
    }

    // No hardware flow control.
    #[cfg(target_os = "linux")]
    {
        tty.c_cflag &= !libc::CRTSCTS;
    }

    // Input modes: raw (disable all processing).
    tty.c_iflag &= !(libc::IGNBRK
        | libc::BRKINT
        | libc::PARMRK
        | libc::ISTRIP
        | libc::INLCR
        | libc::IGNCR
        | libc::ICRNL
        | libc::IXON);

    // Output modes: raw.
    tty.c_oflag &= !libc::OPOST;

    // Local modes: raw.
    tty.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);

    // Read settings: VMIN=0, VTIME=1 (100ms timeout in 1/10 sec units).
    tty.c_cc[libc::VMIN] = 0;
    tty.c_cc[libc::VTIME] = 1;

    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &tty) } != 0 {
        return Err(format!("tcsetattr failed: errno {}", errno()));
    }

    Ok(())
}

/// Convert a numeric baud rate to the corresponding `libc` speed constant.
#[cfg(target_os = "linux")]
fn baud_to_speed(baud: u32) -> libc::speed_t {
    match baud {
        300 => libc::B300,
        1200 => libc::B1200,
        2400 => libc::B2400,
        4800 => libc::B4800,
        9600 => libc::B9600,
        19200 => libc::B19200,
        38400 => libc::B38400,
        57600 => libc::B57600,
        115200 => libc::B115200,
        230400 => libc::B230400,
        _ => libc::B9600,
    }
}

/// Helper to get the current errno value.
#[cfg(target_os = "linux")]
fn errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

/// Check if the last error was EAGAIN / EWOULDBLOCK.
#[cfg(target_os = "linux")]
fn is_eagain() -> bool {
    let e = errno();
    e == libc::EAGAIN || e == libc::EWOULDBLOCK
}

// ---------------------------------------------------------------------------
// Non-Linux stubs (Windows / macOS development builds)
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "linux"))]
impl LinuxUart {
    fn read_measurement_impl(&self, device: u32, _label: &str) -> Result<f64, HalError> {
        Err(HalError::BusError(
            device,
            "UART not available on this platform".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- UartConfig defaults --------------------------------------------------

    #[test]
    fn default_config() {
        let cfg = UartConfig::default();
        assert_eq!(cfg.baud, 9600);
        assert_eq!(cfg.data_bits, 8);
        assert_eq!(cfg.parity, 'N');
        assert_eq!(cfg.stop_bits, 1);
    }

    // -- ASCII parsing --------------------------------------------------------

    #[test]
    fn parse_simple_integer() {
        assert_eq!(parse_ascii_value(b"42\r\n"), Some(42.0));
    }

    #[test]
    fn parse_integer_newline() {
        assert_eq!(parse_ascii_value(b"100\n"), Some(100.0));
    }

    #[test]
    fn parse_float() {
        assert_eq!(parse_ascii_value(b"3.14\r\n"), Some(3.14));
    }

    #[test]
    fn parse_with_leading_spaces() {
        assert_eq!(parse_ascii_value(b"  500\r\n"), Some(500.0));
    }

    #[test]
    fn parse_with_trailing_spaces() {
        // Trailing spaces before newline are part of the trimmed string.
        assert_eq!(parse_ascii_value(b"500  \r\n"), Some(500.0));
    }

    #[test]
    fn parse_zero() {
        assert_eq!(parse_ascii_value(b"0\n"), Some(0.0));
    }

    #[test]
    fn parse_clamp_negative_to_zero() {
        // Negative values are clamped to 0.
        assert_eq!(parse_ascii_value(b"-10\n"), Some(0.0));
    }

    #[test]
    fn parse_clamp_large_to_max() {
        // Values > 65535 are clamped.
        assert_eq!(parse_ascii_value(b"70000\n"), Some(65535.0));
    }

    #[test]
    fn parse_max_value() {
        assert_eq!(parse_ascii_value(b"65535\n"), Some(65535.0));
    }

    #[test]
    fn parse_empty_returns_none() {
        assert_eq!(parse_ascii_value(b""), None);
    }

    #[test]
    fn parse_only_newline_returns_none() {
        assert_eq!(parse_ascii_value(b"\n"), None);
    }

    #[test]
    fn parse_non_numeric_returns_none() {
        assert_eq!(parse_ascii_value(b"abc\n"), None);
    }

    #[test]
    fn parse_no_newline() {
        // Data without terminator should still parse (buffer might be full).
        assert_eq!(parse_ascii_value(b"1234"), Some(1234.0));
    }

    #[test]
    fn parse_with_null_terminator() {
        assert_eq!(parse_ascii_value(b"999\0garbage"), Some(999.0));
    }

    #[test]
    fn parse_scientific_notation() {
        // Standard library f64 parsing handles scientific notation.
        assert_eq!(parse_ascii_value(b"1.5e3\n"), Some(1500.0));
    }

    // -- Constructor ----------------------------------------------------------

    #[test]
    fn new_creates_empty_driver() {
        let uart = LinuxUart::new("/dev/ttyO");
        assert!(uart.ports.lock().unwrap().is_empty());
        assert_eq!(uart.device_prefix, "/dev/ttyO");
    }

    #[test]
    fn device_exceeds_max() {
        let uart = LinuxUart::new("/dev/ttyO");
        let result = uart.read_measurement(MAX_PORTS, "sensor");
        assert!(result.is_err());
    }

    // -- Non-linux stub tests -------------------------------------------------

    #[cfg(not(target_os = "linux"))]
    mod non_linux {
        use super::*;

        #[test]
        fn read_returns_bus_error() {
            let uart = LinuxUart::new("/dev/ttyO");
            let result = uart.read_measurement(1, "co2");
            assert!(result.is_err());
        }
    }

    // -- Integration tests (only on real hardware) ----------------------------

    #[cfg(all(target_os = "linux", feature = "integration-tests"))]
    mod integration {
        use super::*;

        #[test]
        fn open_nonexistent_port() {
            let uart = LinuxUart::new("/dev/ttyO");
            // Port 99 is unlikely to exist.
            let result = uart.read_measurement(7, "test");
            // Should fail to open the device.
            assert!(result.is_err());
        }
    }
}
