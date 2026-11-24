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
//!
//! On the device GPIO60 is exported by `initialize.sh` (ExecStartPre), but
//! on some starts it is missing when the engine opens it (R-42). On Linux a
//! missing GPIO60 is therefore retried from `kick()` for a few minutes.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

const DEV_WATCHDOG_PATH: &str = "/dev/watchdog";
const GPIO60_VALUE_PATH: &str = "/sys/class/gpio/gpio60/value";

/// Kicks between two GPIO60 open retries (kick period 500 ms: ~10 s).
const GPIO60_RETRY_EVERY_KICKS: u32 = 20;
/// GPIO60 open retries after a failed start (~5 min in total).
const GPIO60_MAX_RETRIES: u32 = 30;

/// Open a watchdog file for writing; the error text includes the path.
fn open_watchdog_device(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn log_unavailable(path: &str, err: &str) {
    if cfg!(target_os = "linux") {
        info!(path, err, "watchdog: device not available");
    } else {
        debug!(
            path,
            err, "watchdog: device not available (expected off-device)"
        );
    }
}

/// Dual watchdog manager (software + GPIO60 hardware).
pub struct Watchdog {
    /// Open fd to `/dev/watchdog` (Linux kernel watchdog).
    dev_watchdog: Option<File>,
    /// Open fd to `/sys/class/gpio/gpio60/value` (TPL5010 DONE pin).
    gpio60_value: Option<File>,
    /// Current GPIO60 toggle state (false=0, true=1).
    gpio60_state: bool,
    /// GPIO60 value file, reopened from `kick()` while retries remain.
    gpio60_path: PathBuf,
    /// Open retries left while `gpio60_value` is `None`; 0 = no retry.
    gpio60_retries_left: u32,
    /// Kicks until the next retry.
    gpio60_retry_countdown: u32,
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
            gpio60_path: PathBuf::from(GPIO60_VALUE_PATH),
            gpio60_retries_left: 0,
            gpio60_retry_countdown: 0,
        }
    }

    /// Open watchdog devices. Non-fatal if unavailable.
    ///
    /// On Linux (the device) a device that cannot be opened is logged with
    /// its reason at `info`, and `warn` when neither opens: the process
    /// is then protected only by systemd `WatchdogSec` (R-42). Elsewhere
    /// (dev machines) the devices are expected to be missing: `debug` only.
    pub fn new() -> Self {
        let dev_watchdog = open_watchdog_device(Path::new(DEV_WATCHDOG_PATH));
        let gpio60_value = open_watchdog_device(Path::new(GPIO60_VALUE_PATH));

        match &dev_watchdog {
            Ok(_) => info!("watchdog: /dev/watchdog opened"),
            Err(e) => log_unavailable(DEV_WATCHDOG_PATH, e),
        }
        match &gpio60_value {
            Ok(_) => info!("watchdog: GPIO60 (TPL5010) opened"),
            Err(e) => log_unavailable(GPIO60_VALUE_PATH, e),
        }

        if dev_watchdog.is_err() && gpio60_value.is_err() {
            if cfg!(target_os = "linux") {
                warn!(
                    "watchdog: no watchdog device could be opened; kernel and hardware watchdog disabled, only systemd WatchdogSec applies until GPIO60 opens on retry"
                );
            } else {
                info!("watchdog: no watchdog devices available — watchdog disabled");
            }
        }

        // Retrying only makes sense on the device.
        let gpio60_retries_left = if gpio60_value.is_err() && cfg!(target_os = "linux") {
            GPIO60_MAX_RETRIES
        } else {
            0
        };

        Self {
            dev_watchdog: dev_watchdog.ok(),
            gpio60_value: gpio60_value.ok(),
            gpio60_state: false,
            gpio60_path: PathBuf::from(GPIO60_VALUE_PATH),
            gpio60_retries_left,
            gpio60_retry_countdown: GPIO60_RETRY_EVERY_KICKS,
        }
    }

    /// Try to open GPIO60 again, once every `GPIO60_RETRY_EVERY_KICKS`
    /// kicks, while retries remain. Only called while it is not open.
    fn retry_gpio60(&mut self) {
        if self.gpio60_retries_left == 0 {
            return;
        }
        self.gpio60_retry_countdown = self.gpio60_retry_countdown.saturating_sub(1);
        if self.gpio60_retry_countdown > 0 {
            return;
        }
        self.gpio60_retry_countdown = GPIO60_RETRY_EVERY_KICKS;
        self.gpio60_retries_left -= 1;
        let attempt = GPIO60_MAX_RETRIES.saturating_sub(self.gpio60_retries_left);
        match open_watchdog_device(&self.gpio60_path) {
            Ok(fd) => {
                info!(attempt, "watchdog: GPIO60 (TPL5010) opened on retry");
                self.gpio60_value = Some(fd);
                self.gpio60_retries_left = 0;
            }
            Err(e) if self.gpio60_retries_left == 0 => {
                warn!(
                    attempt,
                    err = %e,
                    "watchdog: GPIO60 still unavailable, giving up; hardware watchdog disabled"
                );
            }
            Err(e) => debug!(attempt, err = %e, "watchdog: GPIO60 retry failed"),
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
        if self.gpio60_value.is_none() {
            self.retry_gpio60();
        }
        if let Some(ref mut fd) = self.gpio60_value {
            self.gpio60_state = !self.gpio60_state;
            let val = if self.gpio60_state {
                b"1" as &[u8]
            } else {
                b"0"
            };
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
            match fd.write_all(b"V") {
                Ok(()) => info!("watchdog: /dev/watchdog closed (magic close)"),
                // The kernel watchdog stays armed and resets the board.
                Err(e) => {
                    warn!(err = %e, "watchdog: magic close failed, /dev/watchdog stays armed")
                }
            }
        }
        self.dev_watchdog = None;
        self.gpio60_value = None;
        // Never reopen after close.
        self.gpio60_retries_left = 0;
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
    fn open_error_names_the_path() {
        let path = Path::new("/nonexistent/sandstar-watchdog");
        let err = open_watchdog_device(path).unwrap_err();
        assert!(err.starts_with(&format!("{}: ", path.display())), "{err}");
    }

    /// Watchdog with only GPIO60 at `path`, closed, with `retries` left.
    fn gpio_only(path: &Path, retries: u32) -> Watchdog {
        Watchdog {
            dev_watchdog: None,
            gpio60_value: None,
            gpio60_state: false,
            gpio60_path: path.to_path_buf(),
            gpio60_retries_left: retries,
            gpio60_retry_countdown: GPIO60_RETRY_EVERY_KICKS,
        }
    }

    fn kick_n(wd: &mut Watchdog, n: u32) {
        for _ in 0..n {
            wd.kick();
        }
    }

    #[test]
    fn gpio60_opens_on_retry_and_toggles() {
        let dir = tempfile::tempdir().unwrap();
        let value = dir.path().join("value");
        let mut wd = gpio_only(&value, 3);

        // File missing: first retry (after 20 kicks) fails.
        kick_n(&mut wd, GPIO60_RETRY_EVERY_KICKS);
        assert!(wd.gpio60_value.is_none());
        assert_eq!(wd.gpio60_retries_left, 2);

        // File appears: next retry opens it and that kick toggles it.
        std::fs::write(&value, b"").unwrap();
        kick_n(&mut wd, GPIO60_RETRY_EVERY_KICKS - 1);
        assert!(
            wd.gpio60_value.is_none(),
            "no retry before the countdown ends"
        );
        wd.kick();
        assert!(wd.gpio60_value.is_some());
        assert_eq!(wd.gpio60_retries_left, 0);
        assert!(wd.gpio60_state);
        wd.kick();
        assert!(!wd.gpio60_state);
        assert_eq!(std::fs::read(&value).unwrap(), b"10");
    }

    #[test]
    fn gpio60_retries_stop_when_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let value = dir.path().join("value");
        let mut wd = gpio_only(&value, 2);
        kick_n(&mut wd, 2 * GPIO60_RETRY_EVERY_KICKS);
        assert_eq!(wd.gpio60_retries_left, 0);

        // Appearing later is ignored: no retries left.
        std::fs::write(&value, b"").unwrap();
        kick_n(&mut wd, 3 * GPIO60_RETRY_EVERY_KICKS);
        assert!(wd.gpio60_value.is_none());
    }

    #[test]
    fn gpio60_no_retry_after_close_or_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let value = dir.path().join("value");
        std::fs::write(&value, b"").unwrap();

        let mut closed = gpio_only(&value, 5);
        closed.close();
        kick_n(&mut closed, 2 * GPIO60_RETRY_EVERY_KICKS);
        assert!(closed.gpio60_value.is_none());

        let mut disabled = Watchdog::disabled();
        disabled.gpio60_path = value.clone();
        kick_n(&mut disabled, 2 * GPIO60_RETRY_EVERY_KICKS);
        assert!(disabled.gpio60_value.is_none());
    }

    #[test]
    fn watchdog_kick_noop_when_disabled() {
        let mut wd = Watchdog {
            dev_watchdog: None,
            gpio60_value: None,
            gpio60_state: false,
            gpio60_path: PathBuf::from(GPIO60_VALUE_PATH),
            gpio60_retries_left: 0,
            gpio60_retry_countdown: 0,
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
