//! Minimal systemd sd_notify support.
//!
//! Sends status messages to $NOTIFY_SOCKET when present (Linux + systemd).
//! No-op on Windows or when the socket is not set.

/// Send a message to the systemd notify socket.
#[cfg(unix)]
pub fn notify(msg: &str) -> bool {
    let path = match std::env::var_os("NOTIFY_SOCKET") {
        Some(p) => p,
        None => return false,
    };
    let sock = match std::os::unix::net::UnixDatagram::unbound() {
        Ok(s) => s,
        Err(_) => return false,
    };
    sock.send_to(msg.as_bytes(), &path).is_ok()
}

#[cfg(not(unix))]
pub fn notify(_msg: &str) -> bool {
    false
}

/// Notify systemd that the service is ready.
pub fn ready() -> bool {
    notify("READY=1")
}

/// Send watchdog keep-alive to systemd.
pub fn watchdog() -> bool {
    notify("WATCHDOG=1")
}
