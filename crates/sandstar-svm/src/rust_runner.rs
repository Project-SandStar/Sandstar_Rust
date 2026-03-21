//! Pure Rust Sedona VM runner — replaces the C FFI-based SvmRunner.
//!
//! `RustSvmRunner` loads an scode image, initializes the pure Rust interpreter,
//! and provides `start()` / `resume()` / `stop()` lifecycle methods that mirror
//! the existing FFI-based `SvmRunner` but execute entirely in safe Rust.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tracing::info;

use crate::image_loader::ScodeImage;
use crate::native_table::NativeTable;
use crate::vm_error::{VmError, VmResult};
use crate::vm_interpreter::VmInterpreter;
use crate::vm_memory::VmMemory;

/// Pure Rust Sedona VM runner — replaces the C FFI-based SvmRunner.
///
/// Unlike `SvmRunner` which spawns a background thread and delegates to C FFI,
/// this runner executes bytecode directly via [`VmInterpreter`]. It is designed
/// to be called synchronously from the poll loop: `start()` runs the main
/// method, and `resume()` runs one application cycle per call.
pub struct RustSvmRunner {
    scode_path: PathBuf,
    running: Arc<AtomicBool>,
    interpreter: Option<VmInterpreter>,
    /// Byte offset of the resume entry method (cached from header).
    resume_offset: Option<usize>,
}

impl RustSvmRunner {
    /// Create a new runner for the given scode file.
    pub fn new(scode_path: impl Into<PathBuf>) -> Self {
        Self {
            scode_path: scode_path.into(),
            running: Arc::new(AtomicBool::new(false)),
            interpreter: None,
            resume_offset: None,
        }
    }

    /// Load the scode image, initialize the interpreter, and run the main method.
    ///
    /// This is the equivalent of `vmRun()` in the C code. After the main method
    /// returns, the VM is considered "running" and ready for `resume()` calls.
    pub fn start(&mut self) -> VmResult<()> {
        if self.is_running() {
            return Err(VmError::BadImage("VM already running".into()));
        }

        // 1. Load scode image from file
        let image = ScodeImage::load_from_file(&self.scode_path)?;

        info!(
            path = %self.scode_path.display(),
            image_size = image.header.image_size,
            data_size = image.header.data_size,
            main_block = image.header.main_method,
            resume_block = image.header.resume_method,
            "loaded scode image for pure Rust VM"
        );

        // 2. Cache the resume method offset
        let resume_method = image.header.resume_method;
        if resume_method != 0 {
            self.resume_offset = Some(image.block_to_offset(resume_method));
        } else {
            self.resume_offset = None;
        }

        // 3. Compute the main method entry point
        let main_offset = image.block_to_offset(image.header.main_method);

        // 4. Create memory from the image
        let memory = VmMemory::from_image(&image)?;

        // 5. Create native table with defaults
        let natives = NativeTable::with_defaults();

        // 6. Create interpreter
        let mut interpreter = VmInterpreter::new(memory, natives);

        // 7. Execute the main method (initialization)
        interpreter.execute(main_offset)?;

        // 8. Mark as running and store interpreter
        self.running.store(true, Ordering::SeqCst);
        self.interpreter = Some(interpreter);

        info!("pure Rust Sedona VM started");
        Ok(())
    }

    /// Resume execution — called each poll cycle to execute one Sedona app cycle.
    ///
    /// This is the equivalent of `vmResume()` in the C code. It executes the
    /// resume method entry point and returns its result.
    pub fn resume(&mut self) -> VmResult<i32> {
        if !self.is_running() {
            return Err(VmError::Stopped);
        }

        let resume_offset = self.resume_offset.ok_or_else(|| {
            VmError::BadImage("no resume method defined in scode header".into())
        })?;

        let interpreter = self.interpreter.as_mut().ok_or(VmError::Stopped)?;

        if interpreter.stopped {
            self.running.store(false, Ordering::SeqCst);
            return Err(VmError::Stopped);
        }

        interpreter.execute(resume_offset)
    }

    /// Stop the VM.
    pub fn stop(&mut self) {
        if let Some(ref mut interp) = self.interpreter {
            interp.stopped = true;
        }
        self.running.store(false, Ordering::SeqCst);
        info!("pure Rust Sedona VM stopped");
    }

    /// Check if the VM is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Get a reference to the interpreter (for inspection/testing).
    pub fn interpreter(&self) -> Option<&VmInterpreter> {
        self.interpreter.as_ref()
    }

    /// Get a mutable reference to the interpreter.
    pub fn interpreter_mut(&mut self) -> Option<&mut VmInterpreter> {
        self.interpreter.as_mut()
    }
}

impl Drop for RustSvmRunner {
    fn drop(&mut self) {
        self.stop();
    }
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_loader::{
        SCODE_BLOCK_SIZE, SCODE_HEADER_SIZE, SCODE_MAGIC, SCODE_MAJOR_VER, SCODE_MINOR_VER,
    };
    use crate::opcodes::Opcode;
    use std::io::Write;

    /// Build a minimal valid scode that immediately returns 0.
    /// The main method is a `LoadI0` + `ReturnPop` at the code start.
    fn make_minimal_scode() -> Vec<u8> {
        let code = &[Opcode::LoadI0 as u8, Opcode::ReturnPop as u8];
        let image_size = SCODE_HEADER_SIZE + code.len();
        let mut buf = vec![0u8; image_size];

        // Header
        buf[0..4].copy_from_slice(&SCODE_MAGIC.to_le_bytes());
        buf[4] = SCODE_MAJOR_VER;
        buf[5] = SCODE_MINOR_VER;
        buf[6] = SCODE_BLOCK_SIZE;
        buf[7] = 4; // ref_size
        buf[8..12].copy_from_slice(&(image_size as u32).to_le_bytes());
        buf[12..16].copy_from_slice(&256u32.to_le_bytes()); // data_size
        // main_method block = header_size / block_size
        let main_block = (SCODE_HEADER_SIZE / SCODE_BLOCK_SIZE as usize) as u16;
        buf[16..18].copy_from_slice(&main_block.to_le_bytes());
        buf[18..20].copy_from_slice(&0u16.to_le_bytes()); // tests_bix
        buf[24..26].copy_from_slice(&0u16.to_le_bytes()); // resume_method = 0

        // Code section
        buf[SCODE_HEADER_SIZE..].copy_from_slice(code);
        buf
    }

    /// Build a scode with both main and resume methods.
    /// Main does LoadI0 + ReturnPop; resume does LoadI1 + ReturnPop.
    fn make_scode_with_resume() -> Vec<u8> {
        let main_code = &[Opcode::LoadI0 as u8, Opcode::ReturnPop as u8];
        let resume_code = &[Opcode::LoadI1 as u8, Opcode::ReturnPop as u8];
        let code_len = main_code.len() + resume_code.len();
        let image_size = SCODE_HEADER_SIZE + code_len;
        let mut buf = vec![0u8; image_size];

        buf[0..4].copy_from_slice(&SCODE_MAGIC.to_le_bytes());
        buf[4] = SCODE_MAJOR_VER;
        buf[5] = SCODE_MINOR_VER;
        buf[6] = SCODE_BLOCK_SIZE;
        buf[7] = 4;
        buf[8..12].copy_from_slice(&(image_size as u32).to_le_bytes());
        buf[12..16].copy_from_slice(&256u32.to_le_bytes()); // data_size

        // main_method at offset SCODE_HEADER_SIZE
        let main_block = (SCODE_HEADER_SIZE / SCODE_BLOCK_SIZE as usize) as u16;
        buf[16..18].copy_from_slice(&main_block.to_le_bytes());
        buf[18..20].copy_from_slice(&0u16.to_le_bytes()); // tests_bix

        // resume_method right after main code
        // Ensure alignment: resume must start on a block boundary
        // main_code is 2 bytes, so resume starts at SCODE_HEADER_SIZE + 2
        // but block index = byte_offset / block_size, so we need it to be aligned.
        // We'll pad main to 4 bytes (one full block) so resume starts at next block.
        // Actually let's just recompute with padding.
        drop(buf); // redo

        let main_padded = &[Opcode::LoadI0 as u8, Opcode::ReturnPop as u8, 0, 0]; // pad to 4
        let resume_bytes = &[Opcode::LoadI1 as u8, Opcode::ReturnPop as u8];
        let total_code = main_padded.len() + resume_bytes.len();
        let image_size = SCODE_HEADER_SIZE + total_code;
        let mut buf = vec![0u8; image_size];

        buf[0..4].copy_from_slice(&SCODE_MAGIC.to_le_bytes());
        buf[4] = SCODE_MAJOR_VER;
        buf[5] = SCODE_MINOR_VER;
        buf[6] = SCODE_BLOCK_SIZE;
        buf[7] = 4;
        buf[8..12].copy_from_slice(&(image_size as u32).to_le_bytes());
        buf[12..16].copy_from_slice(&256u32.to_le_bytes());

        let main_block = (SCODE_HEADER_SIZE / SCODE_BLOCK_SIZE as usize) as u16;
        buf[16..18].copy_from_slice(&main_block.to_le_bytes());
        buf[18..20].copy_from_slice(&0u16.to_le_bytes());

        // resume starts at SCODE_HEADER_SIZE + 4 = block (main_block + 1)
        let resume_block = main_block + 1;
        buf[24..26].copy_from_slice(&resume_block.to_le_bytes());

        buf[SCODE_HEADER_SIZE..SCODE_HEADER_SIZE + main_padded.len()]
            .copy_from_slice(main_padded);
        buf[SCODE_HEADER_SIZE + main_padded.len()..].copy_from_slice(resume_bytes);

        buf
    }

    /// Write bytes to a temp file and return the path.
    fn write_temp_scode(name: &str, data: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join("sandstar_rust_runner_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).expect("create temp file");
        f.write_all(data).expect("write temp file");
        path
    }

    // ── Test 1: new() creates non-running runner ──

    #[test]
    fn test_new_creates_non_running_runner() {
        let runner = RustSvmRunner::new("/nonexistent/kits.scode");
        assert!(!runner.is_running());
        assert!(runner.interpreter().is_none());
    }

    // ── Test 2: start() with nonexistent file returns error ──

    #[test]
    fn test_start_nonexistent_file() {
        let mut runner = RustSvmRunner::new("/totally/bogus/path/kits.scode");
        let result = runner.start();
        assert!(result.is_err());
        match result.unwrap_err() {
            VmError::BadImage(msg) => assert!(
                msg.contains("failed to read"),
                "expected file-read error, got: {msg}"
            ),
            other => panic!("expected BadImage, got: {other:?}"),
        }
        assert!(!runner.is_running());
    }

    // ── Test 3: start() with invalid (too short) scode returns BadImage ──

    #[test]
    fn test_start_invalid_scode_too_short() {
        let path = write_temp_scode("invalid_short.scode", &[0xDE, 0xAD]);
        let mut runner = RustSvmRunner::new(&path);
        let result = runner.start();
        assert!(result.is_err());
        match result.unwrap_err() {
            VmError::BadImage(msg) => assert!(
                msg.contains("too short"),
                "expected 'too short' error, got: {msg}"
            ),
            other => panic!("expected BadImage, got: {other:?}"),
        }
        assert!(!runner.is_running());
        let _ = std::fs::remove_file(&path);
    }

    // ── Test 4: start() with bad magic returns BadImage ──

    #[test]
    fn test_start_bad_magic() {
        let mut data = make_minimal_scode();
        data[0] = 0xFF; // corrupt magic
        let path = write_temp_scode("bad_magic.scode", &data);
        let mut runner = RustSvmRunner::new(&path);
        let result = runner.start();
        assert!(result.is_err());
        match result.unwrap_err() {
            VmError::BadImage(msg) => assert!(
                msg.contains("bad magic"),
                "expected 'bad magic' error, got: {msg}"
            ),
            other => panic!("expected BadImage, got: {other:?}"),
        }
        assert!(!runner.is_running());
        let _ = std::fs::remove_file(&path);
    }

    // ── Test 5: stop() is idempotent ──

    #[test]
    fn test_stop_idempotent() {
        let mut runner = RustSvmRunner::new("/nonexistent/kits.scode");
        runner.stop(); // not running — should be no-op
        runner.stop(); // second call also fine
        assert!(!runner.is_running());
    }

    // ── Test 6: is_running() reflects state ──

    #[test]
    fn test_is_running_reflects_state() {
        let data = make_minimal_scode();
        let path = write_temp_scode("running_state.scode", &data);
        let mut runner = RustSvmRunner::new(&path);

        assert!(!runner.is_running());
        runner.start().expect("start should succeed");
        assert!(runner.is_running());
        runner.stop();
        assert!(!runner.is_running());

        let _ = std::fs::remove_file(&path);
    }

    // ── Test 7: Drop stops the runner ──

    #[test]
    fn test_drop_stops_runner() {
        let data = make_minimal_scode();
        let path = write_temp_scode("drop_test.scode", &data);
        let running_flag;
        {
            let mut runner = RustSvmRunner::new(&path);
            runner.start().expect("start should succeed");
            running_flag = runner.running.clone();
            assert!(running_flag.load(Ordering::Relaxed));
            // runner dropped here
        }
        assert!(!running_flag.load(Ordering::Relaxed));

        let _ = std::fs::remove_file(&path);
    }

    // ── Test 8: start() with valid minimal scode succeeds ──

    #[test]
    fn test_start_valid_minimal_scode() {
        let data = make_minimal_scode();
        let path = write_temp_scode("minimal_valid.scode", &data);
        let mut runner = RustSvmRunner::new(&path);

        let result = runner.start();
        assert!(result.is_ok(), "start failed: {:?}", result.unwrap_err());
        assert!(runner.is_running());
        assert!(runner.interpreter().is_some());

        runner.stop();
        let _ = std::fs::remove_file(&path);
    }

    // ── Test 9: resume() after start with resume method returns result ──

    #[test]
    fn test_resume_after_start() {
        let data = make_scode_with_resume();
        let path = write_temp_scode("with_resume.scode", &data);
        let mut runner = RustSvmRunner::new(&path);

        runner.start().expect("start should succeed");
        let result = runner.resume();
        assert!(result.is_ok(), "resume failed: {:?}", result.unwrap_err());
        // Resume method pushes 1 (LoadI1) and returns it
        assert_eq!(result.unwrap(), 1);

        runner.stop();
        let _ = std::fs::remove_file(&path);
    }

    // ── Test 10: resume() without start returns error ──

    #[test]
    fn test_resume_without_start() {
        let mut runner = RustSvmRunner::new("/nonexistent/kits.scode");
        let result = runner.resume();
        assert!(result.is_err());
        match result.unwrap_err() {
            VmError::Stopped => {} // expected
            other => panic!("expected Stopped, got: {other:?}"),
        }
    }

    // ── Test 11: interpreter() returns None before start, Some after ──

    #[test]
    fn test_interpreter_before_and_after_start() {
        let data = make_minimal_scode();
        let path = write_temp_scode("interp_access.scode", &data);
        let mut runner = RustSvmRunner::new(&path);

        assert!(runner.interpreter().is_none());
        assert!(runner.interpreter_mut().is_none());

        runner.start().expect("start should succeed");

        assert!(runner.interpreter().is_some());
        assert!(runner.interpreter_mut().is_some());

        runner.stop();
        let _ = std::fs::remove_file(&path);
    }

    // ── Test 12: resume() without resume method returns error ──

    #[test]
    fn test_resume_no_resume_method() {
        let data = make_minimal_scode(); // resume_method = 0
        let path = write_temp_scode("no_resume.scode", &data);
        let mut runner = RustSvmRunner::new(&path);

        runner.start().expect("start should succeed");
        let result = runner.resume();
        assert!(result.is_err());
        match result.unwrap_err() {
            VmError::BadImage(msg) => assert!(
                msg.contains("no resume method"),
                "expected 'no resume method', got: {msg}"
            ),
            other => panic!("expected BadImage, got: {other:?}"),
        }

        runner.stop();
        let _ = std::fs::remove_file(&path);
    }

    // ── Test 13: resume() after stop returns Stopped ──

    #[test]
    fn test_resume_after_stop() {
        let data = make_scode_with_resume();
        let path = write_temp_scode("resume_after_stop.scode", &data);
        let mut runner = RustSvmRunner::new(&path);

        runner.start().expect("start should succeed");
        runner.stop();

        let result = runner.resume();
        assert!(result.is_err());
        match result.unwrap_err() {
            VmError::Stopped => {} // expected
            other => panic!("expected Stopped, got: {other:?}"),
        }

        let _ = std::fs::remove_file(&path);
    }

    // ── Test 14: double start returns error ──

    #[test]
    fn test_double_start() {
        let data = make_minimal_scode();
        let path = write_temp_scode("double_start.scode", &data);
        let mut runner = RustSvmRunner::new(&path);

        runner.start().expect("first start should succeed");
        let result = runner.start();
        assert!(result.is_err());
        match result.unwrap_err() {
            VmError::BadImage(msg) => assert!(
                msg.contains("already running"),
                "expected 'already running', got: {msg}"
            ),
            other => panic!("expected BadImage, got: {other:?}"),
        }

        runner.stop();
        let _ = std::fs::remove_file(&path);
    }

    // ── Test 15: multiple resume cycles work ──

    #[test]
    fn test_multiple_resume_cycles() {
        let data = make_scode_with_resume();
        let path = write_temp_scode("multi_resume.scode", &data);
        let mut runner = RustSvmRunner::new(&path);

        runner.start().expect("start should succeed");

        for _ in 0..10 {
            let result = runner.resume();
            assert!(result.is_ok(), "resume failed: {:?}", result.unwrap_err());
            assert_eq!(result.unwrap(), 1);
        }

        runner.stop();
        let _ = std::fs::remove_file(&path);
    }

    // ── Test 16: drop after failed start does not panic ──

    #[test]
    fn test_drop_after_failed_start() {
        let mut runner = RustSvmRunner::new("/nonexistent/kits.scode");
        let _ = runner.start(); // will fail
        drop(runner); // should not panic
    }

    // ── Test 17: interpreter_mut allows state modification ──

    #[test]
    fn test_interpreter_mut_modification() {
        let data = make_minimal_scode();
        let path = write_temp_scode("interp_mut.scode", &data);
        let mut runner = RustSvmRunner::new(&path);

        runner.start().expect("start should succeed");

        // Verify we can modify interpreter state
        let interp = runner.interpreter_mut().unwrap();
        interp.stopped = true;
        assert!(runner.interpreter().unwrap().stopped);

        runner.stop();
        let _ = std::fs::remove_file(&path);
    }

    // ── Test 18: start after stop works (restart) ──

    #[test]
    fn test_restart_after_stop() {
        let data = make_minimal_scode();
        let path = write_temp_scode("restart.scode", &data);
        let mut runner = RustSvmRunner::new(&path);

        runner.start().expect("first start");
        assert!(runner.is_running());
        runner.stop();
        assert!(!runner.is_running());

        // Should be able to start again
        runner.start().expect("second start");
        assert!(runner.is_running());

        runner.stop();
        let _ = std::fs::remove_file(&path);
    }

    // ── Test 19: resume detects interpreter.stopped flag ──

    #[test]
    fn test_resume_detects_stopped_interpreter() {
        let data = make_scode_with_resume();
        let path = write_temp_scode("stopped_interp.scode", &data);
        let mut runner = RustSvmRunner::new(&path);

        runner.start().expect("start should succeed");
        // Manually set the interpreter's stopped flag
        runner.interpreter_mut().unwrap().stopped = true;

        let result = runner.resume();
        assert!(result.is_err());
        match result.unwrap_err() {
            VmError::Stopped => {} // expected
            other => panic!("expected Stopped, got: {other:?}"),
        }
        // Running flag should also be cleared
        assert!(!runner.is_running());

        let _ = std::fs::remove_file(&path);
    }
}
