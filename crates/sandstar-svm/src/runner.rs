//! High-level SVM lifecycle management.
//!
//! `SvmRunner` loads the Sedona bytecode image (`kits.scode`), initializes
//! the VM, and runs it in a background thread with a yield/hibernate/restart
//! loop matching the C `main.cpp` platform mode.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use tracing::{error, info, warn};

use crate::ffi;
use crate::types::*;

/// Manages the SVM background thread lifecycle.
pub struct SvmRunner {
    scode_path: PathBuf,
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<i32>>,
}

impl SvmRunner {
    /// Create a new runner for the given scode image.
    pub fn new(scode_path: impl Into<PathBuf>) -> Self {
        Self {
            scode_path: scode_path.into(),
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    /// Is the SVM currently running?
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Start the SVM in a background thread.
    pub fn start(&mut self) -> Result<(), String> {
        if self.is_running() {
            return Err("SVM already running".into());
        }

        let scode_path = self.scode_path.clone();
        let running = self.running.clone();

        // Load scode image into memory before spawning thread
        let scode_data = std::fs::read(&scode_path).map_err(|e| {
            format!("failed to read {}: {}", scode_path.display(), e)
        })?;

        info!(
            path = %scode_path.display(),
            size = scode_data.len(),
            "loaded Sedona scode image"
        );

        running.store(true, Ordering::SeqCst);

        let handle = thread::Builder::new()
            .name("sedona-vm".into())
            .spawn(move || {
                let result = run_platform_mode(&scode_data, &running);
                running.store(false, Ordering::SeqCst);
                result
            })
            .map_err(|e| format!("failed to spawn SVM thread: {}", e))?;

        self.handle = Some(handle);
        info!("Sedona VM started");
        Ok(())
    }

    /// Signal the SVM to stop and wait for thread to exit.
    pub fn stop(&mut self) {
        if !self.is_running() {
            return;
        }

        info!("stopping Sedona VM...");
        self.running.store(false, Ordering::SeqCst);
        unsafe { ffi::stopVm(); }

        if let Some(handle) = self.handle.take() {
            match handle.join() {
                Ok(code) => info!(exit_code = code, "Sedona VM stopped"),
                Err(_) => error!("SVM thread panicked"),
            }
        }
    }
}

impl Drop for SvmRunner {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Run the SVM in platform mode: yield/hibernate/restart loop.
///
/// This matches the C `runInPlatformMode()` from `main.cpp`.
fn run_platform_mode(scode: &[u8], running: &AtomicBool) -> i32 {
    const STACK_SIZE: usize = 64 * 1024; // 64KB stack (generous for ARM)

    // Allocate stack
    let mut stack = vec![0u8; STACK_SIZE];

    // Get the nativetable pointer from the C library
    extern "C" {
        static mut nativeTable: *mut NativeMethod;
    }

    // Set up the VM struct
    let mut vm = SedonaVM {
        code_base_addr: scode.as_ptr(),
        code_size: scode.len(),
        stack_base_addr: stack.as_mut_ptr(),
        stack_max_size: STACK_SIZE,
        sp: std::ptr::null_mut(),
        args: std::ptr::null(),
        args_len: 0,
        on_assert_failure: Some(on_assert_failure),
        assert_successes: 0,
        assert_failures: 0,
        native_table: &raw mut nativeTable,
        call: Some(vm_call_trampoline),
        data_base_addr: std::ptr::null_mut(),
    };

    // Initial run
    let mut result = unsafe { ffi::vmRun(&mut vm) };
    info!(result, "SVM initial vmRun returned");

    // Platform loop: handle yield, hibernate, restart
    loop {
        if !running.load(Ordering::Relaxed) {
            info!("SVM shutdown requested");
            break;
        }

        match result {
            0 => {
                info!("SVM exited cleanly");
                break;
            }
            ERR_YIELD => {
                // Short sleep, then resume (Sedona yield = ~10ms)
                thread::sleep(std::time::Duration::from_millis(10));
                result = unsafe { ffi::vmResume(&mut vm) };
            }
            ERR_HIBERNATE => {
                // Longer sleep, then resume (Sedona hibernate = ~100ms)
                thread::sleep(std::time::Duration::from_millis(100));
                result = unsafe { ffi::vmResume(&mut vm) };
            }
            ERR_RESTART => {
                warn!("SVM requested restart");
                // Reset stack pointer and re-run
                vm.sp = std::ptr::null_mut();
                result = unsafe { ffi::vmRun(&mut vm) };
            }
            ERR_STOP_BY_USER => {
                info!("SVM stopped by user");
                break;
            }
            code if (1..=39).contains(&code) => {
                error!(code, "SVM unrecoverable error (bad image/runtime)");
                break;
            }
            code if (100..=139).contains(&code) => {
                warn!(code, "SVM recoverable error — restarting");
                thread::sleep(std::time::Duration::from_secs(1));
                vm.sp = std::ptr::null_mut();
                result = unsafe { ffi::vmRun(&mut vm) };
            }
            code => {
                error!(code, "SVM unknown error code");
                break;
            }
        }
    }

    result
}

/// Callback for Sedona assert failures.
unsafe extern "C" fn on_assert_failure(location: *const std::os::raw::c_char, linenum: u16) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let loc = if location.is_null() {
            "<unknown>"
        } else {
            std::ffi::CStr::from_ptr(location)
                .to_str()
                .unwrap_or("<invalid>")
        };
        error!(location = loc, line = linenum, "Sedona assert failure");
    }));
}

/// Trampoline for vm->call: delegates to vmCall.
unsafe extern "C" fn vm_call_trampoline(
    vm: *mut SedonaVM,
    method: u16,
    args: *mut Cell,
    argc: i32,
) -> i32 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ffi::vmCall(vm, method, args, argc)
    })) {
        Ok(result) => result,
        Err(e) => {
            let msg = e
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| e.downcast_ref::<String>().map(|s| s.as_str()))
                .unwrap_or("<unknown panic>");
            eprintln!("PANIC in FFI vm_call_trampoline: {}", msg);
            -1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runner_not_running() {
        let runner = SvmRunner::new("/nonexistent/kits.scode");
        assert!(!runner.is_running());
    }

    #[test]
    fn test_runner_start_missing_file() {
        let mut runner = SvmRunner::new("/nonexistent/kits.scode");
        let result = runner.start();
        assert!(result.is_err());
        assert!(!runner.is_running());
    }

    #[test]
    fn test_runner_start_missing_scode() {
        let mut runner = SvmRunner::new("/totally/bogus/path/kits.scode");
        let result = runner.start();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("failed to read"),
            "expected file-read error, got: {}",
            err
        );
        assert!(!runner.is_running());
    }

    #[test]
    fn test_runner_start_empty_scode_reads_ok() {
        // Create a temp file with zero bytes — verify the file-read phase
        // succeeds for an empty file (no panic, no error from fs::read).
        // We do NOT call start() because the VM thread would segfault on
        // empty bytecode. Instead, test the read path directly.
        let dir = std::env::temp_dir().join("sandstar_test_empty_scode");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("empty.scode");
        std::fs::write(&path, b"").unwrap();

        let data = std::fs::read(&path);
        assert!(data.is_ok());
        assert_eq!(data.unwrap().len(), 0);

        // Cleanup
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_runner_stop_idempotent() {
        let mut runner = SvmRunner::new("/nonexistent/kits.scode");
        // Runner was never started, so stop() should be a no-op
        runner.stop();
        runner.stop(); // second call should also be fine
        assert!(!runner.is_running());
    }

    #[test]
    fn test_runner_not_started_stop() {
        let mut runner = SvmRunner::new("/some/path.scode");
        // Calling stop on a runner that was never started must be a no-op
        runner.stop();
        assert!(!runner.is_running());
    }

    #[test]
    fn test_runner_drop_after_failed_start() {
        // Create runner, fail to start, then drop — should not panic
        let mut runner = SvmRunner::new("/nonexistent/kits.scode");
        let _ = runner.start(); // will fail
        drop(runner); // Drop calls stop(), which should handle None handle gracefully
    }

    #[test]
    fn test_runner_running_flag_initially_false() {
        let runner = SvmRunner::new("/any/path.scode");
        assert!(!runner.is_running());
        // The running flag is an Arc<AtomicBool>, starts false
        assert!(!runner.running.load(Ordering::Relaxed));
    }

    #[test]
    fn test_runner_double_start_after_failure() {
        let mut runner = SvmRunner::new("/nonexistent/kits.scode");
        let r1 = runner.start();
        assert!(r1.is_err());
        // Second start attempt should also fail cleanly
        let r2 = runner.start();
        assert!(r2.is_err());
        assert!(!runner.is_running());
    }
}
