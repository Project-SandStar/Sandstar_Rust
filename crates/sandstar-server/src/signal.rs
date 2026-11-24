//! Platform-agnostic signal handling.
//!
//! On Unix, SIGHUP triggers config reload and SIGTERM (`systemctl stop`)
//! a graceful shutdown (R-13).
//! On Windows, reload is IPC-only (no SIGHUP equivalent) and shutdown is
//! Ctrl+C or IPC.

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
            if self.inner.recv().await.is_none() {
                // The signal driver is gone: never report a signal that
                // did not arrive.
                tracing::error!("signal listener closed");
                std::future::pending::<()>().await;
            }
        }
        #[cfg(not(unix))]
        {
            // Park forever — reload is IPC-only on Windows
            std::future::pending::<()>().await;
        }
    }
}

/// Platform-agnostic SIGTERM listener.
///
/// On Unix, resolves when SIGTERM is received (`systemctl stop`/`restart`).
/// On Windows, the future never resolves.
pub struct TermSignal {
    #[cfg(unix)]
    inner: tokio::signal::unix::Signal,
}

impl TermSignal {
    /// Create a new SIGTERM listener. Once created, SIGTERM no longer kills
    /// the process directly; the caller must shut down.
    pub fn new() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            let inner = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            Ok(Self { inner })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    /// Wait for SIGTERM. On Windows, this future never resolves.
    pub async fn recv(&mut self) {
        #[cfg(unix)]
        {
            if self.inner.recv().await.is_none() {
                // The signal driver is gone: never report a signal that
                // did not arrive.
                tracing::error!("signal listener closed");
                std::future::pending::<()>().await;
            }
        }
        #[cfg(not(unix))]
        {
            std::future::pending::<()>().await;
        }
    }
}
