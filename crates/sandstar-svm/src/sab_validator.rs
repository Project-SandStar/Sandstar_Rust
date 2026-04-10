//! .sab/.sax compatibility validation for the pure Rust VM.
//!
//! Validates that a Sedona Application Binary (.sab) or Sedona Application XML
//! (.sax) file can be loaded and executed by the pure Rust Sedona VM.
//!
//! # .sab File Format
//!
//! A .sab file uses the same scode header as .scode files (magic 0x5ED0BA07),
//! followed by the kit table, component tree, and bytecode. This module parses
//! the header and kit table, scans the bytecode for native method references
//! and opcode usage, and checks each against the Rust VM's capabilities.
//!
//! # .sax File Format
//!
//! A .sax file is the XML source representation of a Sedona application. We can
//! check component types and slot references, but cannot validate bytecode
//! (that requires compilation by sedonac).

use std::collections::HashMap;

use crate::image_loader::{ScodeHeader, SCODE_HEADER_SIZE};
use crate::native_table::NativeTable;
use crate::opcodes::{Opcode, NUM_OPCODES};

// ── Report types ─────────────────────────────────────

/// Information about a kit found in a .sab file.
#[derive(Debug, Clone)]
pub struct SabKitInfo {
    pub id: u8,
    pub name: String,
    pub checksum: u32,
    pub native_count: u16,
}

/// A reference to a native method found in the bytecode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeMethodRef {
    pub kit_id: u8,
    pub method_id: u8,
    pub kit_name: String,
}

/// Validation report for a .sab or .sax file.
#[derive(Debug, Clone)]
pub struct SabValidationReport {
    pub file_path: String,
    pub file_size: usize,
    pub header_valid: bool,
    pub scode_version: u32,
    pub kit_count: u8,
    pub kits: Vec<SabKitInfo>,
    pub component_count: u16,
    pub native_method_refs: Vec<NativeMethodRef>,
    pub unsupported_natives: Vec<NativeMethodRef>,
    pub opcode_usage: HashMap<u8, u32>,
    pub unsupported_opcodes: Vec<u8>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub compatible: bool,
}

impl SabValidationReport {
    fn new(file_path: &str, file_size: usize) -> Self {
        Self {
            file_path: file_path.to_string(),
            file_size,
            header_valid: false,
            scode_version: 0,
            kit_count: 0,
            kits: Vec::new(),
            component_count: 0,
            native_method_refs: Vec::new(),
            unsupported_natives: Vec::new(),
            opcode_usage: HashMap::new(),
            unsupported_opcodes: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
            compatible: false,
        }
    }
}

// ── Opcode metadata ──────────────────────────────────

/// Returns the size in bytes of the instruction (including the opcode byte)
/// for a given opcode. Returns None for invalid opcodes.
fn opcode_instruction_size(op: u8) -> Option<usize> {
    let opcode = Opcode::try_from(op).ok()?;
    Some(match opcode {
        // 1-byte instructions (opcode only)
        Opcode::Nop
        | Opcode::LoadIM1
        | Opcode::LoadI0
        | Opcode::LoadI1
        | Opcode::LoadI2
        | Opcode::LoadI3
        | Opcode::LoadI4
        | Opcode::LoadI5
        | Opcode::LoadL0
        | Opcode::LoadL1
        | Opcode::LoadF0
        | Opcode::LoadF1
        | Opcode::LoadD0
        | Opcode::LoadD1
        | Opcode::LoadNull
        | Opcode::LoadNullBool
        | Opcode::LoadNullFloat
        | Opcode::LoadNullDouble
        | Opcode::LoadParam0
        | Opcode::LoadParam1
        | Opcode::LoadParam2
        | Opcode::LoadParam3
        | Opcode::LoadLocal0
        | Opcode::LoadLocal1
        | Opcode::LoadLocal2
        | Opcode::LoadLocal3
        | Opcode::LoadLocal4
        | Opcode::LoadLocal5
        | Opcode::LoadLocal6
        | Opcode::LoadLocal7
        | Opcode::StoreLocal0
        | Opcode::StoreLocal1
        | Opcode::StoreLocal2
        | Opcode::StoreLocal3
        | Opcode::StoreLocal4
        | Opcode::StoreLocal5
        | Opcode::StoreLocal6
        | Opcode::StoreLocal7
        | Opcode::IntEq
        | Opcode::IntNotEq
        | Opcode::IntGt
        | Opcode::IntGtEq
        | Opcode::IntLt
        | Opcode::IntLtEq
        | Opcode::IntMul
        | Opcode::IntDiv
        | Opcode::IntMod
        | Opcode::IntAdd
        | Opcode::IntSub
        | Opcode::IntOr
        | Opcode::IntXor
        | Opcode::IntAnd
        | Opcode::IntNot
        | Opcode::IntNeg
        | Opcode::IntShiftL
        | Opcode::IntShiftR
        | Opcode::IntInc
        | Opcode::IntDec
        | Opcode::LongEq
        | Opcode::LongNotEq
        | Opcode::LongGt
        | Opcode::LongGtEq
        | Opcode::LongLt
        | Opcode::LongLtEq
        | Opcode::LongMul
        | Opcode::LongDiv
        | Opcode::LongMod
        | Opcode::LongAdd
        | Opcode::LongSub
        | Opcode::LongOr
        | Opcode::LongXor
        | Opcode::LongAnd
        | Opcode::LongNot
        | Opcode::LongNeg
        | Opcode::LongShiftL
        | Opcode::LongShiftR
        | Opcode::FloatEq
        | Opcode::FloatNotEq
        | Opcode::FloatGt
        | Opcode::FloatGtEq
        | Opcode::FloatLt
        | Opcode::FloatLtEq
        | Opcode::FloatMul
        | Opcode::FloatDiv
        | Opcode::FloatAdd
        | Opcode::FloatSub
        | Opcode::FloatNeg
        | Opcode::DoubleEq
        | Opcode::DoubleNotEq
        | Opcode::DoubleGt
        | Opcode::DoubleGtEq
        | Opcode::DoubleLt
        | Opcode::DoubleLtEq
        | Opcode::DoubleMul
        | Opcode::DoubleDiv
        | Opcode::DoubleAdd
        | Opcode::DoubleSub
        | Opcode::DoubleNeg
        | Opcode::IntToFloat
        | Opcode::IntToLong
        | Opcode::IntToDouble
        | Opcode::LongToInt
        | Opcode::LongToFloat
        | Opcode::LongToDouble
        | Opcode::FloatToInt
        | Opcode::FloatToLong
        | Opcode::FloatToDouble
        | Opcode::DoubleToInt
        | Opcode::DoubleToLong
        | Opcode::DoubleToFloat
        | Opcode::ObjEq
        | Opcode::ObjNotEq
        | Opcode::EqZero
        | Opcode::NotEqZero
        | Opcode::Pop
        | Opcode::Pop2
        | Opcode::Pop3
        | Opcode::Dup
        | Opcode::Dup2
        | Opcode::DupDown2
        | Opcode::DupDown3
        | Opcode::Dup2Down2
        | Opcode::Dup2Down3
        | Opcode::ReturnVoid
        | Opcode::ReturnPop
        | Opcode::ReturnPopWide
        | Opcode::LoadDataAddr => 1,

        // 2-byte instructions (opcode + u8)
        Opcode::LoadIntU1
        | Opcode::LoadParam
        | Opcode::LoadParamWide
        | Opcode::StoreParam
        | Opcode::StoreParamWide
        | Opcode::LoadLocal
        | Opcode::LoadLocalWide
        | Opcode::StoreLocal
        | Opcode::StoreLocalWide
        | Opcode::Jump
        | Opcode::JumpNonZero
        | Opcode::JumpZero
        | Opcode::Load8BitFieldU1
        | Opcode::Store8BitFieldU1
        | Opcode::Load16BitFieldU1
        | Opcode::Store16BitFieldU1
        | Opcode::Load32BitFieldU1
        | Opcode::Store32BitFieldU1
        | Opcode::Load64BitFieldU1
        | Opcode::Store64BitFieldU1
        | Opcode::LoadRefFieldU1
        | Opcode::StoreRefFieldU1
        | Opcode::LoadConstFieldU1
        | Opcode::LoadInlineFieldU1
        | Opcode::LoadParam0InlineFieldU1
        | Opcode::LoadDataInlineFieldU1
        | Opcode::Load8BitArray
        | Opcode::Store8BitArray
        | Opcode::Add8BitArray
        | Opcode::Load16BitArray
        | Opcode::Store16BitArray
        | Opcode::Add16BitArray
        | Opcode::Load32BitArray
        | Opcode::Store32BitArray
        | Opcode::Add32BitArray
        | Opcode::Load64BitArray
        | Opcode::Store64BitArray
        | Opcode::Add64BitArray
        | Opcode::LoadRefArray
        | Opcode::StoreRefArray
        | Opcode::AddRefArray
        | Opcode::LoadConstArray => 2,

        // 3-byte instructions (opcode + u16 or opcode + u8 + u8)
        Opcode::LoadIntU2
        | Opcode::LoadInt
        | Opcode::LoadFloat
        | Opcode::LoadLong
        | Opcode::LoadDouble
        | Opcode::LoadStr
        | Opcode::LoadBuf
        | Opcode::LoadType
        | Opcode::LoadSlot
        | Opcode::LoadDefine
        | Opcode::Load8BitFieldU2
        | Opcode::Store8BitFieldU2
        | Opcode::Load16BitFieldU2
        | Opcode::Store16BitFieldU2
        | Opcode::Load32BitFieldU2
        | Opcode::Store32BitFieldU2
        | Opcode::Load64BitFieldU2
        | Opcode::Store64BitFieldU2
        | Opcode::LoadRefFieldU2
        | Opcode::StoreRefFieldU2
        | Opcode::LoadConstFieldU2
        | Opcode::LoadInlineFieldU2
        | Opcode::LoadParam0InlineFieldU2
        | Opcode::LoadDataInlineFieldU2
        | Opcode::JumpFar
        | Opcode::JumpFarNonZero
        | Opcode::JumpFarZero
        | Opcode::JumpIntEq
        | Opcode::JumpIntNotEq
        | Opcode::JumpIntGt
        | Opcode::JumpIntGtEq
        | Opcode::JumpIntLt
        | Opcode::JumpIntLtEq
        | Opcode::Call
        | Opcode::InitVirt
        | Opcode::InitComp
        | Opcode::Assert
        | Opcode::MetaSlot
        | Opcode::LoadSlotId
        | Opcode::LoadConstStatic
        | Opcode::LoadParam0Call => 3,

        // 4-byte instructions
        Opcode::JumpFarIntEq
        | Opcode::JumpFarIntNotEq
        | Opcode::JumpFarIntGt
        | Opcode::JumpFarIntGtEq
        | Opcode::JumpFarIntLt
        | Opcode::JumpFarIntLtEq
        | Opcode::CallVirtual
        | Opcode::Foreach
        | Opcode::ForeachFar => 4,

        // CallNative/CallNativeWide/CallNativeVoid: opcode + u8 kit + u8 method + u8 nparams
        Opcode::CallNative | Opcode::CallNativeWide | Opcode::CallNativeVoid => 4,

        // Variable-length instructions
        Opcode::InitArray => 4,  // opcode + u8 + u16

        // 5-byte instructions
        Opcode::Load8BitFieldU4
        | Opcode::Store8BitFieldU4
        | Opcode::Load16BitFieldU4
        | Opcode::Store16BitFieldU4
        | Opcode::Load32BitFieldU4
        | Opcode::Store32BitFieldU4
        | Opcode::Load64BitFieldU4
        | Opcode::Store64BitFieldU4
        | Opcode::LoadRefFieldU4
        | Opcode::StoreRefFieldU4
        | Opcode::LoadInlineFieldU4
        | Opcode::LoadParam0InlineFieldU4
        | Opcode::LoadDataInlineFieldU4 => 5,

        // Switch is variable: opcode + u16(count) + count*u16(offsets)
        // We return 3 as the minimum (the caller must handle the variable part)
        Opcode::Switch => 3,

        // IR-only opcodes that never appear in scode images
        Opcode::SizeOf | Opcode::Cast | Opcode::LoadArrayLiteral => 1,
    })
}

/// Set of opcodes that are handled by the Rust VM interpreter.
/// Built by scanning the actual match arms in vm_interpreter.rs.
fn supported_opcodes() -> Vec<bool> {
    let mut supported = vec![false; NUM_OPCODES];
    // All opcodes that have handlers in vm_interpreter.rs
    let handled: &[Opcode] = &[
        Opcode::Nop,
        Opcode::LoadIM1,
        Opcode::LoadI0,
        Opcode::LoadI1,
        Opcode::LoadI2,
        Opcode::LoadI3,
        Opcode::LoadI4,
        Opcode::LoadI5,
        Opcode::LoadIntU1,
        Opcode::LoadIntU2,
        Opcode::LoadL0,
        Opcode::LoadL1,
        Opcode::LoadF0,
        Opcode::LoadF1,
        Opcode::LoadD0,
        Opcode::LoadD1,
        Opcode::LoadNull,
        Opcode::LoadNullBool,
        Opcode::LoadNullFloat,
        Opcode::LoadNullDouble,
        Opcode::LoadInt,
        Opcode::LoadFloat,
        Opcode::LoadLong,
        Opcode::LoadDouble,
        Opcode::LoadStr,
        Opcode::LoadBuf,
        Opcode::LoadType,
        Opcode::LoadSlot,
        Opcode::LoadParam0,
        Opcode::LoadParam1,
        Opcode::LoadParam2,
        Opcode::LoadParam3,
        Opcode::LoadParam,
        Opcode::LoadParamWide,
        Opcode::StoreParam,
        Opcode::StoreParamWide,
        Opcode::LoadLocal0,
        Opcode::LoadLocal1,
        Opcode::LoadLocal2,
        Opcode::LoadLocal3,
        Opcode::LoadLocal4,
        Opcode::LoadLocal5,
        Opcode::LoadLocal6,
        Opcode::LoadLocal7,
        Opcode::LoadLocal,
        Opcode::LoadLocalWide,
        Opcode::StoreLocal0,
        Opcode::StoreLocal1,
        Opcode::StoreLocal2,
        Opcode::StoreLocal3,
        Opcode::StoreLocal4,
        Opcode::StoreLocal5,
        Opcode::StoreLocal6,
        Opcode::StoreLocal7,
        Opcode::StoreLocal,
        Opcode::StoreLocalWide,
        Opcode::IntEq,
        Opcode::IntNotEq,
        Opcode::IntGt,
        Opcode::IntGtEq,
        Opcode::IntLt,
        Opcode::IntLtEq,
        Opcode::IntMul,
        Opcode::IntDiv,
        Opcode::IntMod,
        Opcode::IntAdd,
        Opcode::IntSub,
        Opcode::IntOr,
        Opcode::IntXor,
        Opcode::IntAnd,
        Opcode::IntNot,
        Opcode::IntNeg,
        Opcode::IntShiftL,
        Opcode::IntShiftR,
        Opcode::IntInc,
        Opcode::IntDec,
        Opcode::LongEq,
        Opcode::LongNotEq,
        Opcode::LongGt,
        Opcode::LongGtEq,
        Opcode::LongLt,
        Opcode::LongLtEq,
        Opcode::LongMul,
        Opcode::LongDiv,
        Opcode::LongMod,
        Opcode::LongAdd,
        Opcode::LongSub,
        Opcode::LongOr,
        Opcode::LongXor,
        Opcode::LongAnd,
        Opcode::LongNot,
        Opcode::LongNeg,
        Opcode::LongShiftL,
        Opcode::LongShiftR,
        Opcode::FloatEq,
        Opcode::FloatNotEq,
        Opcode::FloatGt,
        Opcode::FloatGtEq,
        Opcode::FloatLt,
        Opcode::FloatLtEq,
        Opcode::FloatMul,
        Opcode::FloatDiv,
        Opcode::FloatAdd,
        Opcode::FloatSub,
        Opcode::FloatNeg,
        Opcode::DoubleEq,
        Opcode::DoubleNotEq,
        Opcode::DoubleGt,
        Opcode::DoubleGtEq,
        Opcode::DoubleLt,
        Opcode::DoubleLtEq,
        Opcode::DoubleMul,
        Opcode::DoubleDiv,
        Opcode::DoubleAdd,
        Opcode::DoubleSub,
        Opcode::DoubleNeg,
        Opcode::IntToFloat,
        Opcode::IntToLong,
        Opcode::IntToDouble,
        Opcode::LongToInt,
        Opcode::LongToFloat,
        Opcode::LongToDouble,
        Opcode::FloatToInt,
        Opcode::FloatToLong,
        Opcode::FloatToDouble,
        Opcode::DoubleToInt,
        Opcode::DoubleToLong,
        Opcode::DoubleToFloat,
        Opcode::ObjEq,
        Opcode::ObjNotEq,
        Opcode::EqZero,
        Opcode::NotEqZero,
        Opcode::Pop,
        Opcode::Pop2,
        Opcode::Pop3,
        Opcode::Dup,
        Opcode::Dup2,
        Opcode::DupDown2,
        Opcode::DupDown3,
        Opcode::Dup2Down2,
        Opcode::Dup2Down3,
        Opcode::Jump,
        Opcode::JumpNonZero,
        Opcode::JumpZero,
        Opcode::Foreach,
        Opcode::JumpFar,
        Opcode::JumpFarNonZero,
        Opcode::JumpFarZero,
        Opcode::ForeachFar,
        Opcode::JumpIntEq,
        Opcode::JumpIntNotEq,
        Opcode::JumpIntGt,
        Opcode::JumpIntGtEq,
        Opcode::JumpIntLt,
        Opcode::JumpIntLtEq,
        Opcode::JumpFarIntEq,
        Opcode::JumpFarIntNotEq,
        Opcode::JumpFarIntGt,
        Opcode::JumpFarIntGtEq,
        Opcode::JumpFarIntLt,
        Opcode::JumpFarIntLtEq,
        Opcode::LoadDataAddr,
        Opcode::Load8BitFieldU1,
        Opcode::Load8BitFieldU2,
        Opcode::Load8BitFieldU4,
        Opcode::Load8BitArray,
        Opcode::Store8BitFieldU1,
        Opcode::Store8BitFieldU2,
        Opcode::Store8BitFieldU4,
        Opcode::Store8BitArray,
        Opcode::Add8BitArray,
        Opcode::Load16BitFieldU1,
        Opcode::Load16BitFieldU2,
        Opcode::Load16BitFieldU4,
        Opcode::Load16BitArray,
        Opcode::Store16BitFieldU1,
        Opcode::Store16BitFieldU2,
        Opcode::Store16BitFieldU4,
        Opcode::Store16BitArray,
        Opcode::Add16BitArray,
        Opcode::Load32BitFieldU1,
        Opcode::Load32BitFieldU2,
        Opcode::Load32BitFieldU4,
        Opcode::Load32BitArray,
        Opcode::Store32BitFieldU1,
        Opcode::Store32BitFieldU2,
        Opcode::Store32BitFieldU4,
        Opcode::Store32BitArray,
        Opcode::Add32BitArray,
        Opcode::Load64BitFieldU1,
        Opcode::Load64BitFieldU2,
        Opcode::Load64BitFieldU4,
        Opcode::Load64BitArray,
        Opcode::Store64BitFieldU1,
        Opcode::Store64BitFieldU2,
        Opcode::Store64BitFieldU4,
        Opcode::Store64BitArray,
        Opcode::Add64BitArray,
        Opcode::LoadRefFieldU1,
        Opcode::LoadRefFieldU2,
        Opcode::LoadRefFieldU4,
        Opcode::LoadRefArray,
        Opcode::StoreRefFieldU1,
        Opcode::StoreRefFieldU2,
        Opcode::StoreRefFieldU4,
        Opcode::StoreRefArray,
        Opcode::AddRefArray,
        Opcode::LoadConstFieldU1,
        Opcode::LoadConstFieldU2,
        Opcode::LoadConstStatic,
        Opcode::LoadConstArray,
        Opcode::LoadInlineFieldU1,
        Opcode::LoadInlineFieldU2,
        Opcode::LoadInlineFieldU4,
        Opcode::LoadParam0InlineFieldU1,
        Opcode::LoadParam0InlineFieldU2,
        Opcode::LoadParam0InlineFieldU4,
        Opcode::LoadDataInlineFieldU1,
        Opcode::LoadDataInlineFieldU2,
        Opcode::LoadDataInlineFieldU4,
        Opcode::LoadParam0Call,
        Opcode::Call,
        Opcode::CallVirtual,
        Opcode::CallNative,
        Opcode::CallNativeWide,
        Opcode::CallNativeVoid,
        Opcode::ReturnPop,
        Opcode::ReturnPopWide,
        Opcode::ReturnVoid,
        Opcode::InitArray,
        Opcode::InitVirt,
        Opcode::InitComp,
        Opcode::Assert,
        Opcode::Switch,
        Opcode::MetaSlot,
        Opcode::LoadSlotId,
        // IR-only opcodes — we handle them (as errors or no-ops)
        Opcode::LoadDefine,
        Opcode::SizeOf,
        Opcode::Cast,
        Opcode::LoadArrayLiteral,
    ];

    for &op in handled {
        supported[op as usize] = true;
    }
    supported
}

// ── Validation functions ─────────────────────────────

/// Validate a .sab (Sedona Application Binary) file for Rust VM compatibility.
///
/// Parses the scode header, scans for native method references and opcode usage,
/// and checks each against the Rust VM's native table and interpreter.
pub fn validate_sab(path: &str) -> Result<SabValidationReport, String> {
    let data = std::fs::read(path).map_err(|e| format!("failed to read {path}: {e}"))?;
    validate_sab_bytes(path, &data)
}

/// Validate .sab from raw bytes (useful for testing).
pub fn validate_sab_bytes(path: &str, data: &[u8]) -> Result<SabValidationReport, String> {
    let mut report = SabValidationReport::new(path, data.len());

    // 1. Parse and validate header
    if data.len() < SCODE_HEADER_SIZE {
        report.errors.push(format!(
            "file too short for scode header: {} bytes (need at least {})",
            data.len(),
            SCODE_HEADER_SIZE,
        ));
        return Ok(report);
    }

    let header = match ScodeHeader::parse(data) {
        Ok(h) => h,
        Err(e) => {
            report.errors.push(format!("header parse failed: {e}"));
            return Ok(report);
        }
    };

    match header.validate() {
        Ok(()) => {
            report.header_valid = true;
        }
        Err(e) => {
            report.errors.push(format!("header validation failed: {e}"));
            // Continue anyway to provide as much info as possible
        }
    }

    report.scode_version =
        ((header.major_ver as u32) << 8) | (header.minor_ver as u32);

    // 2. Scan for native method references and opcode usage
    let natives = NativeTable::with_defaults();
    let supported = supported_opcodes();

    let code_start = SCODE_HEADER_SIZE;
    let code_end = data.len();
    let mut pc = code_start;

    while pc < code_end {
        let op_byte = data[pc];

        // Track opcode usage
        *report.opcode_usage.entry(op_byte).or_insert(0) += 1;

        // Check if this is a valid opcode
        if op_byte >= NUM_OPCODES as u8 {
            if !report.unsupported_opcodes.contains(&op_byte) {
                report.unsupported_opcodes.push(op_byte);
            }
            // Can't determine instruction length, skip one byte
            pc += 1;
            continue;
        }

        // Check if opcode is supported
        if !supported[op_byte as usize] {
            if !report.unsupported_opcodes.contains(&op_byte) {
                report.unsupported_opcodes.push(op_byte);
            }
        }

        // Extract native method references from CallNative* opcodes
        if op_byte == Opcode::CallNative as u8
            || op_byte == Opcode::CallNativeWide as u8
            || op_byte == Opcode::CallNativeVoid as u8
        {
            if pc + 3 < code_end {
                let kit_id = data[pc + 1];
                let method_id = data[pc + 2];
                let kit_name = natives
                    .kit_name(kit_id)
                    .unwrap_or("unknown")
                    .to_string();

                let native_ref = NativeMethodRef {
                    kit_id,
                    method_id,
                    kit_name: kit_name.clone(),
                };

                if !report.native_method_refs.contains(&native_ref) {
                    report.native_method_refs.push(native_ref.clone());
                }

                // Check if we have a real implementation (not just a stub)
                if !natives.is_implemented(kit_id, method_id as u16) {
                    if !report.unsupported_natives.contains(&native_ref) {
                        report.unsupported_natives.push(native_ref);
                    }
                }
            }
        }

        // Advance PC by instruction size
        let size = opcode_instruction_size(op_byte).unwrap_or(1);

        // Special handling for Switch (variable length)
        if op_byte == Opcode::Switch as u8 && pc + 3 <= code_end {
            let num_entries =
                u16::from_le_bytes([data[pc + 1], data[pc + 2]]) as usize;
            pc += 3 + num_entries * 2;
        } else {
            pc += size;
        }
    }

    // 3. Generate warnings
    if !report.unsupported_natives.is_empty() {
        report.warnings.push(format!(
            "{} native method(s) are stubbed (return 0): {}",
            report.unsupported_natives.len(),
            report
                .unsupported_natives
                .iter()
                .map(|n| format!("{}:{}", n.kit_name, n.method_id))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if !report.unsupported_opcodes.is_empty() {
        report.errors.push(format!(
            "{} unsupported opcode(s): {:?}",
            report.unsupported_opcodes.len(),
            report.unsupported_opcodes,
        ));
    }

    // 4. Determine compatibility
    report.compatible = report.errors.is_empty() && report.header_valid;

    Ok(report)
}

/// Validate a .sax (Sedona Application XML) file.
///
/// Performs basic XML parsing to extract component types and check them against
/// the known manifest types. Since .sax is the XML source, we cannot validate
/// bytecode — that requires compilation by sedonac.
pub fn validate_sax(path: &str) -> Result<SabValidationReport, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("failed to read {path}: {e}"))?;
    validate_sax_str(path, &content)
}

/// Validate .sax from a string (useful for testing).
pub fn validate_sax_str(path: &str, content: &str) -> Result<SabValidationReport, String> {
    let mut report = SabValidationReport::new(path, content.len());

    // .sax is XML, not binary — header_valid doesn't apply
    report.header_valid = true;

    // Parse XML-like component references (simple regex-free approach)
    let mut comp_count: u16 = 0;
    let mut type_names: Vec<String> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // Look for <comp> elements: <comp name="..." type="kit::Type" ...>
        if trimmed.starts_with("<comp ") || trimmed.starts_with("<comp>") {
            comp_count += 1;

            // Extract type attribute
            if let Some(type_start) = trimmed.find("type=\"") {
                let rest = &trimmed[type_start + 6..];
                if let Some(type_end) = rest.find('"') {
                    let type_name = &rest[..type_end];
                    if !type_names.contains(&type_name.to_string()) {
                        type_names.push(type_name.to_string());
                    }
                }
            }
        }
    }

    report.component_count = comp_count;

    if comp_count == 0 && !content.is_empty() {
        report.warnings.push("no <comp> elements found in .sax file".into());
    }

    if !type_names.is_empty() {
        report.warnings.push(format!(
            "references {} distinct component type(s): {}",
            type_names.len(),
            type_names.join(", ")
        ));
    }

    // .sax files are always "compatible" in the sense that they need compilation
    // before they can be run. We can only flag potential issues.
    report.compatible = report.errors.is_empty();

    Ok(report)
}

// ══════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_loader::{
        SCODE_BLOCK_SIZE, SCODE_MAGIC, SCODE_MAJOR_VER, SCODE_MINOR_VER,
    };
    use crate::opcodes::Opcode;

    /// Build a minimal valid .sab-like scode image.
    fn make_valid_sab(code: &[u8]) -> Vec<u8> {
        let image_size = SCODE_HEADER_SIZE + code.len();
        let mut buf = vec![0u8; image_size];

        buf[0..4].copy_from_slice(&SCODE_MAGIC.to_le_bytes());
        buf[4] = SCODE_MAJOR_VER;
        buf[5] = SCODE_MINOR_VER;
        buf[6] = SCODE_BLOCK_SIZE;
        buf[7] = 4;
        buf[8..12].copy_from_slice(&(image_size as u32).to_le_bytes());
        buf[12..16].copy_from_slice(&256u32.to_le_bytes()); // data_size

        let main_block = (SCODE_HEADER_SIZE / SCODE_BLOCK_SIZE as usize) as u16;
        buf[16..18].copy_from_slice(&main_block.to_le_bytes());

        if !code.is_empty() {
            buf[SCODE_HEADER_SIZE..].copy_from_slice(code);
        }
        buf
    }

    // ── Header validation tests ──────────────────────

    #[test]
    fn validate_empty_sab_header() {
        let data = make_valid_sab(&[
            Opcode::LoadI0 as u8,
            Opcode::ReturnPop as u8,
        ]);
        let report = validate_sab_bytes("test.sab", &data).unwrap();
        assert!(report.header_valid, "header should be valid");
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        assert!(report.compatible, "should be compatible");
    }

    #[test]
    fn validate_truncated_header() {
        let data = vec![0u8; 16]; // too short
        let report = validate_sab_bytes("short.sab", &data).unwrap();
        assert!(!report.header_valid);
        assert!(!report.errors.is_empty());
        assert!(!report.compatible);
    }

    #[test]
    fn validate_bad_magic() {
        let mut data = make_valid_sab(&[Opcode::Nop as u8]);
        data[0] = 0xFF; // corrupt magic
        let report = validate_sab_bytes("badmagic.sab", &data).unwrap();
        assert!(!report.header_valid);
        assert!(!report.compatible);
    }

    // ── Opcode scanning tests ────────────────────────

    #[test]
    fn validate_opcode_usage_tracked() {
        let code = vec![
            Opcode::LoadI0 as u8,
            Opcode::LoadI1 as u8,
            Opcode::IntAdd as u8,
            Opcode::ReturnPop as u8,
        ];
        let data = make_valid_sab(&code);
        let report = validate_sab_bytes("test.sab", &data).unwrap();

        assert_eq!(
            report.opcode_usage.get(&(Opcode::LoadI0 as u8)),
            Some(&1)
        );
        assert_eq!(
            report.opcode_usage.get(&(Opcode::IntAdd as u8)),
            Some(&1)
        );
        assert!(report.unsupported_opcodes.is_empty());
        assert!(report.compatible);
    }

    #[test]
    fn validate_native_method_refs_detected() {
        // CallNative: opcode + kit_id + method_id + nparams
        let code = vec![
            Opcode::CallNative as u8,
            0, // kit 0 (sys)
            5, // method 5
            0, // nparams
            Opcode::ReturnPop as u8,
        ];
        let data = make_valid_sab(&code);
        let report = validate_sab_bytes("test.sab", &data).unwrap();

        assert!(!report.native_method_refs.is_empty());
        let first = &report.native_method_refs[0];
        assert_eq!(first.kit_id, 0);
        assert_eq!(first.method_id, 5);
        assert_eq!(first.kit_name, "sys");
    }

    #[test]
    fn validate_unsupported_native_flagged() {
        // CallNative to kit 100 (shaystack) method 5 — still a stub
        let code = vec![
            Opcode::CallNative as u8,
            100, // kit 100 (shaystack)
            5,   // method 5
            0,   // nparams
            Opcode::ReturnPop as u8,
        ];
        let data = make_valid_sab(&code);
        let report = validate_sab_bytes("test.sab", &data).unwrap();

        // shaystack methods are stubs, so should appear in unsupported_natives
        assert!(
            !report.unsupported_natives.is_empty(),
            "should flag stubbed shaystack method as unsupported"
        );
    }

    // ── All opcodes supported test ───────────────────

    #[test]
    fn validate_all_opcodes_supported() {
        // Check that all 240 opcodes in opcodes.rs have support entries
        let supported = supported_opcodes();
        let mut unsupported = Vec::new();
        for i in 0..NUM_OPCODES {
            if let Ok(op) = Opcode::try_from(i as u8) {
                if !supported[op as usize] {
                    unsupported.push(op);
                }
            }
        }
        assert!(
            unsupported.is_empty(),
            "the following opcodes lack interpreter support: {:?}",
            unsupported
        );
    }

    // ── Native table completeness test ───────────────

    #[test]
    fn validate_native_table_completeness() {
        let table = NativeTable::with_defaults();

        // Verify all expected kits are registered
        assert!(table.kit_count() >= 101, "should have kit 100");
        assert_eq!(table.kit_name(0), Some("sys"));
        assert_eq!(table.kit_name(2), Some("inet"));
        assert_eq!(table.kit_name(4), Some("EacIo"));
        assert_eq!(table.kit_name(9), Some("datetimeStd"));
        assert_eq!(table.kit_name(100), Some("shaystack"));

        // Verify method counts match expected
        assert_eq!(table.method_count(0), 60, "kit 0 (sys) should have 60 methods");
        assert_eq!(table.method_count(2), 17, "kit 2 (inet) should have 17 methods");
        assert_eq!(table.method_count(4), 23, "kit 4 (EacIo) should have 23 methods");
        assert_eq!(table.method_count(9), 3, "kit 9 (datetimeStd) should have 3 methods");
        assert_eq!(table.method_count(100), 28, "kit 100 (shaystack) should have 28 methods");

        // Verify that core kits have real implementations (not all stubs)
        assert!(
            table.implemented_count(0) >= 33,
            "kit 0 should have at least 33 real impls, got {}",
            table.implemented_count(0)
        );
        assert_eq!(
            table.implemented_count(4),
            22,
            "kit 4 should have 22 real impls"
        );
        assert_eq!(
            table.implemented_count(9),
            3,
            "kit 9 should have 3 real impls"
        );
    }

    // ── .sax validation tests ────────────────────────

    #[test]
    fn validate_sax_basic() {
        let sax = r#"<?xml version="1.0" encoding="UTF-8"?>
<app>
  <comp name="app" type="sys::App">
    <comp name="folder" type="sys::Folder">
      <comp name="add1" type="func::Add2">
      </comp>
    </comp>
  </comp>
</app>"#;
        let report = validate_sax_str("test.sax", sax).unwrap();
        assert_eq!(report.component_count, 3);
        assert!(report.compatible);
    }

    #[test]
    fn validate_sax_empty() {
        let report = validate_sax_str("empty.sax", "").unwrap();
        assert_eq!(report.component_count, 0);
        assert!(report.compatible);
    }

    #[test]
    fn validate_sax_no_comps() {
        let sax = "<app></app>";
        let report = validate_sax_str("nocomps.sax", sax).unwrap();
        assert_eq!(report.component_count, 0);
        assert!(!report.warnings.is_empty()); // should warn about no comps
    }

    // ── Opcode instruction size tests ────────────────

    #[test]
    fn opcode_size_nop_is_1() {
        assert_eq!(opcode_instruction_size(Opcode::Nop as u8), Some(1));
    }

    #[test]
    fn opcode_size_load_int_u1_is_2() {
        assert_eq!(opcode_instruction_size(Opcode::LoadIntU1 as u8), Some(2));
    }

    #[test]
    fn opcode_size_call_native_is_4() {
        assert_eq!(opcode_instruction_size(Opcode::CallNative as u8), Some(4));
    }

    #[test]
    fn opcode_size_call_is_3() {
        assert_eq!(opcode_instruction_size(Opcode::Call as u8), Some(3));
    }

    #[test]
    fn opcode_size_invalid_returns_none() {
        assert_eq!(opcode_instruction_size(255), None);
    }

    // ── Version encoding test ────────────────────────

    #[test]
    fn version_encoding() {
        let data = make_valid_sab(&[Opcode::ReturnVoid as u8]);
        let report = validate_sab_bytes("test.sab", &data).unwrap();
        // Major 1, minor 5 → 0x0105 = 261
        assert_eq!(report.scode_version, 0x0105);
    }

    // ── Multiple native refs deduplication ────────────

    #[test]
    fn native_refs_deduplicated() {
        let code = vec![
            Opcode::CallNative as u8, 0, 5, 0, // first call to sys:5
            Opcode::CallNative as u8, 0, 5, 0, // duplicate call
            Opcode::CallNative as u8, 0, 10, 1, // different method
            Opcode::ReturnPop as u8,
        ];
        let data = make_valid_sab(&code);
        let report = validate_sab_bytes("test.sab", &data).unwrap();
        // Should have exactly 2 unique refs (sys:5 and sys:10), not 3
        assert_eq!(report.native_method_refs.len(), 2);
    }

    // ── Report compatibility logic ───────────────────

    #[test]
    fn compatible_when_all_opcodes_supported() {
        let code = vec![
            Opcode::LoadI1 as u8,
            Opcode::LoadI2 as u8,
            Opcode::IntAdd as u8,
            Opcode::ReturnPop as u8,
        ];
        let data = make_valid_sab(&code);
        let report = validate_sab_bytes("test.sab", &data).unwrap();
        assert!(report.compatible);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn file_size_tracked() {
        let data = make_valid_sab(&[Opcode::Nop as u8]);
        let report = validate_sab_bytes("test.sab", &data).unwrap();
        assert_eq!(report.file_size, data.len());
    }
}
