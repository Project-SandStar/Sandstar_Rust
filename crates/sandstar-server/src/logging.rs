//! Logging initialization with optional file output.
//!
//! Dual output: stderr (for systemd journal / interactive) + optional
//! daily-rotated file. Uses `tracing-appender` non-blocking writer
//! to avoid blocking the poll loop. Old daily files are deleted so the log
//! directory cannot fill the disk (R-41).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Daily-rotated file writer (`<prefix>.YYYY-MM-DD`) that deletes old
/// daily files at startup and when the UTC day changes.
///
/// We do not use tracing-appender's `max_log_files`: it orders files by
/// creation time and, when the filesystem has none (device 1-6: kernel
/// 4.9, `stat %W` = 0), falls back to parsing the daily date as a
/// date-time, which fails, so it silently deletes nothing
/// (tracing-appender 0.2.4, `rolling.rs` `prune_old_logs`). Ordering by
/// the date in the file name works everywhere.
struct PruningWriter {
    inner: RollingFileAppender,
    dir: PathBuf,
    prefix: String,
    max_files: usize,
    last_day: u64,
}

impl PruningWriter {
    fn prune(&self) {
        if let Err(e) = prune_old_logs(&self.dir, &self.prefix, self.max_files) {
            // We are inside the log writer: logging through tracing would recurse.
            eprintln!(
                "logging: cannot prune old log files in {}: {e}",
                self.dir.display()
            );
        }
    }
}

impl Write for PruningWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        // After the write, so the new day's file exists when we prune.
        let day = utc_day_now();
        if day != self.last_day {
            self.last_day = day;
            self.prune();
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Days since the Unix epoch (UTC), matching the appender's UTC file dates.
fn utc_day_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}

/// `<prefix>.YYYY-MM-DD` -> `YYYY-MM-DD`; anything else -> `None`.
fn daily_suffix<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    let date = name.strip_prefix(prefix)?.strip_prefix('.')?;
    let bytes = date.as_bytes();
    let well_formed = bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                *b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    well_formed.then_some(date)
}

/// Delete the oldest `<prefix>.YYYY-MM-DD` files in `dir` so that at most
/// `max_files` remain, ordered by the date in the name. Other files are
/// never touched. `0` keeps everything. Returns how many were deleted.
fn prune_old_logs(dir: &Path, prefix: &str, max_files: usize) -> io::Result<usize> {
    if max_files == 0 {
        return Ok(0);
    }
    let mut dated: Vec<(String, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name();
        let Some(date) = name.to_str().and_then(|n| daily_suffix(n, prefix)) else {
            continue;
        };
        dated.push((date.to_string(), entry.path()));
    }
    if dated.len() <= max_files {
        return Ok(0);
    }
    dated.sort();
    let excess = dated.len() - max_files;
    let mut removed = 0;
    for (_, path) in dated.iter().take(excess) {
        match std::fs::remove_file(path) {
            Ok(()) => removed += 1,
            Err(e) => eprintln!("logging: cannot delete {}: {e}", path.display()),
        }
    }
    Ok(removed)
}

/// Build the daily file writer and prune old files once at startup.
///
/// Keeps at most `max_files` files with this prefix (the current day
/// included); `0` disables deletion. Returns an error instead of panicking
/// when the directory cannot be used.
fn build_file_appender(
    dir: &Path,
    file_name: &str,
    max_files: usize,
) -> Result<PruningWriter, InitError> {
    let inner = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(file_name)
        .build(dir)?;
    let writer = PruningWriter {
        inner,
        dir: dir.to_path_buf(),
        prefix: file_name.to_string(),
        max_files,
        last_day: utc_day_now(),
    };
    writer.prune();
    Ok(writer)
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
    fn prune_orders_by_name_date_and_ignores_other_files() {
        let dir = tempfile::tempdir().unwrap();
        // Written newest-first, so creation-time order is the reverse of the
        // name order: pruning must follow the name (the device has no btime).
        for name in [
            "2026-11-18",
            "2026-11-17",
            "2026-11-14",
            "2016-11-11",
            "2016-11-03",
        ] {
            std::fs::write(dir.path().join(format!("{PREFIX}.{name}")), b"x").unwrap();
        }
        let others = [
            PREFIX.to_string(),
            format!("{PREFIX}.bak"),
            format!("{PREFIX}.2026-1-18"),
            format!("{PREFIX}.2026-11-18.gz"),
            format!("{PREFIX}.2026-11-1ş"),
            "health-monitor.log".to_string(),
        ];
        for other in &others {
            std::fs::write(dir.path().join(other), b"keep").unwrap();
        }
        std::fs::create_dir(dir.path().join(format!("{PREFIX}.2000-01-01"))).unwrap();

        assert_eq!(prune_old_logs(dir.path(), PREFIX, 3).unwrap(), 2);
        for gone in ["2016-11-03", "2016-11-11"] {
            assert!(!dir.path().join(format!("{PREFIX}.{gone}")).exists());
        }
        for kept in ["2026-11-14", "2026-11-17", "2026-11-18"] {
            assert!(dir.path().join(format!("{PREFIX}.{kept}")).exists());
        }
        for other in &others {
            assert!(dir.path().join(other).exists(), "{other}");
        }
        assert!(dir.path().join(format!("{PREFIX}.2000-01-01")).is_dir());
        // Already within the limit / 0 = keep all: nothing deleted.
        assert_eq!(prune_old_logs(dir.path(), PREFIX, 3).unwrap(), 0);
        assert_eq!(prune_old_logs(dir.path(), PREFIX, 0).unwrap(), 0);
    }

    #[test]
    fn daily_suffix_edge_cases() {
        assert_eq!(daily_suffix("a.2026-11-18", "a"), Some("2026-11-18"));
        assert_eq!(daily_suffix("a.2026-11-18", "b"), None);
        assert_eq!(daily_suffix("", "a"), None);
        assert_eq!(daily_suffix("a.", "a"), None);
        assert_eq!(daily_suffix("a2026-11-18", "a"), None);
        assert_eq!(daily_suffix("a.2026/11/18", "a"), None);
        assert_eq!(daily_suffix("a.ğğğ-ğ-ğ", "a"), None);
    }

    #[test]
    fn writer_prunes_when_the_day_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = build_file_appender(dir.path(), PREFIX, 3).unwrap();
        // Today's file only.
        assert_eq!(prefixed_files(dir.path()), 1);
        seed_old_logs(dir.path(), 5);
        assert_eq!(prefixed_files(dir.path()), 6);

        // Same day: no pruning on write.
        writer
            .write_all(
                b"line
",
            )
            .unwrap();
        assert_eq!(prefixed_files(dir.path()), 6);

        // Simulate a day change.
        writer.last_day = writer.last_day.wrapping_sub(1);
        writer
            .write_all(
                b"line
",
            )
            .unwrap();
        assert_eq!(prefixed_files(dir.path()), 3);
    }

    #[test]
    fn unusable_directory_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("file");
        std::fs::write(&not_a_dir, b"x").unwrap();

        assert!(build_file_appender(&not_a_dir, PREFIX, 14).is_err());
    }
}
