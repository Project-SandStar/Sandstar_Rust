//! Dual watchdog support for production BeagleBone deployment.
//!
//! Manages two independent watchdog mechanisms (matching the C engine):
//!
//! 1. **Software watchdog** (`/dev/watchdog`) — Linux kernel watchdog device.
//!    Write `"k"` to kick, `"V"` (magic close) to disable on shutdown.
//!
//! 2. **GPIO60 / TPL5010** (`/sys/class/gpio/gpio60/value`) — External hardware
//!    watchdog on P9-12. Toggle 0↔1 to pulse the DONE signal. If toggling
//!    stops, the TPL5010 resets the BeagleBone.
//!
//! Both are best-effort: if a device is unavailable (Windows, dev machine),
//! the corresponding fd is `None` and `kick()` is a no-op.

use std::fs::{File, OpenOptions};
use std::io::Write;
use tracing::{debug, info, warn};

/// Dual watchdog manager (software + GPIO60 hardware).
pub struct Watchdog {
    /// Open fd to `/dev/watchdog` (Linux kernel watchdog).
    dev_watchdog: Option<File>,
    /// Open fd to `/sys/class/gpio/gpio60/value` (TPL5010 DONE pin).
    gpio60_value: Option<File>,
    /// Current GPIO60 toggle state (false=0, true=1).
    gpio60_state: bool,
}

impl Default for Watchdog {
    fn default() -> Self {
        Self::new()
    }
}

impl Watchdog {
    /// Create a disabled watchdog (for --read-only validation mode).
    pub fn disabled() -> Self {
        info!("watchdog: disabled (read-only mode)");
        Self {
            dev_watchdog: None,
            gpio60_value: None,
            gpio60_state: false,
        }
    }

    /// Open watchdog devices. Non-fatal if unavailable.
    pub fn new() -> Self {
        let dev_watchdog = OpenOptions::new()
            .write(true)
            .open("/dev/watchdog")
            .map_err(|e| {
                debug!(err = %e, "watchdog: /dev/watchdog not available (expected on non-Linux)");
            })
            .ok();

        if dev_watchdog.is_some() {
            info!("watchdog: /dev/watchdog opened");
        }

        let gpio60_value = OpenOptions::new()
            .write(true)
            .open("/sys/class/gpio/gpio60/value")
            .map_err(|e| {
                debug!(err = %e, "watchdog: GPIO60 not available (expected on non-BeagleBone)");
            })
            .ok();

        if gpio60_value.is_some() {
            info!("watchdog: GPIO60 (TPL5010) opened");
        }

        if dev_watchdog.is_none() && gpio60_value.is_none() {
            info!("watchdog: no watchdog devices available — watchdog disabled");
        }

        Self {
            dev_watchdog,
            gpio60_value,
            gpio60_state: false,
        }
    }

    /// Kick both watchdogs. Called every ~500ms from the main event loop.
    pub fn kick(&mut self) {
        // Software watchdog: any write kicks it
        if let Some(ref mut fd) = self.dev_watchdog {
            if let Err(e) = fd.write_all(b"k") {
                warn!(err = %e, "watchdog: failed to kick /dev/watchdog");
                // Close broken fd to avoid repeated warnings
                self.dev_watchdog = None;
            }
        }

        // GPIO60 hardware watchdog: toggle state
        if let Some(ref mut fd) = self.gpio60_value {
            self.gpio60_state = !self.gpio60_state;
            let val = if self.gpio60_state { b"1" as &[u8] } else { b"0" };
            if let Err(e) = fd.write_all(val) {
                warn!(err = %e, "watchdog: failed to toggle GPIO60");
                self.gpio60_value = None;
            }
        }
    }

    /// Gracefully close watchdog devices.
    /// Writes magic close character `"V"` to `/dev/watchdog` to prevent
    /// the kernel from rebooting after the process exits.
    pub fn close(&mut self) {
        if let Some(ref mut fd) = self.dev_watchdog {
            // Magic close: "V" disables watchdog on close
            let _ = fd.write_all(b"V");
            info!("watchdog: /dev/watchdog closed (magic close)");
        }
        self.dev_watchdog = None;
        self.gpio60_value = None;
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_new_on_non_linux() {
        // On Windows / dev machine, both fds should be None
        let wd = Watchdog::new();
        assert!(wd.dev_watchdog.is_none() || cfg!(target_os = "linux"));
        assert!(wd.gpio60_value.is_none() || cfg!(target_os = "linux"));
    }

    #[test]
    fn watchdog_kick_noop_when_disabled() {
        let mut wd = Watchdog {
            dev_watchdog: None,
            gpio60_value: None,
            gpio60_state: false,
        };
        // Should not panic
        wd.kick();
        wd.kick();
        assert!(!wd.gpio60_state); // No toggle when fd is None
    }

    #[test]
    fn watchdog_close_idempotent() {
        let mut wd = Watchdog::new();
        wd.close();
        wd.close(); // Should not panic
    }
}
