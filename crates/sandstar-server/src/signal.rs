//! Platform-agnostic signal handling.
//!
//! On Unix, SIGHUP triggers config reload.
//! On Windows, reload is IPC-only (no SIGHUP equivalent).

/// Platform-agnostic SIGHUP listener.
///
/// On Unix, yields each time SIGHUP is received.
/// On Windows, the future never resolves.
pub struct HupSignal {
    #[cfg(unix)]
    inner: tokio::signal::unix::Signal,
}

impl HupSignal {
    /// Create a new SIGHUP listener.
    pub fn new() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            let inner = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
            Ok(Self { inner })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    /// Wait for the next SIGHUP. On Windows, this future never resolves.
    pub async fn recv(&mut self) {
        #[cfg(unix)]
        {
            self.inner.recv().await;
        }
        #[cfg(not(unix))]
        {
            // Park forever — reload is IPC-only on Windows
            std::future::pending::<()>().await;
        }
    }
}
