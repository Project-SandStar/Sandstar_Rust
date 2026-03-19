//! Logging initialization with optional file output.
//!
//! Dual output: stderr (for systemd journal / interactive) + optional
//! daily-rotated file. Uses `tracing-appender` non-blocking writer
//! to avoid blocking the poll loop.

use std::path::Path;

use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Guard that must be held alive for non-blocking file writer to flush.
/// Drop this at the very end of main().
pub struct LogGuard {
    _guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

/// Initialize tracing with the given log level filter and optional file output.
///
/// Returns a guard that MUST be kept alive for the duration of the process.
/// Dropping the guard flushes any buffered log entries to the file.
///
/// # Log file permissions
///
/// Log files are created by `tracing_appender::rolling::daily`, which manages
/// file creation internally during daily rotation. Per-file permissions cannot
/// be set here. To restrict log file access, configure a restrictive umask
/// in the systemd service file:
///
/// ```ini
/// [Service]
/// UMask=0077
/// ```
///
/// This ensures all files created by the process (including rotated logs)
/// have owner-only access (0600 for files, 0700 for directories).
pub fn init(log_level: &str, log_file: Option<&Path>) -> LogGuard {
    let filter = EnvFilter::try_new(log_level)
        .unwrap_or_else(|_| EnvFilter::new("info"));

    match log_file {
        Some(path) => {
            let file_dir = path.parent().unwrap_or_else(|| Path::new("."));
            let file_name = path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("sandstar-engine.log");

            // Daily rotation: sandstar-engine.log.YYYY-MM-DD
            let file_appender = tracing_appender::rolling::daily(file_dir, file_name);
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

            tracing_subscriber::registry()
                .with(filter)
                .with(
                    fmt::layer()
                        .with_writer(std::io::stderr)
                        .with_ansi(true),
                )
                .with(
                    fmt::layer()
                        .with_writer(non_blocking)
                        .with_ansi(false), // no ANSI escapes in file output
                )
                .init();

            LogGuard {
                _guard: Some(guard),
            }
        }
        None => {
            // stderr only (suitable for systemd journal capture)
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .init();

            LogGuard { _guard: None }
        }
    }
}
