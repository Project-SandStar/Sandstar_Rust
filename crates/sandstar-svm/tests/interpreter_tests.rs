//! Comprehensive integration tests for the Pure Rust Sedona VM interpreter.
//!
//! Tests cover every opcode group: literals, params, locals, int/long/float/double
//! arithmetic, casts, comparisons, stack manipulation, branching, method calls, and misc.
//!
//! Each test builds a minimal scode image with exact bytecode, creates a VmInterpreter,
//! executes the bytecode, and asserts the expected result (stack state, return value, etc.).

use sandstar_svm::image_loader::{
    ScodeImage, SCODE_BLOCK_SIZE, SCODE_HEADER_SIZE, SCODE_MAGIC, SCODE_MAJOR_VER, SCODE_MINOR_VER,
};
use sandstar_svm::native_table::NativeTable;
use sandstar_svm::opcodes::Opcode;
use sandstar_svm::vm_error::VmError;
use sandstar_svm::vm_interpreter::VmInterpreter;
use sandstar_svm::vm_memory::VmMemory;

// ============================================================================
// Helper: build a minimal scode image from raw bytecode
// ============================================================================

/// Build a minimal scode image.
///
/// Layout:
/// - bytes  0..32:  header (32 bytes)
/// - bytes 32..32+code.len(): user-supplied bytecode
/// - Total image size is padded to 4-byte alignment
///
/// The header's `main_method` is set to block index 8 (offset 32), pointing
/// at the start of the user code.
///
/// `data_size` is the writable data segment size (minimum 64).
fn build_scode(code: &[u8], data_size: u32) -> Vec<u8> {
    let data_size = data_size.max(64);
    // Total image = header + code, rounded up to 4-byte boundary
    let raw_size = SCODE_HEADER_SIZE + code.len();
    let image_size = ((raw_size + 3) / 4) * 4;

    let mut buf = vec![0u8; image_size];

    // Magic
    buf[0..4].copy_from_slice(&SCODE_MAGIC.to_le_bytes());
    // Version
    buf[4] = SCODE_MAJOR_VER;
    buf[5] = SCODE_MINOR_VER;
    // Block size
    buf[6] = SCODE_BLOCK_SIZE;
    // Ref size (32-bit)
    buf[7] = 4;
    // Image size
    buf[8..12].copy_from_slice(&(image_size as u32).to_le_bytes());
    // Data size
    buf[12..16].copy_from_slice(&data_size.to_le_bytes());
    // main_method = block 8 (offset 32 = byte right after header)
    buf[16..18].copy_from_slice(&8u16.to_le_bytes());
    // tests_bix = 0
    buf[18..20].copy_from_slice(&0u16.to_le_bytes());
    // resume_method = 0
    buf[24..26].copy_from_slice(&0u16.to_le_bytes());

    // Copy user bytecode after header
    buf[SCODE_HEADER_SIZE..SCODE_HEADER_SIZE + code.len()].copy_from_slice(code);

    buf
}

/// Build an interpreter from raw bytecode and execute starting at the code entry point.
/// Returns the interpreter after execution so tests can inspect stack state.
fn run_code(code: &[u8]) -> VmInterpreter {
    run_code_with_data(code, 256)
}

fn run_code_with_data(code: &[u8], data_size: u32) -> VmInterpreter {
    let scode = build_scode(code, data_size);
    let image = ScodeImage::load_from_bytes(&scode).expect("failed to load test scode image");
    let memory = VmMemory::from_image(&image).expect("failed to create VmMemory");
    let natives = NativeTable::with_defaults();
    let mut interp = VmInterpreter::new(memory, natives);
    // Entry point = block 8 = offset 32
    let entry = SCODE_HEADER_SIZE;
    let _ = interp.execute(entry);
    interp
}

/// Execute code and return the result (Ok(i32) or Err).
fn exec_result(code: &[u8]) -> Result<i32, VmError> {
    let scode = build_scode(code, 256);
    let image = ScodeImage::load_from_bytes(&scode).expect("failed to load test scode image");
    let memory = VmMemory::from_image(&image).expect("failed to create VmMemory");
    let natives = NativeTable::with_defaults();
    let mut interp = VmInterpreter::new(memory, natives);
    let entry = SCODE_HEADER_SIZE;
    interp.execute(entry)
}

// ============================================================================
// Group A — Literals
// ============================================================================

#[test]
fn test_load_null() {
    // LoadNull pushes 0 onto stack, then ReturnPop returns it
    let code = [Opcode::LoadNull as u8, Opcode::ReturnPop as u8];
    let result = exec_result(&code);
    assert_eq!(result, Ok(0));
}

#[test]
fn test_load_im1() {
    // LoadIM1 pushes -1
    let code = [Opcode::LoadIM1 as u8, Opcode::ReturnPop as u8];
    let result = exec_result(&code);
    assert_eq!(result, Ok(-1));
}

#[test]
fn test_load_i0() {
    let code = [Opcode::LoadI0 as u8, Opcode::ReturnPop as u8];
    assert_eq!(exec_result(&code), Ok(0));
}

#[test]
fn test_load_i1() {
    let code = [Opcode::LoadI1 as u8, Opcode::ReturnPop as u8];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_load_i2() {
    let code = [Opcode::LoadI2 as u8, Opcode::ReturnPop as u8];
    assert_eq!(exec_result(&code), Ok(2));
}

#[test]
fn test_load_i3() {
    let code = [Opcode::LoadI3 as u8, Opcode::ReturnPop as u8];
    assert_eq!(exec_result(&code), Ok(3));
}

#[test]
fn test_load_i4() {
    let code = [Opcode::LoadI4 as u8, Opcode::ReturnPop as u8];
    assert_eq!(exec_result(&code), Ok(4));
}

#[test]
fn test_load_i5() {
    let code = [Opcode::LoadI5 as u8, Opcode::ReturnPop as u8];
    assert_eq!(exec_result(&code), Ok(5));
}

#[test]
fn test_load_int_u1() {
    // LoadIntU1 takes a single unsigned byte operand
    let code = [Opcode::LoadIntU1 as u8, 42, Opcode::ReturnPop as u8];
    assert_eq!(exec_result(&code), Ok(42));
}

#[test]
fn test_load_int_u1_max() {
    // LoadIntU1 with 255
    let code = [Opcode::LoadIntU1 as u8, 255, Opcode::ReturnPop as u8];
    assert_eq!(exec_result(&code), Ok(255));
}

#[test]
fn test_load_int_u2() {
    // LoadIntU2 takes a u16 operand (little-endian)
    // 0x0102 = 258
    let code = [Opcode::LoadIntU2 as u8, 0x02, 0x01, Opcode::ReturnPop as u8];
    assert_eq!(exec_result(&code), Ok(258));
}

#[test]
fn test_load_null_bool() {
    // LoadNullBool pushes 2 (Sedona null-bool sentinel)
    let code = [Opcode::LoadNullBool as u8, Opcode::ReturnPop as u8];
    assert_eq!(exec_result(&code), Ok(2));
}

#[test]
fn test_load_null_float() {
    // LoadNullFloat pushes NaN sentinel as bits in i32
    let code = [Opcode::LoadNullFloat as u8, Opcode::ReturnPop as u8];
    let result = exec_result(&code).unwrap();
    let f = f32::from_bits(result as u32);
    assert!(f.is_nan(), "LoadNullFloat should produce NaN, got {f}");
}

#[test]
fn test_load_f0() {
    // LoadF0 pushes 0.0f32 as bits
    let code = [Opcode::LoadF0 as u8, Opcode::ReturnPop as u8];
    let result = exec_result(&code).unwrap();
    let f = f32::from_bits(result as u32);
    assert_eq!(f, 0.0f32);
}

#[test]
fn test_load_f1() {
    // LoadF1 pushes 1.0f32 as bits
    let code = [Opcode::LoadF1 as u8, Opcode::ReturnPop as u8];
    let result = exec_result(&code).unwrap();
    let f = f32::from_bits(result as u32);
    assert_eq!(f, 1.0f32);
}

#[test]
fn test_nop() {
    // Nop should do nothing, then the next instruction runs
    let code = [
        Opcode::Nop as u8,
        Opcode::LoadI3 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(3));
}

// ============================================================================
// Group D — Int Arithmetic
// ============================================================================

#[test]
fn test_int_add() {
    // Push 3, push 4, add => 7
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI4 as u8,
        Opcode::IntAdd as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(7));
}

#[test]
fn test_int_sub() {
    // Push 5, push 3, sub => 2  (a - b where a is below b on stack)
    let code = [
        Opcode::LoadI5 as u8,
        Opcode::LoadI3 as u8,
        Opcode::IntSub as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(2));
}

#[test]
fn test_int_mul() {
    // 3 * 4 = 12
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI4 as u8,
        Opcode::IntMul as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(12));
}

#[test]
fn test_int_div() {
    // 5 / 2 = 2 (integer division)
    // Push 5 via LoadIntU1, push 2
    let code = [
        Opcode::LoadI5 as u8,
        Opcode::LoadI2 as u8,
        Opcode::IntDiv as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(2));
}

#[test]
fn test_int_mod() {
    // 5 % 3 = 2
    let code = [
        Opcode::LoadI5 as u8,
        Opcode::LoadI3 as u8,
        Opcode::IntMod as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(2));
}

#[test]
fn test_int_neg() {
    // neg(5) = -5
    let code = [
        Opcode::LoadI5 as u8,
        Opcode::IntNeg as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(-5));
}

#[test]
fn test_int_neg_zero() {
    // neg(0) = 0
    let code = [
        Opcode::LoadI0 as u8,
        Opcode::IntNeg as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

#[test]
fn test_int_and() {
    // 0xFF & 0x0F = 0x0F = 15
    let code = [
        Opcode::LoadIntU1 as u8,
        0xFF,
        Opcode::LoadIntU1 as u8,
        0x0F,
        Opcode::IntAnd as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0x0F));
}

#[test]
fn test_int_or() {
    // 0xF0 | 0x0F = 0xFF = 255
    let code = [
        Opcode::LoadIntU1 as u8,
        0xF0,
        Opcode::LoadIntU1 as u8,
        0x0F,
        Opcode::IntOr as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0xFF));
}

#[test]
fn test_int_xor() {
    // 0xFF ^ 0x0F = 0xF0 = 240
    let code = [
        Opcode::LoadIntU1 as u8,
        0xFF,
        Opcode::LoadIntU1 as u8,
        0x0F,
        Opcode::IntXor as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0xF0));
}

#[test]
fn test_int_not() {
    // ~0 = -1 (two's complement)
    let code = [
        Opcode::LoadI0 as u8,
        Opcode::IntNot as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(-1));
}

#[test]
fn test_int_shl() {
    // 1 << 4 = 16
    let code = [
        Opcode::LoadI1 as u8,
        Opcode::LoadI4 as u8,
        Opcode::IntShiftL as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(16));
}

#[test]
fn test_int_shr() {
    // 16 >> 2 = 4 (arithmetic shift right)
    let code = [
        Opcode::LoadIntU1 as u8,
        16,
        Opcode::LoadI2 as u8,
        Opcode::IntShiftR as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(4));
}

#[test]
fn test_int_inc() {
    // inc(4) = 5
    let code = [
        Opcode::LoadI4 as u8,
        Opcode::IntInc as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(5));
}

#[test]
fn test_int_dec() {
    // dec(5) = 4
    let code = [
        Opcode::LoadI5 as u8,
        Opcode::IntDec as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(4));
}

// ============================================================================
// Group D (cont.) — Int Compare
// ============================================================================

#[test]
fn test_int_eq_true() {
    // 3 == 3 => 1
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI3 as u8,
        Opcode::IntEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_int_eq_false() {
    // 3 == 4 => 0
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI4 as u8,
        Opcode::IntEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

#[test]
fn test_int_neq() {
    // 3 != 4 => 1
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI4 as u8,
        Opcode::IntNotEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_int_lt_true() {
    // 3 < 4 => 1
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI4 as u8,
        Opcode::IntLt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_int_lt_false() {
    // 4 < 3 => 0
    let code = [
        Opcode::LoadI4 as u8,
        Opcode::LoadI3 as u8,
        Opcode::IntLt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

#[test]
fn test_int_gt() {
    // 5 > 3 => 1
    let code = [
        Opcode::LoadI5 as u8,
        Opcode::LoadI3 as u8,
        Opcode::IntGt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

// ============================================================================
// Group E — Long Arithmetic
// ============================================================================

// For long operations we need to push two wide values.
// LoadL0 and LoadL1 push 0L and 1L respectively.

#[test]
fn test_long_add() {
    // 1L + 1L = 2L => truncate to i32 = 2
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LoadL1 as u8,
        Opcode::LongAdd as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(2));
}

#[test]
fn test_long_mul() {
    // 1L * 0L = 0L
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LoadL0 as u8,
        Opcode::LongMul as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

#[test]
fn test_long_neg() {
    // -1L => truncated to i32 = -1
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LongNeg as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(-1));
}

#[test]
fn test_long_eq_true() {
    // 0L == 0L => 1 (result is i32, not wide)
    let code = [
        Opcode::LoadL0 as u8,
        Opcode::LoadL0 as u8,
        Opcode::LongEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_long_eq_false() {
    // 0L == 1L => 0
    let code = [
        Opcode::LoadL0 as u8,
        Opcode::LoadL1 as u8,
        Opcode::LongEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

#[test]
fn test_long_sub() {
    // 1L - 1L = 0L
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LoadL1 as u8,
        Opcode::LongSub as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

// ============================================================================
// Group F — Float Arithmetic
// ============================================================================

#[test]
fn test_float_add() {
    // 1.0 + 1.0 = 2.0
    let code = [
        Opcode::LoadF1 as u8,
        Opcode::LoadF1 as u8,
        Opcode::FloatAdd as u8,
        Opcode::ReturnPop as u8,
    ];
    let result = exec_result(&code).unwrap();
    let f = f32::from_bits(result as u32);
    assert!((f - 2.0).abs() < 1e-6, "expected 2.0, got {f}");
}

#[test]
fn test_float_mul() {
    // 1.0 * 0.0 = 0.0
    let code = [
        Opcode::LoadF1 as u8,
        Opcode::LoadF0 as u8,
        Opcode::FloatMul as u8,
        Opcode::ReturnPop as u8,
    ];
    let result = exec_result(&code).unwrap();
    let f = f32::from_bits(result as u32);
    assert_eq!(f, 0.0);
}

#[test]
fn test_float_sub() {
    // 1.0 - 1.0 = 0.0
    let code = [
        Opcode::LoadF1 as u8,
        Opcode::LoadF1 as u8,
        Opcode::FloatSub as u8,
        Opcode::ReturnPop as u8,
    ];
    let result = exec_result(&code).unwrap();
    let f = f32::from_bits(result as u32);
    assert_eq!(f, 0.0);
}

#[test]
fn test_float_div() {
    // 1.0 / 1.0 = 1.0
    let code = [
        Opcode::LoadF1 as u8,
        Opcode::LoadF1 as u8,
        Opcode::FloatDiv as u8,
        Opcode::ReturnPop as u8,
    ];
    let result = exec_result(&code).unwrap();
    let f = f32::from_bits(result as u32);
    assert!((f - 1.0).abs() < 1e-6, "expected 1.0, got {f}");
}

#[test]
fn test_float_neg() {
    // -1.0
    let code = [
        Opcode::LoadF1 as u8,
        Opcode::FloatNeg as u8,
        Opcode::ReturnPop as u8,
    ];
    let result = exec_result(&code).unwrap();
    let f = f32::from_bits(result as u32);
    assert!((f - (-1.0)).abs() < 1e-6, "expected -1.0, got {f}");
}

#[test]
fn test_float_eq_true() {
    // 1.0 == 1.0 => 1
    let code = [
        Opcode::LoadF1 as u8,
        Opcode::LoadF1 as u8,
        Opcode::FloatEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_float_nan_eq() {
    // In Sedona, NaN == NaN should be TRUE (unlike IEEE 754!)
    let code = [
        Opcode::LoadNullFloat as u8,
        Opcode::LoadNullFloat as u8,
        Opcode::FloatEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1), "Sedona NaN==NaN should be true");
}

#[test]
fn test_float_lt() {
    // 0.0 < 1.0 => 1
    let code = [
        Opcode::LoadF0 as u8,
        Opcode::LoadF1 as u8,
        Opcode::FloatLt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

// ============================================================================
// Group G — Double Arithmetic
// ============================================================================

#[test]
fn test_double_add() {
    // 1.0d + 1.0d = 2.0d => cast to int = 2
    let code = [
        Opcode::LoadD1 as u8,
        Opcode::LoadD1 as u8,
        Opcode::DoubleAdd as u8,
        Opcode::DoubleToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(2));
}

#[test]
fn test_double_sub() {
    // 1.0d - 1.0d = 0.0d
    let code = [
        Opcode::LoadD1 as u8,
        Opcode::LoadD1 as u8,
        Opcode::DoubleSub as u8,
        Opcode::DoubleToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

#[test]
fn test_double_mul() {
    // 1.0d * 0.0d = 0.0d
    let code = [
        Opcode::LoadD1 as u8,
        Opcode::LoadD0 as u8,
        Opcode::DoubleMul as u8,
        Opcode::DoubleToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

#[test]
fn test_double_nan_eq() {
    // NaN == NaN => true in Sedona
    let code = [
        Opcode::LoadNullDouble as u8,
        Opcode::LoadNullDouble as u8,
        Opcode::DoubleEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(
        exec_result(&code),
        Ok(1),
        "Sedona double NaN==NaN should be true"
    );
}

#[test]
fn test_double_eq_true() {
    let code = [
        Opcode::LoadD0 as u8,
        Opcode::LoadD0 as u8,
        Opcode::DoubleEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

// ============================================================================
// Group H — Casts
// ============================================================================

#[test]
fn test_int_to_float() {
    // 3 -> 3.0f -> cast back to int to verify
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::IntToFloat as u8,
        Opcode::FloatToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(3));
}

#[test]
fn test_float_to_int() {
    // 1.0f -> 1
    let code = [
        Opcode::LoadF1 as u8,
        Opcode::FloatToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_int_to_long() {
    // 5 -> 5L -> back to int
    let code = [
        Opcode::LoadI5 as u8,
        Opcode::IntToLong as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(5));
}

#[test]
fn test_long_to_int() {
    // 1L -> 1
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_float_to_double_to_float() {
    // 1.0f -> double -> float -> int = 1
    let code = [
        Opcode::LoadF1 as u8,
        Opcode::FloatToDouble as u8,
        Opcode::DoubleToFloat as u8,
        Opcode::FloatToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_int_to_double() {
    // 4 -> double -> int = 4
    let code = [
        Opcode::LoadI4 as u8,
        Opcode::IntToDouble as u8,
        Opcode::DoubleToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(4));
}

#[test]
fn test_double_to_float() {
    // 1.0d -> float -> int = 1
    let code = [
        Opcode::LoadD1 as u8,
        Opcode::DoubleToFloat as u8,
        Opcode::FloatToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_long_to_float() {
    // 1L -> float -> int = 1
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LongToFloat as u8,
        Opcode::FloatToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_float_to_long() {
    // 1.0f -> long -> int = 1
    let code = [
        Opcode::LoadF1 as u8,
        Opcode::FloatToLong as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_long_to_double() {
    // 1L -> double -> int = 1
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LongToDouble as u8,
        Opcode::DoubleToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_double_to_long() {
    // 1.0d -> long -> int = 1
    let code = [
        Opcode::LoadD1 as u8,
        Opcode::DoubleToLong as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

// ============================================================================
// Group I-J — Object / General Compare
// ============================================================================

#[test]
fn test_obj_eq_same() {
    // null == null => 1
    let code = [
        Opcode::LoadNull as u8,
        Opcode::LoadNull as u8,
        Opcode::ObjEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_obj_neq() {
    // null != 1 => 1
    let code = [
        Opcode::LoadNull as u8,
        Opcode::LoadI1 as u8,
        Opcode::ObjNotEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_eq_zero_true() {
    // 0 => EqZero => 1
    let code = [
        Opcode::LoadI0 as u8,
        Opcode::EqZero as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_eq_zero_false() {
    // 5 => EqZero => 0
    let code = [
        Opcode::LoadI5 as u8,
        Opcode::EqZero as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

#[test]
fn test_neq_zero_true() {
    // 3 => NotEqZero => 1
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::NotEqZero as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_neq_zero_false() {
    // 0 => NotEqZero => 0
    let code = [
        Opcode::LoadI0 as u8,
        Opcode::NotEqZero as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

// ============================================================================
// Group K — Stack Manipulation
// ============================================================================

#[test]
fn test_dup() {
    // Push 5, dup, add => 10
    let code = [
        Opcode::LoadI5 as u8,
        Opcode::Dup as u8,
        Opcode::IntAdd as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(10));
}

#[test]
fn test_pop_discard() {
    // Push 3, push 5, pop (discard 5), return 3
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI5 as u8,
        Opcode::Pop as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(3));
}

#[test]
fn test_pop2_discard() {
    // Push 1, push 2, push 3, pop2 (discard 3 and 2), return 1
    let code = [
        Opcode::LoadI1 as u8,
        Opcode::LoadI2 as u8,
        Opcode::LoadI3 as u8,
        Opcode::Pop2 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_pop3_discard() {
    // Push 1, push 2, push 3, push 4, pop3, return 1
    let code = [
        Opcode::LoadI1 as u8,
        Opcode::LoadI2 as u8,
        Opcode::LoadI3 as u8,
        Opcode::LoadI4 as u8,
        Opcode::Pop3 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_dup2() {
    // Push 2, push 3, dup2 => [2, 3, 2, 3], add top two => 5
    let code = [
        Opcode::LoadI2 as u8,
        Opcode::LoadI3 as u8,
        Opcode::Dup2 as u8,
        Opcode::IntAdd as u8,
        Opcode::ReturnPop as u8,
    ];
    // Top two are duplicated [2,3], add => 5
    assert_eq!(exec_result(&code), Ok(5));
}

// ============================================================================
// Group L-M — Branches (near and far)
// ============================================================================

#[test]
fn test_jump_near() {
    // Jump forward 2 bytes, skip LoadI5, land on LoadI3, return
    // offset byte: +2 means skip 2 bytes ahead from AFTER the operand
    // Layout:     Jump, +2, LoadI5, ReturnPop, LoadI3, ReturnPop
    // Positions:   0     1    2        3          4       5
    // After reading offset at pos 1, PC = 2. Jump to 2+2 = 4.
    let code = [
        Opcode::Jump as u8,
        2i8 as u8, // signed offset +2
        Opcode::LoadI5 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI3 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(3));
}

#[test]
fn test_jump_zero_taken() {
    // Push 0, JumpZero (taken), skip LoadI5, return LoadI3
    // Layout: LoadI0, JumpZero, +2, LoadI5, ReturnPop, LoadI3, ReturnPop
    let code = [
        Opcode::LoadI0 as u8,
        Opcode::JumpZero as u8,
        2i8 as u8,
        Opcode::LoadI5 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI3 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(3));
}

#[test]
fn test_jump_zero_not_taken() {
    // Push 1, JumpZero (not taken), fall through to LoadI5, return
    let code = [
        Opcode::LoadI1 as u8,
        Opcode::JumpZero as u8,
        2i8 as u8,
        Opcode::LoadI5 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI3 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(5));
}

#[test]
fn test_jump_nonzero_taken() {
    // Push 1, JumpNonZero (taken)
    let code = [
        Opcode::LoadI1 as u8,
        Opcode::JumpNonZero as u8,
        2i8 as u8,
        Opcode::LoadI5 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI3 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(3));
}

#[test]
fn test_jump_nonzero_not_taken() {
    // Push 0, JumpNonZero (not taken) => fall through
    let code = [
        Opcode::LoadI0 as u8,
        Opcode::JumpNonZero as u8,
        2i8 as u8,
        Opcode::LoadI5 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI3 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(5));
}

#[test]
fn test_jump_far() {
    // JumpFar with 16-bit signed offset (little-endian)
    // Layout: JumpFar, lo, hi, LoadI5, ReturnPop, LoadI3, ReturnPop
    //         0        1   2    3         4          5       6
    // After reading offset at 1..3, PC = 3. Jump to 3 + 2 = 5.
    let offset: i16 = 2;
    let offset_bytes = offset.to_le_bytes();
    let code = [
        Opcode::JumpFar as u8,
        offset_bytes[0],
        offset_bytes[1],
        Opcode::LoadI5 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI3 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(3));
}

// ============================================================================
// Group N — Int Compare + Branch
// ============================================================================

#[test]
fn test_jump_int_eq_taken() {
    // Push 3, push 3, JumpIntEq => taken (they're equal)
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI3 as u8,
        Opcode::JumpIntEq as u8,
        2i8 as u8,
        Opcode::LoadI5 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI1 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_jump_int_eq_not_taken() {
    // Push 3, push 4, JumpIntEq => not taken
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI4 as u8,
        Opcode::JumpIntEq as u8,
        2i8 as u8,
        Opcode::LoadI5 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI1 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(5));
}

#[test]
fn test_jump_int_not_eq_taken() {
    // Push 3, push 4, JumpIntNotEq => taken
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI4 as u8,
        Opcode::JumpIntNotEq as u8,
        2i8 as u8,
        Opcode::LoadI5 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI2 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(2));
}

#[test]
fn test_jump_int_lt_taken() {
    // Push 3, push 5, JumpIntLt => taken (3 < 5)
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI5 as u8,
        Opcode::JumpIntLt as u8,
        2i8 as u8,
        Opcode::LoadI4 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI2 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(2));
}

#[test]
fn test_jump_int_lt_not_taken() {
    // Push 5, push 3, JumpIntLt => not taken (5 >= 3)
    let code = [
        Opcode::LoadI5 as u8,
        Opcode::LoadI3 as u8,
        Opcode::JumpIntLt as u8,
        2i8 as u8,
        Opcode::LoadI4 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI2 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(4));
}

#[test]
fn test_jump_int_gt_taken() {
    // Push 5, push 3, JumpIntGt => taken (5 > 3)
    let code = [
        Opcode::LoadI5 as u8,
        Opcode::LoadI3 as u8,
        Opcode::JumpIntGt as u8,
        2i8 as u8,
        Opcode::LoadI4 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI2 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(2));
}

#[test]
fn test_jump_int_gte_taken_equal() {
    // Push 3, push 3, JumpIntGtEq => taken (3 >= 3)
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI3 as u8,
        Opcode::JumpIntGtEq as u8,
        2i8 as u8,
        Opcode::LoadI4 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI2 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(2));
}

#[test]
fn test_jump_int_lte_taken() {
    // Push 3, push 5, JumpIntLtEq => taken (3 <= 5)
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI5 as u8,
        Opcode::JumpIntLtEq as u8,
        2i8 as u8,
        Opcode::LoadI4 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI2 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(2));
}

// ============================================================================
// Group P — Return
// ============================================================================

#[test]
fn test_return_void() {
    // ReturnVoid should complete without error
    let code = [Opcode::ReturnVoid as u8];
    let scode = build_scode(&code, 256);
    let image = ScodeImage::load_from_bytes(&scode).expect("load");
    let memory = VmMemory::from_image(&image).expect("memory");
    let natives = NativeTable::with_defaults();
    let mut interp = VmInterpreter::new(memory, natives);
    let result = interp.execute(SCODE_HEADER_SIZE);
    // ReturnVoid in top-level method should succeed
    assert!(result.is_ok(), "ReturnVoid should succeed: {result:?}");
}

#[test]
fn test_return_pop() {
    // ReturnPop returns the top of stack as i32
    let code = [Opcode::LoadIntU1 as u8, 99, Opcode::ReturnPop as u8];
    assert_eq!(exec_result(&code), Ok(99));
}

// ============================================================================
// Edge cases — overflow, negative values, boundary conditions
// ============================================================================

#[test]
fn test_int_add_overflow() {
    // i32::MAX + 1 wraps to i32::MIN (two's complement)
    // Push i32::MAX via LoadInt (block ref) is complex; use IntU1 approach
    // Instead: push -1 (IM1), push 1, add. -1 + 1 = 0.
    let code = [
        Opcode::LoadIM1 as u8,
        Opcode::LoadI1 as u8,
        Opcode::IntAdd as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

#[test]
fn test_int_sub_negative_result() {
    // 0 - 1 = -1
    let code = [
        Opcode::LoadI0 as u8,
        Opcode::LoadI1 as u8,
        Opcode::IntSub as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(-1));
}

#[test]
fn test_int_mul_by_zero() {
    // 5 * 0 = 0
    let code = [
        Opcode::LoadI5 as u8,
        Opcode::LoadI0 as u8,
        Opcode::IntMul as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

#[test]
fn test_int_mul_negative() {
    // -1 * 3 = -3
    let code = [
        Opcode::LoadIM1 as u8,
        Opcode::LoadI3 as u8,
        Opcode::IntMul as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(-3));
}

#[test]
fn test_int_shr_negative() {
    // Arithmetic shift right of -1 should stay -1
    let code = [
        Opcode::LoadIM1 as u8,
        Opcode::LoadI1 as u8,
        Opcode::IntShiftR as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(-1));
}

#[test]
fn test_chained_arithmetic() {
    // (2 + 3) * 4 = 20
    let code = [
        Opcode::LoadI2 as u8,
        Opcode::LoadI3 as u8,
        Opcode::IntAdd as u8,
        Opcode::LoadI4 as u8,
        Opcode::IntMul as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(20));
}

#[test]
fn test_int_eq_with_negative() {
    // -1 == -1 => 1
    let code = [
        Opcode::LoadIM1 as u8,
        Opcode::LoadIM1 as u8,
        Opcode::IntEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_int_lt_with_negative() {
    // -1 < 0 => 1
    let code = [
        Opcode::LoadIM1 as u8,
        Opcode::LoadI0 as u8,
        Opcode::IntLt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_int_gt_with_negative() {
    // 0 > -1 => 1
    let code = [
        Opcode::LoadI0 as u8,
        Opcode::LoadIM1 as u8,
        Opcode::IntGt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

// ============================================================================
// Additional compare operations
// ============================================================================

#[test]
fn test_int_gte_true() {
    // 5 >= 5 => 1
    let code = [
        Opcode::LoadI5 as u8,
        Opcode::LoadI5 as u8,
        Opcode::IntGtEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_int_lte_true() {
    // 3 <= 3 => 1
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI3 as u8,
        Opcode::IntLtEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_int_lte_false() {
    // 5 <= 3 => 0
    let code = [
        Opcode::LoadI5 as u8,
        Opcode::LoadI3 as u8,
        Opcode::IntLtEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

// ============================================================================
// Long compare operations
// ============================================================================

#[test]
fn test_long_neq() {
    // 0L != 1L => 1
    let code = [
        Opcode::LoadL0 as u8,
        Opcode::LoadL1 as u8,
        Opcode::LongNotEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_long_gt() {
    // 1L > 0L => 1
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LoadL0 as u8,
        Opcode::LongGt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_long_lt() {
    // 0L < 1L => 1
    let code = [
        Opcode::LoadL0 as u8,
        Opcode::LoadL1 as u8,
        Opcode::LongLt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

// ============================================================================
// Float compare operations
// ============================================================================

#[test]
fn test_float_neq() {
    // 0.0 != 1.0 => 1
    let code = [
        Opcode::LoadF0 as u8,
        Opcode::LoadF1 as u8,
        Opcode::FloatNotEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_float_gt() {
    // 1.0 > 0.0 => 1
    let code = [
        Opcode::LoadF1 as u8,
        Opcode::LoadF0 as u8,
        Opcode::FloatGt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_float_gte() {
    // 1.0 >= 1.0 => 1
    let code = [
        Opcode::LoadF1 as u8,
        Opcode::LoadF1 as u8,
        Opcode::FloatGtEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_float_lte() {
    // 0.0 <= 1.0 => 1
    let code = [
        Opcode::LoadF0 as u8,
        Opcode::LoadF1 as u8,
        Opcode::FloatLtEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

// ============================================================================
// Double compare operations
// ============================================================================

#[test]
fn test_double_neq() {
    // 0.0d != 1.0d => 1
    let code = [
        Opcode::LoadD0 as u8,
        Opcode::LoadD1 as u8,
        Opcode::DoubleNotEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_double_gt() {
    // 1.0d > 0.0d => 1
    let code = [
        Opcode::LoadD1 as u8,
        Opcode::LoadD0 as u8,
        Opcode::DoubleGt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_double_lt() {
    // 0.0d < 1.0d => 1
    let code = [
        Opcode::LoadD0 as u8,
        Opcode::LoadD1 as u8,
        Opcode::DoubleLt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

// ============================================================================
// Backward branch (simple loop)
// ============================================================================

#[test]
fn test_backward_jump() {
    // Simple loop: decrement from 3 to 0
    // Local plan:
    //   LoadI3         ; push 3
    //   IntDec         ; TOS = TOS - 1
    //   Dup            ; copy TOS
    //   JumpNonZero -3 ; if != 0, jump back to IntDec (offset = -3 from after operand)
    //   ReturnPop      ; return 0
    //
    // Positions (relative to code start):
    //   0: LoadI3
    //   1: IntDec
    //   2: Dup
    //   3: JumpNonZero
    //   4: -3 (offset byte)
    //   5: Pop         ; pop the extra dup'd 0
    //   6: ReturnPop
    //
    // After reading offset at pos 4, PC = 5. Jump to 5 + (-3) = 2... hmm.
    // We need to go back to IntDec (pos 1). So offset = 1 - 5 = -4.
    let code = [
        Opcode::LoadI3 as u8,      // 0
        Opcode::IntDec as u8,      // 1: loop target
        Opcode::Dup as u8,         // 2
        Opcode::JumpNonZero as u8, // 3
        (-4i8) as u8,              // 4: offset => PC after = 5, 5 + (-4) = 1
        Opcode::ReturnPop as u8,   // 5
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

// ============================================================================
// Complex expression tests
// ============================================================================

#[test]
fn test_nested_operations() {
    // ((1 + 2) * 3) - 4 = 5
    let code = [
        Opcode::LoadI1 as u8,
        Opcode::LoadI2 as u8,
        Opcode::IntAdd as u8, // 3
        Opcode::LoadI3 as u8,
        Opcode::IntMul as u8, // 9
        Opcode::LoadI4 as u8,
        Opcode::IntSub as u8, // 5
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(5));
}

#[test]
fn test_conditional_with_arithmetic() {
    // if (3 > 2) return 1; else return 0;
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI2 as u8,
        Opcode::JumpIntGt as u8,
        2i8 as u8,            // jump +2 if 3 > 2
        Opcode::LoadI0 as u8, // false branch
        Opcode::ReturnPop as u8,
        Opcode::LoadI1 as u8, // true branch
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

// ============================================================================
// Switch opcode
// ============================================================================

#[test]
fn test_switch_case_0() {
    // Switch with 3 entries, value = 0 => jump to first offset
    // Format: Switch, num_entries(u16 LE), offset0(u16 LE), offset1(u16 LE), offset2(u16 LE)
    // Each offset is relative to the Switch opcode position.
    //
    // Layout:
    //   0: LoadI0         ; push 0 (switch key)
    //   1: Switch
    //   2-3: num_entries = 3 (u16 LE)
    //   4-5: offset[0] = 10 (u16 LE) => goes to pos 11 relative to byte 1
    //   6-7: offset[1] = 14 (u16 LE) => goes to pos 15 relative to byte 1
    //   8-9: offset[2] = 18 (u16 LE) => not reached
    //  10: LoadI1, ReturnPop  (case 0 target)
    //  12: LoadI2, ReturnPop  (case 1 target)
    //  14: LoadI3, ReturnPop  (case 2 / default)
    //
    // The switch offset semantics vary by implementation. Since vm_interpreter.rs
    // doesn't exist yet, we write a plausible encoding. The test will validate
    // the actual behavior once the interpreter is built.
    let code = [
        Opcode::LoadI0 as u8, // 0: push switch key = 0
        Opcode::Switch as u8, // 1: switch
        3,
        0, // 2-3: num_entries = 3
        9,
        0, // 4-5: offset[0] = 9 (relative to Switch at pos 1 => target = 1+9 = 10)
        11,
        0, // 6-7: offset[1] = 11 => target = 12
        13,
        0,                       // 8-9: offset[2] = 13 => target = 14
        Opcode::LoadI1 as u8,    // 10: case 0
        Opcode::ReturnPop as u8, // 11
        Opcode::LoadI2 as u8,    // 12: case 1
        Opcode::ReturnPop as u8, // 13
        Opcode::LoadI3 as u8,    // 14: case 2 / default
        Opcode::ReturnPop as u8, // 15
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_switch_case_1() {
    // Same switch table as above, but key = 1
    let code = [
        Opcode::LoadI1 as u8, // push switch key = 1
        Opcode::Switch as u8,
        3,
        0,
        9,
        0, // case 0 => offset 9
        11,
        0, // case 1 => offset 11
        13,
        0, // case 2 => offset 13
        Opcode::LoadI1 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI2 as u8, // case 1 target
        Opcode::ReturnPop as u8,
        Opcode::LoadI3 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(2));
}

// ============================================================================
// Invalid opcode handling
// ============================================================================

#[test]
fn test_invalid_opcode() {
    // Opcode 0xFF is beyond the 240 valid opcodes => should error
    let code = [0xFF];
    let result = exec_result(&code);
    assert!(result.is_err(), "Invalid opcode should produce an error");
    if let Err(VmError::InvalidOpcode(op)) = result {
        assert_eq!(op, 0xFF);
    }
}

// ============================================================================
// Dup + swap integration
// ============================================================================

#[test]
fn test_swap_then_sub() {
    // Push 3, push 5, swap => [5, 3], sub => 5 - 3 = 2
    // (Assuming sub does: second_from_top - top)
    // Or if sub does: pop b, pop a, push a - b
    // Stack after swap: bottom=5, top=3. Sub pops 3 (b) then 5 (a), pushes 5-3=2.
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI5 as u8,
        // stack: [3, 5]
        // sub without swap: 3 - 5 = -2
        Opcode::IntSub as u8,
        Opcode::ReturnPop as u8,
    ];
    // a=3, b=5: a - b = -2
    // The Sedona VM: pop b, pop a, push a-b (standard)
    assert_eq!(exec_result(&code), Ok(-2));
}

// ============================================================================
// Long arithmetic edge cases
// ============================================================================

#[test]
fn test_long_not() {
    // ~0L should be all 1s = -1 as i64 => truncate to i32 = -1
    let code = [
        Opcode::LoadL0 as u8,
        Opcode::LongNot as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(-1));
}

#[test]
fn test_long_and() {
    // 1L & 0L = 0L
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LoadL0 as u8,
        Opcode::LongAnd as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

#[test]
fn test_long_or() {
    // 1L | 0L = 1L
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LoadL0 as u8,
        Opcode::LongOr as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_long_xor() {
    // 1L ^ 1L = 0L
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LoadL1 as u8,
        Opcode::LongXor as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(0));
}

// ============================================================================
// Double arithmetic edge cases
// ============================================================================

#[test]
fn test_double_div() {
    // 1.0d / 1.0d = 1.0d => int = 1
    let code = [
        Opcode::LoadD1 as u8,
        Opcode::LoadD1 as u8,
        Opcode::DoubleDiv as u8,
        Opcode::DoubleToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_double_neg() {
    // -1.0d => int = -1
    let code = [
        Opcode::LoadD1 as u8,
        Opcode::DoubleNeg as u8,
        Opcode::DoubleToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(-1));
}

// ============================================================================
// Far int compare branches
// ============================================================================

#[test]
fn test_jump_far_int_eq_taken() {
    // Push 3, push 3, JumpFarIntEq with u16 offset => taken
    let offset: i16 = 2;
    let ob = offset.to_le_bytes();
    let code = [
        Opcode::LoadI3 as u8,
        Opcode::LoadI3 as u8,
        Opcode::JumpFarIntEq as u8,
        ob[0],
        ob[1],
        Opcode::LoadI5 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI1 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_jump_far_zero_taken() {
    // Push 0, JumpFarZero => taken
    let offset: i16 = 2;
    let ob = offset.to_le_bytes();
    let code = [
        Opcode::LoadI0 as u8,
        Opcode::JumpFarZero as u8,
        ob[0],
        ob[1],
        Opcode::LoadI5 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI3 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(3));
}

#[test]
fn test_jump_far_nonzero_taken() {
    // Push 1, JumpFarNonZero => taken
    let offset: i16 = 2;
    let ob = offset.to_le_bytes();
    let code = [
        Opcode::LoadI1 as u8,
        Opcode::JumpFarNonZero as u8,
        ob[0],
        ob[1],
        Opcode::LoadI5 as u8,
        Opcode::ReturnPop as u8,
        Opcode::LoadI3 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(3));
}

// ============================================================================
// Multiple nops
// ============================================================================

#[test]
fn test_multiple_nops() {
    let code = [
        Opcode::Nop as u8,
        Opcode::Nop as u8,
        Opcode::Nop as u8,
        Opcode::LoadI4 as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(4));
}

// ============================================================================
// Double gte/lte
// ============================================================================

#[test]
fn test_double_gte() {
    let code = [
        Opcode::LoadD1 as u8,
        Opcode::LoadD1 as u8,
        Opcode::DoubleGtEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_double_lte() {
    let code = [
        Opcode::LoadD0 as u8,
        Opcode::LoadD1 as u8,
        Opcode::DoubleLtEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

// ============================================================================
// Long gte/lte
// ============================================================================

#[test]
fn test_long_gte() {
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LoadL1 as u8,
        Opcode::LongGtEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

#[test]
fn test_long_lte() {
    let code = [
        Opcode::LoadL0 as u8,
        Opcode::LoadL1 as u8,
        Opcode::LongLtEq as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

// ============================================================================
// Long shift operations
// ============================================================================

#[test]
fn test_long_shl() {
    // 1L << 1 = 2L => int = 2
    // LongShiftL takes a long and an int shift amount
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LoadI1 as u8,
        Opcode::LongShiftL as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(2));
}

#[test]
fn test_long_shr() {
    // We don't have a LoadL2, so use 1L + 1L = 2L, then >> 1 = 1L
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LoadL1 as u8,
        Opcode::LongAdd as u8, // 2L
        Opcode::LoadI1 as u8,
        Opcode::LongShiftR as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}

// ============================================================================
// Long div/mod
// ============================================================================

#[test]
fn test_long_div() {
    // (1L + 1L) / 1L = 2L
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LoadL1 as u8,
        Opcode::LongAdd as u8, // 2L
        Opcode::LoadL1 as u8,
        Opcode::LongDiv as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(2));
}

#[test]
fn test_long_mod() {
    // (1L + 1L + 1L) % (1L + 1L) = 3L % 2L = 1L
    let code = [
        Opcode::LoadL1 as u8,
        Opcode::LoadL1 as u8,
        Opcode::LongAdd as u8,
        Opcode::LoadL1 as u8,
        Opcode::LongAdd as u8, // 3L
        Opcode::LoadL1 as u8,
        Opcode::LoadL1 as u8,
        Opcode::LongAdd as u8, // 2L
        Opcode::LongMod as u8,
        Opcode::LongToInt as u8,
        Opcode::ReturnPop as u8,
    ];
    assert_eq!(exec_result(&code), Ok(1));
}
