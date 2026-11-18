//! Logging initialization with optional file output.
//!
//! Dual output: stderr (for systemd journal / interactive) + optional
//! daily-rotated file. Uses `tracing-appender` non-blocking writer
//! to avoid blocking the poll loop. Old daily files are deleted so the log
//! directory cannot fill the disk (R-41).

use std::path::Path;

use tracing_appender::rolling::{InitError, RollingFileAppender, Rotation};

use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Guard that must be held alive for non-blocking file writer to flush.
/// Drop this at the very end of main().
pub struct LogGuard {
    _guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

/// Default number of daily log files kept (R-41): about 8-10 MB per day on
/// the device, so roughly 140 MB at most. Override with `--log-max-files` /
/// `SANDSTAR_LOG_MAX_FILES`; `0` keeps every file.
pub const DEFAULT_LOG_MAX_FILES: usize = 14;

/// Build the daily-rotated file appender (`<file_name>.YYYY-MM-DD`).
///
/// Keeps at most `max_files` files with this prefix (the current one
/// included); older ones are deleted at startup and at each daily rotation.
/// `0` disables deletion. Returns an error instead of panicking when the
/// directory cannot be used.
fn build_file_appender(
    dir: &Path,
    file_name: &str,
    max_files: usize,
) -> Result<RollingFileAppender, InitError> {
    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(file_name)
        .max_log_files(max_files)
        .build(dir)
}

/// Initialize tracing with the given log level filter and optional file output.
///
/// Returns a guard that MUST be kept alive for the duration of the process.
/// Dropping the guard flushes any buffered log entries to the file.
///
/// # Log file permissions
///
/// Log files are created by `tracing_appender`'s daily rolling appender, which manages
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
///
/// If the log file cannot be opened, logging continues on stderr only and
/// the reason is printed to stderr (the process does not abort).
pub fn init(log_level: &str, log_file: Option<&Path>, max_files: usize) -> LogGuard {
    let filter = EnvFilter::try_new(log_level).unwrap_or_else(|_| EnvFilter::new("info"));

    let file_appender = log_file.and_then(|path| {
        let file_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("sandstar-engine.log");
        match build_file_appender(file_dir, file_name, max_files) {
            Ok(appender) => Some(appender),
            Err(e) => {
                eprintln!(
                    "logging: cannot open log file {} ({e}); logging to stderr only",
                    path.display()
                );
                None
            }
        }
    });

    match file_appender {
        Some(file_appender) => {
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

            tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().with_writer(std::io::stderr).with_ansi(true))
                .with(
                    fmt::layer().with_writer(non_blocking).with_ansi(false), // no ANSI escapes in file output
                )
                .init();

            tracing::info!(
                max_files,
                "logging: daily log files, keeping the newest max_files (0 = all)"
            );

            LogGuard {
                _guard: Some(guard),
            }
        }
        None => {
            // stderr only (suitable for systemd journal capture)
            tracing_subscriber::fmt().with_env_filter(filter).init();

            LogGuard { _guard: None }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: &str = "sandstar-engine.log";

    fn prefixed_files(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(PREFIX))
            .count()
    }

    fn seed_old_logs(dir: &Path, n: usize) {
        for day in 1..=n {
            let name = format!("{PREFIX}.2026-01-{day:02}");
            std::fs::write(
                dir.join(name),
                b"old
",
            )
            .unwrap();
        }
    }

    #[test]
    fn prunes_to_max_files_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        seed_old_logs(dir.path(), 20);
        std::fs::write(dir.path().join("unrelated.txt"), b"keep").unwrap();

        let _appender = build_file_appender(dir.path(), PREFIX, 14).unwrap();

        // 13 old files + today's new file
        assert_eq!(prefixed_files(dir.path()), 14);
        assert!(dir.path().join("unrelated.txt").exists());
    }

    #[test]
    fn zero_keeps_all_files() {
        let dir = tempfile::tempdir().unwrap();
        seed_old_logs(dir.path(), 20);

        let _appender = build_file_appender(dir.path(), PREFIX, 0).unwrap();

        // 20 old files + today's new file
        assert_eq!(prefixed_files(dir.path()), 21);
    }

    #[test]
    fn unusable_directory_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("file");
        std::fs::write(&not_a_dir, b"x").unwrap();

        assert!(build_file_appender(&not_a_dir, PREFIX, 14).is_err());
    }
}
