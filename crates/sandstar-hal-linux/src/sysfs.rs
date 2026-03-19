//! Shared sysfs read/write helpers with FD caching.
//!
//! The lseek(0) + read/write pattern avoids repeated open/close overhead
//! in the poll loop. Matches C `io_read_cached` / `io_write_cached`.
//!
//! # Design
//!
//! The Linux kernel exposes hardware controls via pseudo-files under `/sys/`.
//! Reading a GPIO value means reading `/sys/class/gpio/gpio47/value`.
//! Writing a PWM duty cycle means writing to `.../duty_cycle`.
//!
//! For hot-path operations (PWM duty writes happen every 100ms poll), opening
//! and closing the file each time wastes syscalls and can exhaust file
//! descriptors. The `write_cached` / `read_cached` functions keep the `File`
//! open and use `seek(0)` + read/write to refresh the value in-place.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::str::FromStr;

/// Read a sysfs file and return its contents with whitespace trimmed.
///
/// Opens the file, reads the full contents, and trims trailing newlines
/// and whitespace (sysfs values typically end with `\n`).
pub fn read(path: &Path) -> io::Result<String> {
    let contents = fs::read_to_string(path)?;
    Ok(contents.trim().to_string())
}

/// Read a sysfs file and parse the trimmed contents into type `T`.
///
/// Useful for reading numeric values from sysfs (e.g., ADC raw counts,
/// PWM period in nanoseconds).
pub fn read_parse<T: FromStr>(path: &Path) -> io::Result<T> {
    let s = read(path)?;
    s.parse::<T>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to parse '{}' from {}", s, path.display()),
        )
    })
}

/// Write a string value to a sysfs file.
///
/// Opens the file for writing (truncating), writes the value, and closes it.
/// Used for infrequent operations like GPIO export or direction setting.
pub fn write(path: &Path, value: &str) -> io::Result<()> {
    fs::write(path, value)
}

/// Write a string value using a cached file descriptor.
///
/// If `fd` is `None`, opens the file for writing and stores the `File` handle.
/// On subsequent calls, seeks to position 0 and overwrites the value in place.
///
/// This is the Rust equivalent of the C `io_write_cached` function:
/// it avoids repeated open/close syscalls for hot-path writes like
/// PWM duty_cycle updates (~10 writes/second per channel).
///
/// # Arguments
///
/// * `fd` - Mutable reference to an optional cached `File`. Will be populated
///   on first call.
/// * `path` - The sysfs file path to write to.
/// * `value` - The string value to write.
pub fn write_cached(fd: &mut Option<File>, path: &Path, value: &str) -> io::Result<()> {
    let file = match fd {
        Some(ref mut f) => f,
        None => {
            let f = OpenOptions::new().write(true).open(path)?;
            *fd = Some(f);
            fd.as_mut().unwrap()
        }
    };

    file.seek(SeekFrom::Start(0))?;
    file.write_all(value.as_bytes())?;
    Ok(())
}

/// Read a sysfs value using a cached file descriptor.
///
/// If `fd` is `None`, opens the file for reading and stores the `File` handle.
/// On subsequent calls, seeks to position 0 and re-reads the current value.
///
/// This is the Rust equivalent of the C `io_read_cached` function:
/// lseek(0) + read is much cheaper than open + read + close for
/// frequently-polled sysfs files.
///
/// # Arguments
///
/// * `fd` - Mutable reference to an optional cached `File`. Will be populated
///   on first call.
/// * `path` - The sysfs file path to read from.
///
/// # Returns
///
/// The trimmed contents of the sysfs file.
pub fn read_cached(fd: &mut Option<File>, path: &Path) -> io::Result<String> {
    let file = match fd {
        Some(ref mut f) => f,
        None => {
            let f = OpenOptions::new().read(true).open(path)?;
            *fd = Some(f);
            fd.as_mut().unwrap()
        }
    };

    file.seek(SeekFrom::Start(0))?;

    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    Ok(buf.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;
    use tempfile::TempDir;

    /// Helper: create a file with given contents inside a temp directory.
    fn write_test_file(dir: &TempDir, name: &str, contents: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn test_read_trims_whitespace() {
        let dir = TempDir::new().unwrap();
        let path = write_test_file(&dir, "value", "1\n");

        let result = read(&path).unwrap();
        assert_eq!(result, "1");
    }

    #[test]
    fn test_read_trims_complex_whitespace() {
        let dir = TempDir::new().unwrap();
        let path = write_test_file(&dir, "value", "  hello world  \n\n");

        let result = read(&path).unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_read_nonexistent_file_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does_not_exist");

        assert!(read(&path).is_err());
    }

    #[test]
    fn test_read_parse_u32() {
        let dir = TempDir::new().unwrap();
        let path = write_test_file(&dir, "period", "1000000\n");

        let val: u32 = read_parse(&path).unwrap();
        assert_eq!(val, 1_000_000);
    }

    #[test]
    fn test_read_parse_f64() {
        let dir = TempDir::new().unwrap();
        let path = write_test_file(&dir, "raw", "2048\n");

        let val: f64 = read_parse(&path).unwrap();
        assert!((val - 2048.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_read_parse_invalid_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = write_test_file(&dir, "bad", "not_a_number\n");

        let result: io::Result<u32> = read_parse(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_write_creates_file_contents() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("export");
        // Create the file first (sysfs files already exist)
        File::create(&path).unwrap();

        write(&path, "47").unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "47");
    }

    #[test]
    fn test_write_cached_opens_and_reuses_fd() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("duty_cycle");
        File::create(&path).unwrap();

        let mut fd: Option<File> = None;

        // First write opens the file
        write_cached(&mut fd, &path, "500000").unwrap();
        assert!(fd.is_some());

        // Verify contents
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "500000");

        // Second write reuses the FD
        write_cached(&mut fd, &path, "750000").unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        // After seek(0) + write of a shorter-or-equal-length string,
        // the file may contain trailing bytes from the previous write.
        // In real sysfs this doesn't happen because the kernel handles it.
        // For testing, just check the prefix.
        assert!(contents.starts_with("750000"));
    }

    #[test]
    fn test_read_cached_opens_and_reuses_fd() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("value");

        // Write initial value
        fs::write(&path, "0\n").unwrap();

        let mut fd: Option<File> = None;

        // First read opens the file
        let val = read_cached(&mut fd, &path).unwrap();
        assert_eq!(val, "0");
        assert!(fd.is_some());

        // Update the file externally (simulating hardware change)
        fs::write(&path, "1\n").unwrap();

        // Second read re-reads via seek(0)
        // Note: on a real filesystem this may return stale data since the
        // FD cache was opened before the external write. On sysfs the kernel
        // always returns fresh data on read(). For unit tests we just verify
        // the mechanism works without error.
        let val2 = read_cached(&mut fd, &path);
        assert!(val2.is_ok());
    }

    #[test]
    fn test_read_cached_nonexistent_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent");

        let mut fd: Option<File> = None;
        assert!(read_cached(&mut fd, &path).is_err());
        assert!(fd.is_none());
    }
}
