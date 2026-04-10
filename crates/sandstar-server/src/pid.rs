//! PID file management with exclusive file locking.
//!
//! Prevents multiple engine instances from running simultaneously,
//! which would cause hardware bus conflicts (I2C, GPIO, ADC).
//!
//! Uses `flock(2)` on Unix and `LockFileEx` on Windows (via `fs2`).
//! The lock is released automatically when the process exits, even
//! on `kill -9` or panic.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;

use fs2::FileExt;
use tracing::{info, warn};

/// Manages a PID file with exclusive file locking.
///
/// On creation, writes the current PID and holds an exclusive lock.
/// Drop removes the file and releases the lock.
pub struct PidFile {
    path: PathBuf,
    _file: File, // held open to maintain the flock
}

impl PidFile {
    /// Create and lock a PID file. Returns Err if another instance is running.
    pub fn create(path: &Path) -> io::Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }

        // Check for stale PID before locking
        if path.exists() {
            if let Ok(contents) = fs::read_to_string(path) {
                if let Ok(old_pid) = contents.trim().parse::<u32>() {
                    if process_alive(old_pid) {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            format!("another instance running (PID {})", old_pid),
                        ));
                    }
                    warn!(old_pid, "removing stale PID file");
                }
            }
        }

        // Open, lock, write
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        file.try_lock_exclusive().map_err(|_| {
            io::Error::new(
                io::ErrorKind::AddrInUse,
                "PID file locked by another process",
            )
        })?;

        let mut f = &file;
        write!(f, "{}", process::id())?;
        f.flush()?;

        // Restrict PID file to owner-only (0600) on Unix to prevent
        // information disclosure or tampering by other users.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }

        info!(pid = process::id(), path = %path.display(), "PID file created");

        Ok(Self {
            path: path.to_path_buf(),
            _file: file,
        })
    }

    /// Get the PID file path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(&self.path) {
            // Don't panic in Drop
            eprintln!(
                "warning: failed to remove PID file {}: {}",
                self.path.display(),
                e
            );
        }
    }
}

/// Check if a process with the given PID is alive.
fn process_alive(_pid: u32) -> bool {
    #[cfg(all(unix, feature = "linux-hal"))]
    {
        // kill(pid, 0) checks existence without sending a signal
        unsafe { libc::kill(_pid as i32, 0) == 0 }
    }
    #[cfg(not(all(unix, feature = "linux-hal")))]
    {
        // On Windows / mock-hal builds, rely on file lock alone
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_acquire_and_drop() {
        let dir = TempDir::new().unwrap();
        let pid_path = dir.path().join("test.pid");

        {
            let _pid = PidFile::create(&pid_path).unwrap();
            assert!(pid_path.exists());
            // On Windows, exclusive flock is mandatory — can't read while locked.
            // On Unix, flock is advisory so read_to_string works fine.
            #[cfg(unix)]
            {
                let contents = fs::read_to_string(&pid_path).unwrap();
                assert_eq!(contents, process::id().to_string());
            }
        }
        // After drop, file should be removed
        assert!(!pid_path.exists());
    }

    #[test]
    fn test_double_acquire_fails() {
        let dir = TempDir::new().unwrap();
        let pid_path = dir.path().join("test.pid");

        let _pid1 = PidFile::create(&pid_path).unwrap();
        let result = PidFile::create(&pid_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_creates_parent_dir() {
        let dir = TempDir::new().unwrap();
        let pid_path = dir.path().join("nested").join("dir").join("test.pid");

        let _pid = PidFile::create(&pid_path).unwrap();
        assert!(pid_path.exists());
    }

    #[test]
    fn test_stale_pid_overwritten() {
        let dir = TempDir::new().unwrap();
        let pid_path = dir.path().join("test.pid");

        // Write a bogus PID (very unlikely to be alive)
        fs::write(&pid_path, "999999999").unwrap();

        // Should succeed — stale PID is overwritten
        let _pid = PidFile::create(&pid_path).unwrap();
        // On Windows, exclusive flock is mandatory — can't read while locked.
        #[cfg(unix)]
        {
            let contents = fs::read_to_string(&pid_path).unwrap();
            assert_eq!(contents, process::id().to_string());
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_pid_file_permissions_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let pid_path = dir.path().join("test_perms.pid");

        let _pid = PidFile::create(&pid_path).unwrap();
        let meta = fs::metadata(&pid_path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "PID file should have 0600 permissions, got {:o}",
            mode
        );
    }
}
