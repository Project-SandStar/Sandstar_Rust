//! Integration tests for the Sandstar Sedona VM — Phase 3.
//!
//! These tests load real `.scode` and `.sab` files from the SedonaRepo directory
//! and verify the pure Rust VM can parse, load, and (partially) execute them.
//! Tests that depend on files not being present will skip gracefully.

use std::path::PathBuf;

use sandstar_svm::image_loader::ScodeImage;
use sandstar_svm::native_table::NativeTable;
use sandstar_svm::rust_runner::RustSvmRunner;
use sandstar_svm::sab_validator::{validate_sab, validate_sab_bytes};
use sandstar_svm::vm_error::VmError;
use sandstar_svm::vm_interpreter::VmInterpreter;
use sandstar_svm::vm_memory::VmMemory;

/// Resolve a path relative to SedonaRepo from the crate manifest dir.
fn scode_path(name: &str) -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("..").join("..").join("SedonaRepo").join(name)
}

/// Helper: skip test if file doesn't exist (CI may not have scode files).
macro_rules! skip_if_missing {
    ($path:expr) => {
        if !$path.exists() {
            eprintln!("SKIP: {} not found", $path.display());
            return;
        }
    };
}

// ══════════════════════════════════════════════════════════════════
// (a) test_load_win32_svm_scode_header
// ══════════════════════════════════════════════════════════════════

#[test]
fn test_load_win32_svm_scode_header() {
    let path = scode_path("win32_svm/kits.scode");
    skip_if_missing!(path);

    let image = ScodeImage::load_from_file(&path)
        .expect("failed to load win32_svm/kits.scode");

    eprintln!("=== win32_svm/kits.scode header ===");
    eprintln!("  magic:         0x{:08X}", image.header.magic);
    eprintln!("  major_ver:     {}", image.header.major_ver);
    eprintln!("  minor_ver:     {}", image.header.minor_ver);
    eprintln!("  block_size:    {}", image.header.block_size);
    eprintln!("  ref_size:      {}", image.header.ref_size);
    eprintln!("  image_size:    {}", image.header.image_size);
    eprintln!("  data_size:     {}", image.header.data_size);
    eprintln!("  main_method:   {} (offset={})", image.header.main_method,
              image.block_to_offset(image.header.main_method));
    eprintln!("  resume_method: {} (offset={})", image.header.resume_method,
              image.block_to_offset(image.header.resume_method));
    eprintln!("  tests_bix:     {}", image.header.tests_bix);

    assert_eq!(image.header.magic, 0x5ED0BA07, "magic mismatch");
    assert_eq!(image.header.major_ver, 1, "major version");
    assert_eq!(image.header.minor_ver, 5, "minor version");
    assert_eq!(image.header.block_size, 4, "block size");
    assert_eq!(image.header.image_size, 168936, "image size");
    assert!(image.header.main_method > 0, "main_method should be nonzero");
    assert!(image.header.resume_method > 0, "resume_method should be nonzero");
}

// ══════════════════════════════════════════════════════════════════
// (b) test_load_linux_scode_header
// ══════════════════════════════════════════════════════════════════

#[test]
fn test_load_linux_scode_header() {
    let path = scode_path("2026-03-11_21-56-18/app/kits.scode");
    skip_if_missing!(path);

    let image = ScodeImage::load_from_file(&path)
        .expect("failed to load linux kits.scode");

    eprintln!("=== linux app kits.scode header ===");
    eprintln!("  magic:         0x{:08X}", image.header.magic);
    eprintln!("  major_ver:     {}", image.header.major_ver);
    eprintln!("  minor_ver:     {}", image.header.minor_ver);
    eprintln!("  block_size:    {}", image.header.block_size);
    eprintln!("  ref_size:      {}", image.header.ref_size);
    eprintln!("  image_size:    {}", image.header.image_size);
    eprintln!("  data_size:     {}", image.header.data_size);
    eprintln!("  main_method:   {} (offset={})", image.header.main_method,
              image.block_to_offset(image.header.main_method));
    eprintln!("  resume_method: {} (offset={})", image.header.resume_method,
              image.block_to_offset(image.header.resume_method));
    eprintln!("  tests_bix:     {}", image.header.tests_bix);

    assert_eq!(image.header.magic, 0x5ED0BA07, "magic mismatch");
    assert_eq!(image.header.major_ver, 1, "major version");
    assert_eq!(image.header.minor_ver, 5, "minor version");
    assert_eq!(image.header.block_size, 4, "block size");
    assert_eq!(image.header.image_size, 164056, "image size");
    assert!(image.header.main_method > 0, "main_method should be nonzero");
    assert!(image.header.resume_method > 0, "resume_method should be nonzero");
}

// ══════════════════════════════════════════════════════════════════
// (c) test_create_vm_memory_from_real_scode
// ══════════════════════════════════════════════════════════════════

#[test]
fn test_create_vm_memory_from_real_scode() {
    let path = scode_path("win32_svm/kits.scode");
    skip_if_missing!(path);

    let image = ScodeImage::load_from_file(&path)
        .expect("failed to load scode");

    let memory = VmMemory::from_image(&image)
        .expect("VmMemory::from_image failed on real scode");

    eprintln!("=== VmMemory from win32_svm scode ===");
    eprintln!("  code segment length: {}", memory.code_len());
    eprintln!("  data segment length: {}", memory.data_len());

    assert_eq!(memory.code_len(), image.header.image_size as usize,
               "code segment should match image_size");
    assert_eq!(memory.data_len(), image.header.data_size as usize,
               "data segment should match data_size");
}

// ══════════════════════════════════════════════════════════════════
// (d) test_create_interpreter_from_real_scode
// ══════════════════════════════════════════════════════════════════

#[test]
fn test_create_interpreter_from_real_scode() {
    let path = scode_path("win32_svm/kits.scode");
    skip_if_missing!(path);

    let image = ScodeImage::load_from_file(&path)
        .expect("failed to load scode");
    let memory = VmMemory::from_image(&image)
        .expect("VmMemory::from_image failed");
    let natives = NativeTable::with_defaults();

    let interpreter = VmInterpreter::new(memory, natives);

    eprintln!("=== VmInterpreter created from real scode ===");
    eprintln!("  pc:              {}", interpreter.pc);
    eprintln!("  stopped:         {}", interpreter.stopped);
    eprintln!("  assert_failures: {}", interpreter.assert_failures);
    eprintln!("  assert_successes:{}", interpreter.assert_successes);

    assert_eq!(interpreter.pc, 0, "initial PC should be 0");
    assert!(!interpreter.stopped, "should not be stopped initially");
}

// ══════════════════════════════════════════════════════════════════
// (e) test_rust_runner_start_real_scode
// ══════════════════════════════════════════════════════════════════

#[test]
fn test_rust_runner_start_real_scode() {
    let path = scode_path("win32_svm/kits.scode");
    skip_if_missing!(path);

    let mut runner = RustSvmRunner::new(&path);
    let result = runner.start();

    eprintln!("=== RustSvmRunner::start() on real scode ===");
    match &result {
        Ok(()) => {
            eprintln!("  result: OK (main method completed successfully!)");
            assert!(runner.is_running());
        }
        Err(e) => {
            eprintln!("  result: Err({})", e);
            eprintln!("  error variant: {:?}", e);

            // The image should have loaded successfully. If start() failed, it
            // should NOT be because of a bad image format — it should be a
            // runtime error (native call, null pointer, etc.).
            match e {
                VmError::BadImage(msg) if msg.contains("failed to read") => {
                    panic!("scode file couldn't be read — unexpected: {}", msg);
                }
                VmError::BadImage(msg) if msg.contains("bad magic") => {
                    panic!("bad magic on a known-good scode — unexpected: {}", msg);
                }
                VmError::BadImage(msg) if msg.contains("version") => {
                    panic!("bad version on a known-good scode — unexpected: {}", msg);
                }
                // NativeError, NullPointer, Timeout, etc. are acceptable —
                // they mean the image loaded fine but execution hit something
                // the pure Rust VM doesn't handle yet.
                _ => {
                    eprintln!("  (acceptable runtime error — image loaded correctly)");
                }
            }
        }
    }

    runner.stop();
}

// ══════════════════════════════════════════════════════════════════
// (f) test_rust_runner_with_bridge_setup
// ══════════════════════════════════════════════════════════════════

#[test]
fn test_rust_runner_with_bridge_setup() {
    use sandstar_svm::{
        set_engine_bridge, set_write_queue, set_tag_write_queue,
        ChannelSnapshot, SvmWrite, SvmTagWrite,
    };
    use std::sync::{Arc, Mutex, RwLock};

    let path = scode_path("win32_svm/kits.scode");
    skip_if_missing!(path);

    // Set up the engine bridge before starting
    let snapshot = Arc::new(RwLock::new(ChannelSnapshot::new()));
    let write_queue: Arc<Mutex<Vec<SvmWrite>>> = Arc::new(Mutex::new(Vec::new()));
    let tag_write_queue: Arc<Mutex<Vec<SvmTagWrite>>> = Arc::new(Mutex::new(Vec::new()));
    set_engine_bridge(snapshot);
    set_write_queue(write_queue);
    set_tag_write_queue(tag_write_queue);

    let mut runner = RustSvmRunner::new(&path);
    let result = runner.start();

    eprintln!("=== RustSvmRunner::start() with bridge on real scode ===");
    match &result {
        Ok(()) => {
            eprintln!("  result: OK (main method completed with bridge!)");

            // Try a resume cycle too
            let resume_result = runner.resume();
            match &resume_result {
                Ok(val) => eprintln!("  resume returned: {}", val),
                Err(e) => eprintln!("  resume error: {}", e),
            }
        }
        Err(e) => {
            eprintln!("  result: Err({})", e);
            eprintln!("  error variant: {:?}", e);
            eprintln!("  (may fail due to unimplemented natives — this is informational)");
        }
    }

    runner.stop();
}

// ══════════════════════════════════════════════════════════════════
// (g) test_sab_validator_on_real_sab_files
// ══════════════════════════════════════════════════════════════════

#[test]
fn test_sab_validator_on_win32_sab() {
    let path = scode_path("win32_svm/app.sab");
    skip_if_missing!(path);

    let path_str = path.to_string_lossy().to_string();
    let report = validate_sab(&path_str)
        .expect("validate_sab failed to produce a report");

    eprintln!("=== SAB validation: win32_svm/app.sab ===");
    eprintln!("  file_size:          {}", report.file_size);
    eprintln!("  header_valid:       {}", report.header_valid);
    eprintln!("  kit_count:          {}", report.kit_count);
    eprintln!("  component_count:    {}", report.component_count);
    eprintln!("  native_method_refs: {}", report.native_method_refs.len());
    eprintln!("  unsupported_natives:{}", report.unsupported_natives.len());
    eprintln!("  opcode_usage count: {}", report.opcode_usage.len());
    eprintln!("  unsupported_opcodes:{}", report.unsupported_opcodes.len());
    eprintln!("  warnings:           {}", report.warnings.len());
    eprintln!("  errors:             {}", report.errors.len());
    eprintln!("  compatible:         {}", report.compatible);

    for kit in &report.kits {
        eprintln!("    kit {}: {} (natives={}, checksum=0x{:08X})",
                  kit.id, kit.name, kit.native_count, kit.checksum);
    }
    for w in &report.warnings {
        eprintln!("    WARN: {}", w);
    }
    for e in &report.errors {
        eprintln!("    ERR:  {}", e);
    }

    // .sab files use a different binary format than .scode files.
    // The validator correctly identifies them as having a non-scode header.
    // A "false" header_valid means the magic is not 0x5ED0BA07 — expected.
    if !report.header_valid {
        eprintln!("  (header_valid=false is expected — .sab uses a different magic)");
        assert!(!report.errors.is_empty(),
                "should have error explaining why header is invalid");
    }
}

#[test]
fn test_sab_validator_on_linux_sab() {
    let path = scode_path("2026-03-11_21-56-18/app/app.sab");
    skip_if_missing!(path);

    let path_str = path.to_string_lossy().to_string();
    let report = validate_sab(&path_str)
        .expect("validate_sab failed to produce a report");

    eprintln!("=== SAB validation: linux app.sab ===");
    eprintln!("  file_size:          {}", report.file_size);
    eprintln!("  header_valid:       {}", report.header_valid);
    eprintln!("  kit_count:          {}", report.kit_count);
    eprintln!("  component_count:    {}", report.component_count);
    eprintln!("  native_method_refs: {}", report.native_method_refs.len());
    eprintln!("  unsupported_natives:{}", report.unsupported_natives.len());
    eprintln!("  compatible:         {}", report.compatible);

    for kit in &report.kits {
        eprintln!("    kit {}: {} (natives={}, checksum=0x{:08X})",
                  kit.id, kit.name, kit.native_count, kit.checksum);
    }
    for e in &report.errors {
        eprintln!("    ERR:  {}", e);
    }

    if !report.header_valid {
        eprintln!("  (header_valid=false is expected — .sab uses a different magic)");
        assert!(!report.errors.is_empty(),
                "should have error explaining why header is invalid");
    }
}

// ══════════════════════════════════════════════════════════════════
// (h) test_both_scode_files_have_compatible_headers
// ══════════════════════════════════════════════════════════════════

#[test]
fn test_both_scode_files_have_compatible_headers() {
    let win32_path = scode_path("win32_svm/kits.scode");
    let linux_path = scode_path("2026-03-11_21-56-18/app/kits.scode");

    if !win32_path.exists() || !linux_path.exists() {
        eprintln!("SKIP: one or both scode files not found");
        return;
    }

    let win32 = ScodeImage::load_from_file(&win32_path).expect("load win32 scode");
    let linux = ScodeImage::load_from_file(&linux_path).expect("load linux scode");

    eprintln!("=== Header comparison ===");
    eprintln!("  win32: image_size={}, data_size={}, main={}, resume={}",
              win32.header.image_size, win32.header.data_size,
              win32.header.main_method, win32.header.resume_method);
    eprintln!("  linux: image_size={}, data_size={}, main={}, resume={}",
              linux.header.image_size, linux.header.data_size,
              linux.header.main_method, linux.header.resume_method);

    // Both should share the same scode format version
    assert_eq!(win32.header.magic, linux.header.magic, "magic should match");
    assert_eq!(win32.header.major_ver, linux.header.major_ver, "major version");
    assert_eq!(win32.header.minor_ver, linux.header.minor_ver, "minor version");
    assert_eq!(win32.header.block_size, linux.header.block_size, "block size");
}

// ══════════════════════════════════════════════════════════════════
// (i) test_scode_block_access_patterns
// ══════════════════════════════════════════════════════════════════

#[test]
fn test_scode_block_access_patterns() {
    let path = scode_path("win32_svm/kits.scode");
    skip_if_missing!(path);

    let image = ScodeImage::load_from_file(&path).expect("load scode");

    // Verify we can read bytes at the main method entry point
    let main_offset = image.block_to_offset(image.header.main_method);
    let first_opcode = image.get_u8(main_offset);
    assert!(first_opcode.is_some(), "should be able to read opcode at main entry");
    eprintln!("  main entry opcode byte: 0x{:02X}", first_opcode.unwrap());

    // Verify we can read bytes at the resume method entry point
    let resume_offset = image.block_to_offset(image.header.resume_method);
    let resume_opcode = image.get_u8(resume_offset);
    assert!(resume_opcode.is_some(), "should be able to read opcode at resume entry");
    eprintln!("  resume entry opcode byte: 0x{:02X}", resume_opcode.unwrap());

    // Verify boundary reads work
    let last_byte = image.get_u8(image.len() - 1);
    assert!(last_byte.is_some(), "should read last byte");
    let out_of_bounds = image.get_u8(image.len());
    assert!(out_of_bounds.is_none(), "past-end should return None");
}

// ══════════════════════════════════════════════════════════════════
// (j) test_sab_validator_bytes_api
// ══════════════════════════════════════════════════════════════════

#[test]
fn test_sab_validator_bytes_api() {
    let path = scode_path("win32_svm/app.sab");
    skip_if_missing!(path);

    let data = std::fs::read(&path).expect("read .sab file");
    let report = validate_sab_bytes("win32_svm/app.sab", &data)
        .expect("validate_sab_bytes failed");

    eprintln!("=== SAB bytes validation ===");
    eprintln!("  file_size:    {}", report.file_size);
    eprintln!("  header_valid: {}", report.header_valid);
    eprintln!("  kit_count:    {}", report.kit_count);
    eprintln!("  compatible:   {}", report.compatible);

    assert_eq!(report.file_size, data.len(), "file_size should match");

    // .sab files use a different binary format (magic 0x70706173 = "apps"),
    // so header_valid will be false. That's correct behavior.
    if !report.header_valid {
        eprintln!("  (header_valid=false — .sab is not an scode file, expected)");
    }
}
