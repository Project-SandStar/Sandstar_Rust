//! Pure Rust Sedona VM interpreter — complete opcode dispatch loop.
//!
//! This is a line-by-line port of `vm.c`'s `vmCall()` function.  Every opcode
//! in the Sedona bytecode set is handled, matching the C implementation's
//! semantics exactly (including the NaN-equality special case for floats/doubles).
//!
//! # Stack layout per call frame
//!
//! ```text
//!   stack temp 2  <- sp
//!   stack temp 1
//!   stack temp 0
//!   local n       <- sp here on start of call
//!   local 1
//!   local 0       <- locals_base (fp + 3)
//!   method addr   <- fp + 2  (stored as block index)
//!   prev fp       <- fp + 1  (stored as absolute stack index, 0 = none)
//!   return cp     <- fp       (stored as code offset, 0 = top-level)
//!   param n
//!   param 1
//!   param 0       <- pp = fp - num_params
//! ```

use crate::native_table::{NativeContext, NativeTable};
use crate::opcodes::Opcode;
use crate::vm_error::{VmError, VmResult};
use crate::vm_memory::VmMemory;
use crate::vm_stack::{CallFrame, VmStack};

/// Sentinel NaN for Sedona's `null` float value: 0x7FC00000.
const NULLFLOAT: i32 = 0x7fc0_0000_u32 as i32;

/// Sentinel NaN for Sedona's `null` double value: 0x7FF8000000000000.
const NULLDOUBLE: i64 = 0x7ff8_0000_0000_0000_u64 as i64;

/// Maximum number of instructions before we bail with a timeout.
/// (Prevents infinite loops during testing.)
const MAX_INSTRUCTIONS: u64 = 10_000_000;

/// The pure Rust Sedona VM interpreter.
pub struct VmInterpreter {
    pub stack: VmStack,
    pub memory: VmMemory,
    pub natives: NativeTable,
    /// Program counter — byte offset into the code segment.
    pub pc: usize,
    /// Set by `Sys.sleep` or external stop request.
    pub stopped: bool,
    /// Running count of assertion failures (for `Test` framework).
    pub assert_failures: i32,
    /// Running count of assertion successes.
    pub assert_successes: i32,
    /// Instruction counter for timeout detection.
    instruction_count: u64,
}

impl VmInterpreter {
    /// Create a new interpreter with the given memory and native table.
    pub fn new(memory: VmMemory, natives: NativeTable) -> Self {
        Self {
            stack: VmStack::new(4096),
            memory,
            natives,
            pc: 0,
            stopped: false,
            assert_failures: 0,
            assert_successes: 0,
            instruction_count: 0,
        }
    }

    /// Execute bytecode starting at `entry_point` until a top-level return.
    ///
    /// No method header is read — the PC begins exactly at `entry_point`.
    /// A sentinel call frame is set up so `ReturnPop`/`ReturnVoid` will
    /// end execution.
    pub fn execute(&mut self, entry_point: usize) -> VmResult<i32> {
        self.execute_with_args(entry_point, &[])
    }

    /// Execute bytecode at `entry_point`, pushing `args` as parameters first.
    pub fn execute_with_args(&mut self, entry_point: usize, args: &[i32]) -> VmResult<i32> {
        // Push args onto the stack
        for &arg in args {
            self.stack.push_i32(arg)?;
        }

        let num_params = args.len();

        // Push sentinel frame cells onto the stack
        self.stack.push_i32(0)?; // return cp = 0 (sentinel)
        self.stack.push_i32(0)?; // prev fp = 0 (sentinel)
        self.stack.push_i32(entry_point as i32)?; // method addr (for debug)

        let fp_index = self.stack.sp() - 3;

        self.stack.push_frame(CallFrame {
            return_pc: 0,
            frame_pointer: fp_index,
            method_block: 0,
            num_params: num_params as u8,
            num_locals: 0,
        })?;

        self.pc = entry_point;
        self.instruction_count = 0;

        self.run_loop()
    }

    /// Execute a **Sedona method** whose 2-byte header is at `entry_point`.
    ///
    /// Header: `[num_params: u8, num_locals: u8]` followed by opcodes.
    /// `args` must have exactly `num_params` elements.
    pub fn execute_method(&mut self, entry_point: usize, args: &[i32]) -> VmResult<i32> {
        let num_params = self.memory.code_u8(entry_point)? as usize;
        let num_locals = self.memory.code_u8(entry_point + 1)? as usize;

        if num_params != args.len() {
            return Err(VmError::BadImage(format!(
                "method at {} expects {} params, got {}",
                entry_point,
                num_params,
                args.len()
            )));
        }

        for &arg in args {
            self.stack.push_i32(arg)?;
        }

        self.stack.push_i32(0)?;
        self.stack.push_i32(0)?;
        self.stack.push_i32(entry_point as i32)?;

        let fp_index = self.stack.sp() - 3;

        self.stack.push_frame(CallFrame {
            return_pc: 0,
            frame_pointer: fp_index,
            method_block: 0,
            num_params: num_params as u8,
            num_locals: num_locals as u8,
        })?;

        for _ in 0..num_locals {
            self.stack.push_i32(0)?;
        }

        self.pc = entry_point + 2;
        self.instruction_count = 0;

        self.run_loop()
    }

    /// Main execution loop.
    fn run_loop(&mut self) -> VmResult<i32> {
        loop {
            if self.stopped {
                return Err(VmError::Stopped);
            }
            self.instruction_count += 1;
            if self.instruction_count > MAX_INSTRUCTIONS {
                return Err(VmError::Timeout);
            }
            match self.step()? {
                StepResult::Continue => {}
                StepResult::ReturnI32(val) => return Ok(val),
                StepResult::ReturnWide(_) => return Ok(0),
                StepResult::ReturnVoid => return Ok(0),
            }
        }
    }

    /// Execute one instruction. Returns whether execution should continue.
    pub fn step(&mut self) -> VmResult<StepResult> {
        let op_byte = self.memory.code_u8(self.pc)?;
        let op = Opcode::try_from(op_byte).map_err(VmError::InvalidOpcode)?;

        match op {
            // ==================================================================
            // Literals
            // ==================================================================
            Opcode::Nop => {
                self.pc += 1;
            }
            Opcode::LoadIM1 => {
                self.stack.push_i32(-1)?;
                self.pc += 1;
            }
            Opcode::LoadNull | Opcode::LoadI0 => {
                self.stack.push_i32(0)?;
                self.pc += 1;
            }
            Opcode::LoadI1 => {
                self.stack.push_i32(1)?;
                self.pc += 1;
            }
            Opcode::LoadNullBool | Opcode::LoadI2 => {
                self.stack.push_i32(2)?;
                self.pc += 1;
            }
            Opcode::LoadI3 => {
                self.stack.push_i32(3)?;
                self.pc += 1;
            }
            Opcode::LoadI4 => {
                self.stack.push_i32(4)?;
                self.pc += 1;
            }
            Opcode::LoadI5 => {
                self.stack.push_i32(5)?;
                self.pc += 1;
            }
            Opcode::LoadIntU1 => {
                let val = self.memory.code_u8(self.pc + 1)? as i32;
                self.stack.push_i32(val)?;
                self.pc += 2;
            }
            Opcode::LoadIntU2 => {
                let val = self.memory.code_u16(self.pc + 1)? as i32;
                self.stack.push_i32(val)?;
                self.pc += 3;
            }
            Opcode::LoadL0 => {
                self.stack.push_i64(0)?;
                self.pc += 1;
            }
            Opcode::LoadL1 => {
                self.stack.push_i64(1)?;
                self.pc += 1;
            }
            Opcode::LoadF0 => {
                self.stack.push_f32(0.0)?;
                self.pc += 1;
            }
            Opcode::LoadF1 => {
                self.stack.push_f32(1.0)?;
                self.pc += 1;
            }
            Opcode::LoadNullFloat => {
                self.stack.push_i32(NULLFLOAT)?;
                self.pc += 1;
            }
            Opcode::LoadNullDouble => {
                self.stack.push_i64(NULLDOUBLE)?;
                self.pc += 1;
            }
            Opcode::LoadD0 => {
                self.stack.push_f64(0.0)?;
                self.pc += 1;
            }
            Opcode::LoadD1 => {
                self.stack.push_f64(1.0)?;
                self.pc += 1;
            }
            // LoadInt / LoadFloat: read u16 block index, read i32 from code at that block address
            Opcode::LoadInt | Opcode::LoadFloat => {
                let bix = self.memory.code_u16(self.pc + 1)?;
                let addr = self.memory.block_to_addr(bix);
                let val = self.memory.code_i32(addr)?;
                self.stack.push_i32(val)?;
                self.pc += 3;
            }
            // LoadLong / LoadDouble: read u16 block index, read i64 from code at that block address
            Opcode::LoadLong | Opcode::LoadDouble => {
                let bix = self.memory.code_u16(self.pc + 1)?;
                let addr = self.memory.block_to_addr(bix);
                let lo = self.memory.code_u32(addr)?;
                let hi = self.memory.code_u32(addr + 4)?;
                let val = ((hi as u64) << 32 | lo as u64) as i64;
                self.stack.push_i64(val)?;
                self.pc += 3;
            }
            // LoadStr / LoadBuf / LoadType / LoadSlot: push code address as ref
            Opcode::LoadStr | Opcode::LoadBuf | Opcode::LoadType | Opcode::LoadSlot => {
                let bix = self.memory.code_u16(self.pc + 1)?;
                let addr = self.memory.block_to_addr(bix);
                self.stack.push_ref(addr)?;
                self.pc += 3;
            }

            // ==================================================================
            // Params
            // ==================================================================
            Opcode::LoadParam0 => {
                let pp = self.param_base()?;
                let val = self.stack.get(pp)?;
                self.stack.push_i32(val)?;
                self.pc += 1;
            }
            Opcode::LoadParam1 => {
                let pp = self.param_base()?;
                let val = self.stack.get(pp + 1)?;
                self.stack.push_i32(val)?;
                self.pc += 1;
            }
            Opcode::LoadParam2 => {
                let pp = self.param_base()?;
                let val = self.stack.get(pp + 2)?;
                self.stack.push_i32(val)?;
                self.pc += 1;
            }
            Opcode::LoadParam3 => {
                let pp = self.param_base()?;
                let val = self.stack.get(pp + 3)?;
                self.stack.push_i32(val)?;
                self.pc += 1;
            }
            Opcode::LoadParam => {
                let idx = self.memory.code_u8(self.pc + 1)? as usize;
                let pp = self.param_base()?;
                let val = self.stack.get(pp + idx)?;
                self.stack.push_i32(val)?;
                self.pc += 2;
            }
            Opcode::LoadParamWide => {
                let idx = self.memory.code_u8(self.pc + 1)? as usize;
                let pp = self.param_base()?;
                let val = self.stack.get_wide(pp + idx)?;
                self.stack.push_i64(val)?;
                self.pc += 2;
            }
            Opcode::StoreParam => {
                let idx = self.memory.code_u8(self.pc + 1)? as usize;
                let pp = self.param_base()?;
                let val = self.stack.pop_i32()?;
                self.stack.set(pp + idx, val)?;
                self.pc += 2;
            }
            Opcode::StoreParamWide => {
                let idx = self.memory.code_u8(self.pc + 1)? as usize;
                let pp = self.param_base()?;
                let val = self.stack.pop_i64()?;
                self.stack.set_wide(pp + idx, val)?;
                self.pc += 2;
            }

            // ==================================================================
            // Locals
            // ==================================================================
            Opcode::LoadLocal0 => {
                let lp = self.locals_base()?;
                let val = self.stack.get(lp)?;
                self.stack.push_i32(val)?;
                self.pc += 1;
            }
            Opcode::LoadLocal1 => {
                let lp = self.locals_base()?;
                let val = self.stack.get(lp + 1)?;
                self.stack.push_i32(val)?;
                self.pc += 1;
            }
            Opcode::LoadLocal2 => {
                let lp = self.locals_base()?;
                let val = self.stack.get(lp + 2)?;
                self.stack.push_i32(val)?;
                self.pc += 1;
            }
            Opcode::LoadLocal3 => {
                let lp = self.locals_base()?;
                let val = self.stack.get(lp + 3)?;
                self.stack.push_i32(val)?;
                self.pc += 1;
            }
            Opcode::LoadLocal4 => {
                let lp = self.locals_base()?;
                let val = self.stack.get(lp + 4)?;
                self.stack.push_i32(val)?;
                self.pc += 1;
            }
            Opcode::LoadLocal5 => {
                let lp = self.locals_base()?;
                let val = self.stack.get(lp + 5)?;
                self.stack.push_i32(val)?;
                self.pc += 1;
            }
            Opcode::LoadLocal6 => {
                let lp = self.locals_base()?;
                let val = self.stack.get(lp + 6)?;
                self.stack.push_i32(val)?;
                self.pc += 1;
            }
            Opcode::LoadLocal7 => {
                let lp = self.locals_base()?;
                let val = self.stack.get(lp + 7)?;
                self.stack.push_i32(val)?;
                self.pc += 1;
            }
            Opcode::LoadLocal => {
                let idx = self.memory.code_u8(self.pc + 1)? as usize;
                let lp = self.locals_base()?;
                let val = self.stack.get(lp + idx)?;
                self.stack.push_i32(val)?;
                self.pc += 2;
            }
            Opcode::LoadLocalWide => {
                let idx = self.memory.code_u8(self.pc + 1)? as usize;
                let lp = self.locals_base()?;
                let val = self.stack.get_wide(lp + idx)?;
                self.stack.push_i64(val)?;
                self.pc += 2;
            }
            Opcode::StoreLocal0 => {
                let lp = self.locals_base()?;
                let val = self.stack.pop_i32()?;
                self.stack.set(lp, val)?;
                self.pc += 1;
            }
            Opcode::StoreLocal1 => {
                let lp = self.locals_base()?;
                let val = self.stack.pop_i32()?;
                self.stack.set(lp + 1, val)?;
                self.pc += 1;
            }
            Opcode::StoreLocal2 => {
                let lp = self.locals_base()?;
                let val = self.stack.pop_i32()?;
                self.stack.set(lp + 2, val)?;
                self.pc += 1;
            }
            Opcode::StoreLocal3 => {
                let lp = self.locals_base()?;
                let val = self.stack.pop_i32()?;
                self.stack.set(lp + 3, val)?;
                self.pc += 1;
            }
            Opcode::StoreLocal4 => {
                let lp = self.locals_base()?;
                let val = self.stack.pop_i32()?;
                self.stack.set(lp + 4, val)?;
                self.pc += 1;
            }
            Opcode::StoreLocal5 => {
                let lp = self.locals_base()?;
                let val = self.stack.pop_i32()?;
                self.stack.set(lp + 5, val)?;
                self.pc += 1;
            }
            Opcode::StoreLocal6 => {
                let lp = self.locals_base()?;
                let val = self.stack.pop_i32()?;
                self.stack.set(lp + 6, val)?;
                self.pc += 1;
            }
            Opcode::StoreLocal7 => {
                let lp = self.locals_base()?;
                let val = self.stack.pop_i32()?;
                self.stack.set(lp + 7, val)?;
                self.pc += 1;
            }
            Opcode::StoreLocal => {
                let idx = self.memory.code_u8(self.pc + 1)? as usize;
                let lp = self.locals_base()?;
                let val = self.stack.pop_i32()?;
                self.stack.set(lp + idx, val)?;
                self.pc += 2;
            }
            Opcode::StoreLocalWide => {
                let idx = self.memory.code_u8(self.pc + 1)? as usize;
                let lp = self.locals_base()?;
                let val = self.stack.pop_i64()?;
                self.stack.set_wide(lp + idx, val)?;
                self.pc += 2;
            }

            // ==================================================================
            // Int compare
            // ==================================================================
            Opcode::IntEq => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(if a == b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::IntNotEq => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(if a != b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::IntGt => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(if a > b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::IntGtEq => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(if a >= b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::IntLt => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(if a < b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::IntLtEq => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(if a <= b { 1 } else { 0 })?;
                self.pc += 1;
            }

            // ==================================================================
            // Int math
            // ==================================================================
            Opcode::IntMul => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(a.wrapping_mul(b))?;
                self.pc += 1;
            }
            Opcode::IntDiv => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                if b == 0 {
                    self.stack.push_i32(0)?;
                } else {
                    self.stack.push_i32(a.wrapping_div(b))?;
                }
                self.pc += 1;
            }
            Opcode::IntMod => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                if b == 0 {
                    self.stack.push_i32(0)?;
                } else {
                    self.stack.push_i32(a.wrapping_rem(b))?;
                }
                self.pc += 1;
            }
            Opcode::IntAdd => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(a.wrapping_add(b))?;
                self.pc += 1;
            }
            Opcode::IntSub => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(a.wrapping_sub(b))?;
                self.pc += 1;
            }
            Opcode::IntOr => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(a | b)?;
                self.pc += 1;
            }
            Opcode::IntXor => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(a ^ b)?;
                self.pc += 1;
            }
            Opcode::IntAnd => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(a & b)?;
                self.pc += 1;
            }
            Opcode::IntNot => {
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(!a)?;
                self.pc += 1;
            }
            Opcode::IntNeg => {
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(a.wrapping_neg())?;
                self.pc += 1;
            }
            Opcode::IntShiftL => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(a.wrapping_shl(b as u32))?;
                self.pc += 1;
            }
            Opcode::IntShiftR => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                // C uses signed right shift (arithmetic)
                self.stack.push_i32(a.wrapping_shr(b as u32))?;
                self.pc += 1;
            }
            Opcode::IntInc => {
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(a.wrapping_add(1))?;
                self.pc += 1;
            }
            Opcode::IntDec => {
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(a.wrapping_sub(1))?;
                self.pc += 1;
            }

            // ==================================================================
            // Long compare
            // ==================================================================
            Opcode::LongEq => {
                let b = self.stack.pop_i64()?;
                let a = self.stack.pop_i64()?;
                self.stack.push_i32(if a == b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::LongNotEq => {
                let b = self.stack.pop_i64()?;
                let a = self.stack.pop_i64()?;
                self.stack.push_i32(if a != b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::LongGt => {
                let b = self.stack.pop_i64()?;
                let a = self.stack.pop_i64()?;
                self.stack.push_i32(if a > b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::LongGtEq => {
                let b = self.stack.pop_i64()?;
                let a = self.stack.pop_i64()?;
                self.stack.push_i32(if a >= b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::LongLt => {
                let b = self.stack.pop_i64()?;
                let a = self.stack.pop_i64()?;
                self.stack.push_i32(if a < b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::LongLtEq => {
                let b = self.stack.pop_i64()?;
                let a = self.stack.pop_i64()?;
                self.stack.push_i32(if a <= b { 1 } else { 0 })?;
                self.pc += 1;
            }

            // ==================================================================
            // Long math
            // ==================================================================
            Opcode::LongMul => {
                let b = self.stack.pop_i64()?;
                let a = self.stack.pop_i64()?;
                self.stack.push_i64(a.wrapping_mul(b))?;
                self.pc += 1;
            }
            Opcode::LongDiv => {
                let b = self.stack.pop_i64()?;
                let a = self.stack.pop_i64()?;
                if b == 0 {
                    self.stack.push_i64(0)?;
                } else {
                    self.stack.push_i64(a.wrapping_div(b))?;
                }
                self.pc += 1;
            }
            Opcode::LongMod => {
                let b = self.stack.pop_i64()?;
                let a = self.stack.pop_i64()?;
                if b == 0 {
                    self.stack.push_i64(0)?;
                } else {
                    self.stack.push_i64(a.wrapping_rem(b))?;
                }
                self.pc += 1;
            }
            Opcode::LongAdd => {
                let b = self.stack.pop_i64()?;
                let a = self.stack.pop_i64()?;
                self.stack.push_i64(a.wrapping_add(b))?;
                self.pc += 1;
            }
            Opcode::LongSub => {
                let b = self.stack.pop_i64()?;
                let a = self.stack.pop_i64()?;
                self.stack.push_i64(a.wrapping_sub(b))?;
                self.pc += 1;
            }
            Opcode::LongOr => {
                let b = self.stack.pop_i64()?;
                let a = self.stack.pop_i64()?;
                self.stack.push_i64(a | b)?;
                self.pc += 1;
            }
            Opcode::LongXor => {
                let b = self.stack.pop_i64()?;
                let a = self.stack.pop_i64()?;
                self.stack.push_i64(a ^ b)?;
                self.pc += 1;
            }
            Opcode::LongAnd => {
                let b = self.stack.pop_i64()?;
                let a = self.stack.pop_i64()?;
                self.stack.push_i64(a & b)?;
                self.pc += 1;
            }
            Opcode::LongNot => {
                let a = self.stack.pop_i64()?;
                self.stack.push_i64(!a)?;
                self.pc += 1;
            }
            Opcode::LongNeg => {
                let a = self.stack.pop_i64()?;
                self.stack.push_i64(a.wrapping_neg())?;
                self.pc += 1;
            }
            Opcode::LongShiftL => {
                let b = self.stack.pop_i32()?; // shift amount is int, not long
                let a = self.stack.pop_i64()?;
                self.stack.push_i64(a.wrapping_shl(b as u32))?;
                self.pc += 1;
            }
            Opcode::LongShiftR => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i64()?;
                self.stack.push_i64(a.wrapping_shr(b as u32))?;
                self.pc += 1;
            }

            // ==================================================================
            // Float compare (with Sedona NaN-equality special case)
            // ==================================================================
            Opcode::FloatEq => {
                let b = self.stack.pop_f32()?;
                let a = self.stack.pop_f32()?;
                // Sedona: NaN == NaN is true (unlike IEEE 754)
                let eq = (a.is_nan() && b.is_nan()) || a == b;
                self.stack.push_i32(i32::from(eq))?;
                self.pc += 1;
            }
            Opcode::FloatNotEq => {
                let b = self.stack.pop_f32()?;
                let a = self.stack.pop_f32()?;
                let result = if a.is_nan() && b.is_nan() {
                    0 // Sedona: NaN == NaN, so NaN != NaN is false
                } else if a != b {
                    1
                } else {
                    0
                };
                self.stack.push_i32(result)?;
                self.pc += 1;
            }
            Opcode::FloatGt => {
                let b = self.stack.pop_f32()?;
                let a = self.stack.pop_f32()?;
                self.stack.push_i32(if a > b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::FloatGtEq => {
                let b = self.stack.pop_f32()?;
                let a = self.stack.pop_f32()?;
                self.stack.push_i32(if a >= b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::FloatLt => {
                let b = self.stack.pop_f32()?;
                let a = self.stack.pop_f32()?;
                self.stack.push_i32(if a < b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::FloatLtEq => {
                let b = self.stack.pop_f32()?;
                let a = self.stack.pop_f32()?;
                self.stack.push_i32(if a <= b { 1 } else { 0 })?;
                self.pc += 1;
            }

            // ==================================================================
            // Float math
            // ==================================================================
            Opcode::FloatMul => {
                let b = self.stack.pop_f32()?;
                let a = self.stack.pop_f32()?;
                self.stack.push_f32(a * b)?;
                self.pc += 1;
            }
            Opcode::FloatDiv => {
                let b = self.stack.pop_f32()?;
                let a = self.stack.pop_f32()?;
                self.stack.push_f32(a / b)?;
                self.pc += 1;
            }
            Opcode::FloatAdd => {
                let b = self.stack.pop_f32()?;
                let a = self.stack.pop_f32()?;
                self.stack.push_f32(a + b)?;
                self.pc += 1;
            }
            Opcode::FloatSub => {
                let b = self.stack.pop_f32()?;
                let a = self.stack.pop_f32()?;
                self.stack.push_f32(a - b)?;
                self.pc += 1;
            }
            Opcode::FloatNeg => {
                let a = self.stack.pop_f32()?;
                self.stack.push_f32(-a)?;
                self.pc += 1;
            }

            // ==================================================================
            // Double compare (with Sedona NaN-equality special case)
            // ==================================================================
            Opcode::DoubleEq => {
                let b = self.stack.pop_f64()?;
                let a = self.stack.pop_f64()?;
                // Sedona: NaN == NaN is true (unlike IEEE 754)
                let eq = (a.is_nan() && b.is_nan()) || a == b;
                self.stack.push_i32(i32::from(eq))?;
                self.pc += 1;
            }
            Opcode::DoubleNotEq => {
                let b = self.stack.pop_f64()?;
                let a = self.stack.pop_f64()?;
                let result = if a.is_nan() && b.is_nan() {
                    0
                } else if a != b {
                    1
                } else {
                    0
                };
                self.stack.push_i32(result)?;
                self.pc += 1;
            }
            Opcode::DoubleGt => {
                let b = self.stack.pop_f64()?;
                let a = self.stack.pop_f64()?;
                self.stack.push_i32(if a > b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::DoubleGtEq => {
                let b = self.stack.pop_f64()?;
                let a = self.stack.pop_f64()?;
                self.stack.push_i32(if a >= b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::DoubleLt => {
                let b = self.stack.pop_f64()?;
                let a = self.stack.pop_f64()?;
                self.stack.push_i32(if a < b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::DoubleLtEq => {
                let b = self.stack.pop_f64()?;
                let a = self.stack.pop_f64()?;
                self.stack.push_i32(if a <= b { 1 } else { 0 })?;
                self.pc += 1;
            }

            // ==================================================================
            // Double math
            // ==================================================================
            Opcode::DoubleMul => {
                let b = self.stack.pop_f64()?;
                let a = self.stack.pop_f64()?;
                self.stack.push_f64(a * b)?;
                self.pc += 1;
            }
            Opcode::DoubleDiv => {
                let b = self.stack.pop_f64()?;
                let a = self.stack.pop_f64()?;
                self.stack.push_f64(a / b)?;
                self.pc += 1;
            }
            Opcode::DoubleAdd => {
                let b = self.stack.pop_f64()?;
                let a = self.stack.pop_f64()?;
                self.stack.push_f64(a + b)?;
                self.pc += 1;
            }
            Opcode::DoubleSub => {
                let b = self.stack.pop_f64()?;
                let a = self.stack.pop_f64()?;
                self.stack.push_f64(a - b)?;
                self.pc += 1;
            }
            Opcode::DoubleNeg => {
                let a = self.stack.pop_f64()?;
                self.stack.push_f64(-a)?;
                self.pc += 1;
            }

            // ==================================================================
            // Object compare
            // ==================================================================
            Opcode::ObjEq => {
                let b = self.stack.pop_ref()?;
                let a = self.stack.pop_ref()?;
                self.stack.push_i32(if a == b { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::ObjNotEq => {
                let b = self.stack.pop_ref()?;
                let a = self.stack.pop_ref()?;
                self.stack.push_i32(if a != b { 1 } else { 0 })?;
                self.pc += 1;
            }

            // ==================================================================
            // General purpose compare
            // ==================================================================
            Opcode::EqZero => {
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(if a == 0 { 1 } else { 0 })?;
                self.pc += 1;
            }
            Opcode::NotEqZero => {
                let a = self.stack.pop_i32()?;
                self.stack.push_i32(if a != 0 { 1 } else { 0 })?;
                self.pc += 1;
            }

            // ==================================================================
            // Casts
            // ==================================================================
            Opcode::IntToFloat => {
                let a = self.stack.pop_i32()?;
                self.stack.push_f32(a as f32)?;
                self.pc += 1;
            }
            Opcode::IntToLong => {
                let a = self.stack.pop_i32()?;
                self.stack.push_i64(a as i64)?;
                self.pc += 1;
            }
            Opcode::IntToDouble => {
                let a = self.stack.pop_i32()?;
                self.stack.push_f64(a as f64)?;
                self.pc += 1;
            }
            Opcode::LongToInt => {
                let a = self.stack.pop_i64()?;
                self.stack.push_i32(a as i32)?;
                self.pc += 1;
            }
            Opcode::LongToFloat => {
                let a = self.stack.pop_i64()?;
                self.stack.push_f32(a as f32)?;
                self.pc += 1;
            }
            Opcode::LongToDouble => {
                let a = self.stack.pop_i64()?;
                self.stack.push_f64(a as f64)?;
                self.pc += 1;
            }
            Opcode::FloatToInt => {
                let a = self.stack.pop_f32()?;
                self.stack.push_i32(a as i32)?;
                self.pc += 1;
            }
            Opcode::FloatToLong => {
                let a = self.stack.pop_f32()?;
                self.stack.push_i64(a as i64)?;
                self.pc += 1;
            }
            Opcode::FloatToDouble => {
                let a = self.stack.pop_f32()?;
                self.stack.push_f64(a as f64)?;
                self.pc += 1;
            }
            Opcode::DoubleToInt => {
                let a = self.stack.pop_f64()?;
                self.stack.push_i32(a as i32)?;
                self.pc += 1;
            }
            Opcode::DoubleToLong => {
                let a = self.stack.pop_f64()?;
                self.stack.push_i64(a as i64)?;
                self.pc += 1;
            }
            Opcode::DoubleToFloat => {
                let a = self.stack.pop_f64()?;
                self.stack.push_f32(a as f32)?;
                self.pc += 1;
            }

            // ==================================================================
            // Stack manipulation
            // ==================================================================
            Opcode::Pop => {
                self.stack.pop()?;
                self.pc += 1;
            }
            Opcode::Pop2 => {
                self.stack.pop2()?;
                self.pc += 1;
            }
            Opcode::Pop3 => {
                self.stack.pop3()?;
                self.pc += 1;
            }
            Opcode::Dup => {
                self.stack.dup()?;
                self.pc += 1;
            }
            Opcode::Dup2 => {
                self.stack.dup2()?;
                self.pc += 1;
            }
            Opcode::DupDown2 => {
                self.stack.dupdown2()?;
                self.pc += 1;
            }
            Opcode::DupDown3 => {
                self.stack.dupdown3()?;
                self.pc += 1;
            }
            Opcode::Dup2Down2 => {
                // sp += 2; *sp = *(sp-2); *(sp-1) = *(sp-3); *(sp-2) = *(sp-4); *(sp-3) = *sp; *(sp-4) = *(sp-1)
                // Read 4 values from top, duplicate top 2 and insert below 2
                let sp = self.stack.sp();
                if sp < 4 {
                    return Err(VmError::StackUnderflow);
                }
                let a = self.stack.get(sp - 4)?; // bottom
                let b = self.stack.get(sp - 3)?;
                let c = self.stack.get(sp - 2)?;
                let d = self.stack.get(sp - 1)?; // top
                                                 // Result: [c, d, a, b, c, d]
                self.stack.push_i32(0)?;
                self.stack.push_i32(0)?;
                let sp2 = self.stack.sp();
                self.stack.set(sp2 - 6, c)?;
                self.stack.set(sp2 - 5, d)?;
                self.stack.set(sp2 - 4, a)?;
                self.stack.set(sp2 - 3, b)?;
                self.stack.set(sp2 - 2, c)?;
                self.stack.set(sp2 - 1, d)?;
                self.pc += 1;
            }
            Opcode::Dup2Down3 => {
                // sp += 2; *sp = *(sp-2); *(sp-1) = *(sp-3); *(sp-2) = *(sp-4); *(sp-3) = *(sp-5); *(sp-4) = *sp; *(sp-5) = *(sp-1)
                let sp = self.stack.sp();
                if sp < 5 {
                    return Err(VmError::StackUnderflow);
                }
                let a = self.stack.get(sp - 5)?;
                let b = self.stack.get(sp - 4)?;
                let c = self.stack.get(sp - 3)?;
                let d = self.stack.get(sp - 2)?;
                let e = self.stack.get(sp - 1)?;
                // Result: [d, e, a, b, c, d, e]
                self.stack.push_i32(0)?;
                self.stack.push_i32(0)?;
                let sp2 = self.stack.sp();
                self.stack.set(sp2 - 7, d)?;
                self.stack.set(sp2 - 6, e)?;
                self.stack.set(sp2 - 5, a)?;
                self.stack.set(sp2 - 4, b)?;
                self.stack.set(sp2 - 3, c)?;
                self.stack.set(sp2 - 2, d)?;
                self.stack.set(sp2 - 1, e)?;
                self.pc += 1;
            }

            // ==================================================================
            // Near jumps (1-byte signed offset, relative to end of instruction)
            // Instruction size = 2 bytes (opcode + i8 offset)
            // ==================================================================
            Opcode::Jump => {
                let offset = self.memory.code_u8(self.pc + 1)? as i8;
                let base = self.pc + 2; // past the instruction
                self.pc = (base as isize + offset as isize) as usize;
            }
            Opcode::JumpNonZero => {
                let val = self.stack.pop_i32()?;
                if val != 0 {
                    let offset = self.memory.code_u8(self.pc + 1)? as i8;
                    let base = self.pc + 2;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 2;
                }
            }
            Opcode::JumpZero => {
                let val = self.stack.pop_i32()?;
                if val == 0 {
                    let offset = self.memory.code_u8(self.pc + 1)? as i8;
                    let base = self.pc + 2;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 2;
                }
            }
            Opcode::Foreach => {
                // Stack: [..., array_ref, length, counter]
                let sp = self.stack.sp();
                let counter = self.stack.get(sp - 1)?.wrapping_add(1);
                self.stack.set(sp - 1, counter)?;
                let length = self.stack.get(sp - 2)?;
                if counter >= length {
                    let offset = self.memory.code_u8(self.pc + 1)? as i8;
                    let base = self.pc + 2;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    let array_ref = self.stack.get(sp - 3)?;
                    self.stack.push_i32(array_ref)?;
                    self.stack.push_i32(counter)?;
                    self.pc += 2;
                }
            }

            // ==================================================================
            // Far jumps (2-byte signed offset, relative to end of instruction)
            // Instruction size = 3 bytes (opcode + i16 offset)
            // ==================================================================
            Opcode::JumpFar => {
                let offset = self.read_i16_at(self.pc + 1)?;
                let base = self.pc + 3;
                self.pc = (base as isize + offset as isize) as usize;
            }
            Opcode::JumpFarNonZero => {
                let val = self.stack.pop_i32()?;
                if val != 0 {
                    let offset = self.read_i16_at(self.pc + 1)?;
                    let base = self.pc + 3;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 3;
                }
            }
            Opcode::JumpFarZero => {
                let val = self.stack.pop_i32()?;
                if val == 0 {
                    let offset = self.read_i16_at(self.pc + 1)?;
                    let base = self.pc + 3;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 3;
                }
            }
            Opcode::ForeachFar => {
                let sp = self.stack.sp();
                let counter = self.stack.get(sp - 1)?.wrapping_add(1);
                self.stack.set(sp - 1, counter)?;
                let length = self.stack.get(sp - 2)?;
                if counter >= length {
                    let offset = self.read_i16_at(self.pc + 1)?;
                    let base = self.pc + 3;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    let array_ref = self.stack.get(sp - 3)?;
                    self.stack.push_i32(array_ref)?;
                    self.stack.push_i32(counter)?;
                    self.pc += 3;
                }
            }

            // ==================================================================
            // Int compare near branch (pop 2, compare, 1-byte offset from end)
            // Instruction size = 2 (opcode + i8)
            // ==================================================================
            Opcode::JumpIntEq => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                if a == b {
                    let offset = self.memory.code_u8(self.pc + 1)? as i8;
                    let base = self.pc + 2;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 2;
                }
            }
            Opcode::JumpIntNotEq => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                if a != b {
                    let offset = self.memory.code_u8(self.pc + 1)? as i8;
                    let base = self.pc + 2;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 2;
                }
            }
            Opcode::JumpIntGt => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                if a > b {
                    let offset = self.memory.code_u8(self.pc + 1)? as i8;
                    let base = self.pc + 2;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 2;
                }
            }
            Opcode::JumpIntGtEq => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                if a >= b {
                    let offset = self.memory.code_u8(self.pc + 1)? as i8;
                    let base = self.pc + 2;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 2;
                }
            }
            Opcode::JumpIntLt => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                if a < b {
                    let offset = self.memory.code_u8(self.pc + 1)? as i8;
                    let base = self.pc + 2;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 2;
                }
            }
            Opcode::JumpIntLtEq => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                if a <= b {
                    let offset = self.memory.code_u8(self.pc + 1)? as i8;
                    let base = self.pc + 2;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 2;
                }
            }

            // ==================================================================
            // Int compare far branch (pop 2, compare, 2-byte offset from end)
            // Instruction size = 3 (opcode + i16)
            // ==================================================================
            Opcode::JumpFarIntEq => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                if a == b {
                    let offset = self.read_i16_at(self.pc + 1)?;
                    let base = self.pc + 3;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 3;
                }
            }
            Opcode::JumpFarIntNotEq => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                if a != b {
                    let offset = self.read_i16_at(self.pc + 1)?;
                    let base = self.pc + 3;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 3;
                }
            }
            Opcode::JumpFarIntGt => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                if a > b {
                    let offset = self.read_i16_at(self.pc + 1)?;
                    let base = self.pc + 3;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 3;
                }
            }
            Opcode::JumpFarIntGtEq => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                if a >= b {
                    let offset = self.read_i16_at(self.pc + 1)?;
                    let base = self.pc + 3;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 3;
                }
            }
            Opcode::JumpFarIntLt => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                if a < b {
                    let offset = self.read_i16_at(self.pc + 1)?;
                    let base = self.pc + 3;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 3;
                }
            }
            Opcode::JumpFarIntLtEq => {
                let b = self.stack.pop_i32()?;
                let a = self.stack.pop_i32()?;
                if a <= b {
                    let offset = self.read_i16_at(self.pc + 1)?;
                    let base = self.pc + 3;
                    self.pc = (base as isize + offset as isize) as usize;
                } else {
                    self.pc += 3;
                }
            }

            // ==================================================================
            // Storage -- Load data base address
            // ==================================================================
            Opcode::LoadDataAddr => {
                // Push 0 as data base address (all data offsets are relative to 0)
                self.stack.push_ref(0)?;
                self.pc += 1;
            }

            // ==================================================================
            // 8-bit field load/store
            // ==================================================================
            Opcode::Load8BitFieldU1 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u8(self.pc + 1)? as i32;
                let val = self.memory.get_byte(base, offset)?;
                self.stack.push_i32(val as i32)?;
                self.pc += 2;
            }
            Opcode::Load8BitFieldU2 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u16(self.pc + 1)? as i32;
                let val = self.memory.get_byte(base, offset)?;
                self.stack.push_i32(val as i32)?;
                self.pc += 3;
            }
            Opcode::Load8BitFieldU4 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u32(self.pc + 1)? as i32;
                let val = self.memory.get_byte(base, offset)?;
                self.stack.push_i32(val as i32)?;
                self.pc += 5;
            }
            Opcode::Load8BitArray => {
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                let val = self.memory.get_byte(base, idx)?;
                self.stack.push_i32(val as i32)?;
                self.pc += 1;
            }
            Opcode::Store8BitFieldU1 => {
                let val = self.stack.pop_i32()? as u8;
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u8(self.pc + 1)? as i32;
                self.memory.set_byte(base, offset, val)?;
                self.pc += 2;
            }
            Opcode::Store8BitFieldU2 => {
                let val = self.stack.pop_i32()? as u8;
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u16(self.pc + 1)? as i32;
                self.memory.set_byte(base, offset, val)?;
                self.pc += 3;
            }
            Opcode::Store8BitFieldU4 => {
                let val = self.stack.pop_i32()? as u8;
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u32(self.pc + 1)? as i32;
                self.memory.set_byte(base, offset, val)?;
                self.pc += 5;
            }
            Opcode::Store8BitArray => {
                let val = self.stack.pop_i32()? as u8;
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                self.memory.set_byte(base, idx, val)?;
                self.pc += 1;
            }
            Opcode::Add8BitArray => {
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                self.stack.push_ref(base.wrapping_add(idx as usize))?;
                self.pc += 1;
            }

            // ==================================================================
            // 16-bit field load/store
            // ==================================================================
            Opcode::Load16BitFieldU1 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u8(self.pc + 1)? as i32;
                let val = self.memory.get_short(base, offset)?;
                self.stack.push_i32(val as i32)?;
                self.pc += 2;
            }
            Opcode::Load16BitFieldU2 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u16(self.pc + 1)? as i32;
                let val = self.memory.get_short(base, offset)?;
                self.stack.push_i32(val as i32)?;
                self.pc += 3;
            }
            Opcode::Load16BitFieldU4 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u32(self.pc + 1)? as i32;
                let val = self.memory.get_short(base, offset)?;
                self.stack.push_i32(val as i32)?;
                self.pc += 5;
            }
            Opcode::Load16BitArray => {
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                // Array indexing: base + idx * 2
                let addr = base.wrapping_add(idx as usize * 2);
                let val = self.memory.get_short(addr, 0)?;
                self.stack.push_i32(val as i32)?;
                self.pc += 1;
            }
            Opcode::Store16BitFieldU1 => {
                let val = self.stack.pop_i32()? as u16;
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u8(self.pc + 1)? as i32;
                self.memory.set_short(base, offset, val)?;
                self.pc += 2;
            }
            Opcode::Store16BitFieldU2 => {
                let val = self.stack.pop_i32()? as u16;
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u16(self.pc + 1)? as i32;
                self.memory.set_short(base, offset, val)?;
                self.pc += 3;
            }
            Opcode::Store16BitFieldU4 => {
                let val = self.stack.pop_i32()? as u16;
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u32(self.pc + 1)? as i32;
                self.memory.set_short(base, offset, val)?;
                self.pc += 5;
            }
            Opcode::Store16BitArray => {
                let val = self.stack.pop_i32()? as u16;
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                let addr = base.wrapping_add(idx as usize * 2);
                self.memory.set_short(addr, 0, val)?;
                self.pc += 1;
            }
            Opcode::Add16BitArray => {
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                self.stack.push_ref(base.wrapping_add(idx as usize * 2))?;
                self.pc += 1;
            }

            // ==================================================================
            // 32-bit field load/store
            // ==================================================================
            Opcode::Load32BitFieldU1 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u8(self.pc + 1)? as i32;
                let val = self.memory.get_int(base, offset)?;
                self.stack.push_i32(val)?;
                self.pc += 2;
            }
            Opcode::Load32BitFieldU2 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u16(self.pc + 1)? as i32;
                let val = self.memory.get_int(base, offset)?;
                self.stack.push_i32(val)?;
                self.pc += 3;
            }
            Opcode::Load32BitFieldU4 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u32(self.pc + 1)? as i32;
                let val = self.memory.get_int(base, offset)?;
                self.stack.push_i32(val)?;
                self.pc += 5;
            }
            Opcode::Load32BitArray => {
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                let addr = base.wrapping_add(idx as usize * 4);
                let val = self.memory.get_int(addr, 0)?;
                self.stack.push_i32(val)?;
                self.pc += 1;
            }
            Opcode::Store32BitFieldU1 => {
                let val = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u8(self.pc + 1)? as i32;
                self.memory.set_int(base, offset, val)?;
                self.pc += 2;
            }
            Opcode::Store32BitFieldU2 => {
                let val = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u16(self.pc + 1)? as i32;
                self.memory.set_int(base, offset, val)?;
                self.pc += 3;
            }
            Opcode::Store32BitFieldU4 => {
                let val = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u32(self.pc + 1)? as i32;
                self.memory.set_int(base, offset, val)?;
                self.pc += 5;
            }
            Opcode::Store32BitArray => {
                let val = self.stack.pop_i32()?;
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                let addr = base.wrapping_add(idx as usize * 4);
                self.memory.set_int(addr, 0, val)?;
                self.pc += 1;
            }
            Opcode::Add32BitArray => {
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                self.stack.push_ref(base.wrapping_add(idx as usize * 4))?;
                self.pc += 1;
            }

            // ==================================================================
            // 64-bit field load/store
            // ==================================================================
            Opcode::Load64BitFieldU1 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u8(self.pc + 1)? as i32;
                let val = self.memory.get_wide(base, offset)?;
                self.stack.push_i64(val)?;
                self.pc += 2;
            }
            Opcode::Load64BitFieldU2 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u16(self.pc + 1)? as i32;
                let val = self.memory.get_wide(base, offset)?;
                self.stack.push_i64(val)?;
                self.pc += 3;
            }
            Opcode::Load64BitFieldU4 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u32(self.pc + 1)? as i32;
                let val = self.memory.get_wide(base, offset)?;
                self.stack.push_i64(val)?;
                self.pc += 5;
            }
            Opcode::Load64BitArray => {
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                let addr = base.wrapping_add(idx as usize * 8);
                let val = self.memory.get_wide(addr, 0)?;
                self.stack.push_i64(val)?;
                self.pc += 1;
            }
            Opcode::Store64BitFieldU1 => {
                let val = self.stack.pop_i64()?;
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u8(self.pc + 1)? as i32;
                self.memory.set_wide(base, offset, val)?;
                self.pc += 2;
            }
            Opcode::Store64BitFieldU2 => {
                let val = self.stack.pop_i64()?;
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u16(self.pc + 1)? as i32;
                self.memory.set_wide(base, offset, val)?;
                self.pc += 3;
            }
            Opcode::Store64BitFieldU4 => {
                let val = self.stack.pop_i64()?;
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u32(self.pc + 1)? as i32;
                self.memory.set_wide(base, offset, val)?;
                self.pc += 5;
            }
            Opcode::Store64BitArray => {
                let val = self.stack.pop_i64()?;
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                let addr = base.wrapping_add(idx as usize * 8);
                self.memory.set_wide(addr, 0, val)?;
                self.pc += 1;
            }
            Opcode::Add64BitArray => {
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                self.stack.push_ref(base.wrapping_add(idx as usize * 8))?;
                self.pc += 1;
            }

            // ==================================================================
            // Ref field load/store (refs are 4 bytes in VM data segment)
            // ==================================================================
            Opcode::LoadRefFieldU1 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u8(self.pc + 1)? as i32;
                let val = self.memory.get_ref(base, offset)?;
                self.stack.push_ref(val)?;
                self.pc += 2;
            }
            Opcode::LoadRefFieldU2 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u16(self.pc + 1)? as i32;
                let val = self.memory.get_ref(base, offset)?;
                self.stack.push_ref(val)?;
                self.pc += 3;
            }
            Opcode::LoadRefFieldU4 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u32(self.pc + 1)? as i32;
                let val = self.memory.get_ref(base, offset)?;
                self.stack.push_ref(val)?;
                self.pc += 5;
            }
            Opcode::LoadRefArray => {
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                let addr = base.wrapping_add(idx as usize * 4);
                let val = self.memory.get_ref(addr, 0)?;
                self.stack.push_ref(val)?;
                self.pc += 1;
            }
            Opcode::StoreRefFieldU1 => {
                let val = self.stack.pop_ref()?;
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u8(self.pc + 1)? as i32;
                self.memory.set_ref(base, offset, val)?;
                self.pc += 2;
            }
            Opcode::StoreRefFieldU2 => {
                let val = self.stack.pop_ref()?;
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u16(self.pc + 1)? as i32;
                self.memory.set_ref(base, offset, val)?;
                self.pc += 3;
            }
            Opcode::StoreRefFieldU4 => {
                let val = self.stack.pop_ref()?;
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u32(self.pc + 1)? as i32;
                self.memory.set_ref(base, offset, val)?;
                self.pc += 5;
            }
            Opcode::StoreRefArray => {
                let val = self.stack.pop_ref()?;
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                let addr = base.wrapping_add(idx as usize * 4);
                self.memory.set_ref(addr, 0, val)?;
                self.pc += 1;
            }
            Opcode::AddRefArray => {
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                self.stack.push_ref(base.wrapping_add(idx as usize * 4))?;
                self.pc += 1;
            }

            // ==================================================================
            // Const field load (block index in data → code address)
            // ==================================================================
            Opcode::LoadConstFieldU1 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u8(self.pc + 1)? as i32;
                let bix = self.memory.get_short(base, offset)?;
                let addr = if bix != 0 {
                    self.memory.block_to_addr(bix)
                } else {
                    0 // null
                };
                self.stack.push_ref(addr)?;
                self.pc += 2;
            }
            Opcode::LoadConstFieldU2 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u16(self.pc + 1)? as i32;
                let bix = self.memory.get_short(base, offset)?;
                let addr = if bix != 0 {
                    self.memory.block_to_addr(bix)
                } else {
                    0
                };
                self.stack.push_ref(addr)?;
                self.pc += 3;
            }
            Opcode::LoadConstStatic => {
                let bix = self.memory.code_u16(self.pc + 1)?;
                let addr = if bix != 0 {
                    self.memory.block_to_addr(bix)
                } else {
                    0
                };
                self.stack.push_ref(addr)?;
                self.pc += 3;
            }
            Opcode::LoadConstArray => {
                let idx = self.stack.pop_i32()?;
                let base = self.stack.pop_ref()?;
                let addr = base.wrapping_add(idx as usize * 2);
                let bix = self.memory.get_short(addr, 0)?;
                let code_addr = if bix != 0 {
                    self.memory.block_to_addr(bix)
                } else {
                    0
                };
                self.stack.push_ref(code_addr)?;
                self.pc += 1;
            }

            // ==================================================================
            // Inline field load (pointer addition)
            // ==================================================================
            Opcode::LoadInlineFieldU1 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u8(self.pc + 1)? as usize;
                self.stack.push_ref(base.wrapping_add(offset))?;
                self.pc += 2;
            }
            Opcode::LoadInlineFieldU2 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u16(self.pc + 1)? as usize;
                self.stack.push_ref(base.wrapping_add(offset))?;
                self.pc += 3;
            }
            Opcode::LoadInlineFieldU4 => {
                let base = self.stack.pop_ref()?;
                let offset = self.memory.code_u32(self.pc + 1)? as usize;
                self.stack.push_ref(base.wrapping_add(offset))?;
                self.pc += 5;
            }

            // ==================================================================
            // Param0 + inline field (push param0 + offset)
            // ==================================================================
            Opcode::LoadParam0InlineFieldU1 => {
                let pp = self.param_base()?;
                let base = self.stack.get(pp)? as u32 as usize;
                let offset = self.memory.code_u8(self.pc + 1)? as usize;
                self.stack.push_ref(base.wrapping_add(offset))?;
                self.pc += 2;
            }
            Opcode::LoadParam0InlineFieldU2 => {
                let pp = self.param_base()?;
                let base = self.stack.get(pp)? as u32 as usize;
                let offset = self.memory.code_u16(self.pc + 1)? as usize;
                self.stack.push_ref(base.wrapping_add(offset))?;
                self.pc += 3;
            }
            Opcode::LoadParam0InlineFieldU4 => {
                let pp = self.param_base()?;
                let base = self.stack.get(pp)? as u32 as usize;
                let offset = self.memory.code_u32(self.pc + 1)? as usize;
                self.stack.push_ref(base.wrapping_add(offset))?;
                self.pc += 5;
            }

            // ==================================================================
            // Static data + inline field (push data_base + offset)
            // ==================================================================
            Opcode::LoadDataInlineFieldU1 => {
                let offset = self.memory.code_u8(self.pc + 1)? as usize;
                self.stack.push_ref(offset)?; // data_base = 0
                self.pc += 2;
            }
            Opcode::LoadDataInlineFieldU2 => {
                let offset = self.memory.code_u16(self.pc + 1)? as usize;
                self.stack.push_ref(offset)?;
                self.pc += 3;
            }
            Opcode::LoadDataInlineFieldU4 => {
                let offset = self.memory.code_u32(self.pc + 1)? as usize;
                self.stack.push_ref(offset)?;
                self.pc += 5;
            }

            // ==================================================================
            // Method calling
            // ==================================================================
            Opcode::LoadParam0Call => {
                // Push param0 then fall through to Call
                let pp = self.param_base()?;
                let val = self.stack.get(pp)?;
                self.stack.push_i32(val)?;
                // Fall through to Call — read block index at pc+1
                self.do_call(self.pc + 1)?;
                // do_call advances PC
            }
            Opcode::Call => {
                self.do_call(self.pc + 1)?;
            }
            Opcode::CallVirtual => {
                // Read method index and num params from bytecode
                let method_idx = self.memory.code_u16(self.pc + 1)?;
                let num_params = self.memory.code_u8(self.pc + 3)? as usize;

                // Get 'this' pointer from the stack
                let sp = self.stack.sp();
                let this_ref = self.stack.get(sp - num_params)? as u32 as usize;

                // Read vtable block index from first field of 'this'
                let vtable_bix = self.memory.get_short(this_ref, 0)?;
                let vtable_addr = self.memory.block_to_addr(vtable_bix);

                // Look up method block index in vtable
                let method_bix_addr = vtable_addr + (method_idx as usize) * 2;
                let method_bix = self.memory.code_u16(method_bix_addr)?;
                let method_addr = self.memory.block_to_addr(method_bix);

                // Read method header
                let m_num_params = self.memory.code_u8(method_addr)? as usize;
                let m_num_locals = self.memory.code_u8(method_addr + 1)? as usize;

                // Set up the call frame
                let return_pc = self.pc + 4;
                self.setup_call_frame(method_addr, m_num_params, m_num_locals, return_pc)?;
            }
            Opcode::CallNative => {
                let kit_id = self.memory.code_u8(self.pc + 1)?;
                let method_id = self.memory.code_u8(self.pc + 2)? as u16;
                let num_params = self.memory.code_u8(self.pc + 3)? as usize;

                // Collect params from the stack (top num_params cells)
                let sp = self.stack.sp();
                let mut params = Vec::with_capacity(num_params);
                for i in 0..num_params {
                    params.push(self.stack.get(sp - num_params + i)?);
                }

                // Create native context with mutable data access
                let mut data_vec = Vec::new(); // placeholder
                let mut ctx = NativeContext::new(&mut data_vec);

                let result = self.natives.call(kit_id, method_id, &mut ctx, &params)?;

                // Pop params, push result
                for _ in 0..num_params {
                    self.stack.pop_i32()?;
                }
                self.stack.push_i32(result)?;
                self.pc += 4;
            }
            Opcode::CallNativeWide => {
                let kit_id = self.memory.code_u8(self.pc + 1)?;
                let method_id = self.memory.code_u8(self.pc + 2)? as u16;
                let num_params = self.memory.code_u8(self.pc + 3)? as usize;

                let sp = self.stack.sp();
                let mut params = Vec::with_capacity(num_params);
                for i in 0..num_params {
                    params.push(self.stack.get(sp - num_params + i)?);
                }

                let mut data_vec = Vec::new();
                let mut ctx = NativeContext::new(&mut data_vec);

                let result = self
                    .natives
                    .call_wide(kit_id, method_id, &mut ctx, &params)?;

                for _ in 0..num_params {
                    self.stack.pop_i32()?;
                }
                self.stack.push_i64(result)?;
                self.pc += 4;
            }
            Opcode::CallNativeVoid => {
                let kit_id = self.memory.code_u8(self.pc + 1)?;
                let method_id = self.memory.code_u8(self.pc + 2)? as u16;
                let num_params = self.memory.code_u8(self.pc + 3)? as usize;

                let sp = self.stack.sp();
                let mut params = Vec::with_capacity(num_params);
                for i in 0..num_params {
                    params.push(self.stack.get(sp - num_params + i)?);
                }

                let mut data_vec = Vec::new();
                let mut ctx = NativeContext::new(&mut data_vec);

                let _ = self.natives.call(kit_id, method_id, &mut ctx, &params)?;

                // Pop all params, no result pushed
                for _ in 0..num_params {
                    self.stack.pop_i32()?;
                }
                self.pc += 4;
            }
            Opcode::ReturnPop => {
                let result = self.stack.pop_i32()?;
                match self.unwind_frame()? {
                    UnwindResult::TopLevel => {
                        return Ok(StepResult::ReturnI32(result));
                    }
                    UnwindResult::Caller => {
                        self.stack.push_i32(result)?;
                    }
                }
            }
            Opcode::ReturnPopWide => {
                let result = self.stack.pop_i64()?;
                match self.unwind_frame()? {
                    UnwindResult::TopLevel => {
                        return Ok(StepResult::ReturnWide(result));
                    }
                    UnwindResult::Caller => {
                        self.stack.push_i64(result)?;
                    }
                }
            }
            Opcode::ReturnVoid => {
                match self.unwind_frame()? {
                    UnwindResult::TopLevel => {
                        return Ok(StepResult::ReturnVoid);
                    }
                    UnwindResult::Caller => {
                        // nothing to push
                    }
                }
            }

            // ==================================================================
            // Misc
            // ==================================================================
            Opcode::InitArray => {
                // Stack: [..., base_ref, length, element_size]
                let elem_size = self.stack.pop_i32()? as usize;
                let length = self.stack.pop_i32()? as usize;
                let base = self.stack.pop_ref()?;

                // Initialize array: refs[0..length] point to objs[0..length]
                // where objs start at base + length * 4 (pointer size)
                let refs_start = base;
                let objs_start = base + length * 4;
                for i in 0..length {
                    let ref_addr = refs_start + i * 4;
                    let obj_addr = objs_start + i * elem_size;
                    self.memory.set_ref(ref_addr, 0, obj_addr)?;
                }
                self.pc += 1;
            }
            Opcode::InitVirt => {
                // Set vtable block index at the object's first field
                let obj = self.stack.pop_ref()?;
                let bix = self.memory.code_u16(self.pc + 1)?;
                self.memory.set_short(obj, 0, bix)?;
                self.pc += 3;
            }
            Opcode::InitComp => {
                // Set component type ID at offset +2 from object start
                let obj = self.stack.pop_ref()?;
                let bix = self.memory.code_u16(self.pc + 1)?;
                self.memory.set_short(obj, 2, bix)?;
                self.pc += 3;
            }
            Opcode::Assert => {
                let val = self.stack.pop_i32()?;
                let _line = self.memory.code_u16(self.pc + 1)?;
                if val == 0 {
                    self.assert_failures += 1;
                } else {
                    self.assert_successes += 1;
                }
                self.pc += 3;
            }
            Opcode::Switch => {
                // u1 Switch opcode
                // u2 num entries
                // u2 * num entries: jump offsets (relative to Switch opcode position)
                let num = self.memory.code_u16(self.pc + 1)? as i32;
                let cond = self.stack.pop_i32()?;
                if cond < 0 || cond >= num {
                    // Out of range: jump past the entire switch table
                    self.pc += 3 + num as usize * 2;
                } else {
                    // Read the jump offset from the table (relative to switch opcode)
                    let offset = self.read_i16_at(self.pc + 3 + cond as usize * 2)?;
                    self.pc = (self.pc as isize + offset as isize) as usize;
                }
            }
            Opcode::MetaSlot => {
                // Debug metadata — ignore, skip the u16 block index operand
                self.pc += 3;
            }

            // ==================================================================
            // Unsupported / IR-only opcodes
            // ==================================================================
            Opcode::LoadDefine | Opcode::SizeOf | Opcode::Cast | Opcode::LoadArrayLiteral => {
                return Err(VmError::InvalidOpcode(op_byte));
            }
            Opcode::LoadSlotId => {
                return Err(VmError::InvalidOpcode(op_byte));
            }
        }

        Ok(StepResult::Continue)
    }

    // ======================================================================
    // Helper methods
    // ======================================================================

    /// Read a signed i16 from the code segment at the given offset.
    #[inline]
    fn read_i16_at(&self, offset: usize) -> VmResult<i16> {
        let raw = self.memory.code_u16(offset)?;
        Ok(raw as i16)
    }

    /// Get the parameter base index (pp) for the current frame.
    /// pp = frame_pointer - num_params
    #[inline]
    fn param_base(&self) -> VmResult<usize> {
        let frame = self.stack.current_frame()?;
        Ok(frame.frame_pointer - frame.num_params as usize)
    }

    /// Get the locals base index (lp) for the current frame.
    /// lp = frame_pointer + 3 (past return_cp, prev_fp, method_addr)
    #[inline]
    fn locals_base(&self) -> VmResult<usize> {
        let frame = self.stack.current_frame()?;
        Ok(frame.frame_pointer + 3)
    }

    /// Perform a non-virtual call. `bix_offset` is the code offset where
    /// the u16 block index operand is located.
    fn do_call(&mut self, bix_offset: usize) -> VmResult<()> {
        let bix = self.memory.code_u16(bix_offset)?;
        let method_addr = self.memory.block_to_addr(bix);
        let num_params = self.memory.code_u8(method_addr)? as usize;
        let num_locals = self.memory.code_u8(method_addr + 1)? as usize;
        let return_pc = bix_offset + 2; // past the u16 operand
        self.setup_call_frame(method_addr, num_params, num_locals, return_pc)
    }

    /// Set up a new call frame and jump to the method body.
    fn setup_call_frame(
        &mut self,
        method_addr: usize,
        num_params: usize,
        num_locals: usize,
        return_pc: usize,
    ) -> VmResult<()> {
        // The parameters are already on the stack.
        // Push frame: [return_cp, prev_fp, method_addr]
        let prev_fp = self
            .stack
            .current_frame()
            .map(|f| f.frame_pointer)
            .unwrap_or(0);

        self.stack.push_i32(return_pc as i32)?; // return cp
        self.stack.push_i32(prev_fp as i32)?; // prev fp
        self.stack.push_i32(method_addr as i32)?; // method addr

        let fp_index = self.stack.sp() - 3;

        self.stack.push_frame(CallFrame {
            return_pc,
            frame_pointer: fp_index,
            method_block: 0,
            num_params: num_params as u8,
            num_locals: num_locals as u8,
        })?;

        // Allocate locals
        for _ in 0..num_locals {
            self.stack.push_i32(0)?;
        }

        // Jump to first opcode
        self.pc = method_addr + 2;
        Ok(())
    }

    /// Unwind the current call frame. Returns whether we've reached top-level.
    fn unwind_frame(&mut self) -> VmResult<UnwindResult> {
        let frame = self.stack.pop_frame()?;
        let return_pc = frame.return_pc;
        let fp = frame.frame_pointer;
        let num_params = frame.num_params as usize;

        // Pop stack back down to before the params
        self.stack.set_sp(fp - num_params);

        if return_pc == 0 {
            // Top-level call — we're done
            return Ok(UnwindResult::TopLevel);
        }

        // Restore PC to return address
        self.pc = return_pc;

        Ok(UnwindResult::Caller)
    }
}

/// Result of executing one instruction.
#[derive(Debug, Clone, PartialEq)]
pub enum StepResult {
    /// Continue executing the next instruction.
    Continue,
    /// Method returned a 32-bit value (ReturnPop from top-level).
    ReturnI32(i32),
    /// Method returned a 64-bit value (ReturnPopWide from top-level).
    ReturnWide(i64),
    /// Method returned void (ReturnVoid from top-level).
    ReturnVoid,
}

/// Internal: result of frame unwinding.
enum UnwindResult {
    /// Returned from the top-level entry — execution is complete.
    TopLevel,
    /// Returned to a caller — continue executing.
    Caller,
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_loader::{
        ScodeImage, SCODE_BLOCK_SIZE, SCODE_MAGIC, SCODE_MAJOR_VER, SCODE_MINOR_VER,
    };

    /// Build a minimal VmMemory from raw code and data bytes.
    fn make_memory(code: &[u8], data_size: u32) -> VmMemory {
        // Build a valid scode image buffer: 32-byte header + method code
        let code_size = (32 + code.len()) as u32;
        let mut buf = vec![0u8; code_size as usize];

        // Header
        buf[0..4].copy_from_slice(&SCODE_MAGIC.to_le_bytes());
        buf[4] = SCODE_MAJOR_VER;
        buf[5] = SCODE_MINOR_VER;
        buf[6] = SCODE_BLOCK_SIZE;
        buf[7] = 4; // ref_size
        buf[8..12].copy_from_slice(&code_size.to_le_bytes());
        buf[12..16].copy_from_slice(&data_size.to_le_bytes());
        buf[16..18].copy_from_slice(&7u16.to_le_bytes()); // main_method
        buf[18..20].copy_from_slice(&0u16.to_le_bytes()); // tests_bix
        buf[24..26].copy_from_slice(&12u16.to_le_bytes()); // resume_method

        // Copy method code into the buffer after the header
        buf[32..32 + code.len()].copy_from_slice(code);

        let image = ScodeImage::load_from_bytes(&buf).expect("make_memory: load failed");
        VmMemory::from_image(&image).expect("make_memory: from_image failed")
    }

    /// Build an interpreter with code placed at offset 32 (after the header).
    /// The method header (num_params, num_locals) is at the start of `code`.
    fn make_interp(code: &[u8]) -> VmInterpreter {
        let mem = make_memory(code, 256);
        VmInterpreter::new(mem, NativeTable::new())
    }

    // ------------------------------------------------------------------
    // Basic literal + return
    // ------------------------------------------------------------------

    #[test]
    fn load_i0_return() {
        // Method: 0 params, 0 locals
        // LoadI0, ReturnPop
        let code = &[
            0,
            0, // num_params=0, num_locals=0
            Opcode::LoadI0 as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        let result = interp.execute_method(32, &[]).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn load_i5_return() {
        let code = &[0, 0, Opcode::LoadI5 as u8, Opcode::ReturnPop as u8];
        let mut interp = make_interp(code);
        let result = interp.execute_method(32, &[]).unwrap();
        assert_eq!(result, 5);
    }

    #[test]
    fn load_im1_return() {
        let code = &[0, 0, Opcode::LoadIM1 as u8, Opcode::ReturnPop as u8];
        let mut interp = make_interp(code);
        let result = interp.execute_method(32, &[]).unwrap();
        assert_eq!(result, -1);
    }

    // ------------------------------------------------------------------
    // Int arithmetic
    // ------------------------------------------------------------------

    #[test]
    fn int_add() {
        // Push 3, push 4, add -> 7
        let code = &[
            0,
            0,
            Opcode::LoadI3 as u8,
            Opcode::LoadI4 as u8,
            Opcode::IntAdd as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 7);
    }

    #[test]
    fn int_sub() {
        let code = &[
            0,
            0,
            Opcode::LoadI5 as u8,
            Opcode::LoadI3 as u8,
            Opcode::IntSub as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 2);
    }

    #[test]
    fn int_mul() {
        let code = &[
            0,
            0,
            Opcode::LoadI3 as u8,
            Opcode::LoadI4 as u8,
            Opcode::IntMul as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 12);
    }

    #[test]
    fn int_div() {
        // 5 / 3 = 1 (integer division)
        let code = &[
            0,
            0,
            Opcode::LoadI5 as u8,
            Opcode::LoadI3 as u8,
            Opcode::IntDiv as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 1);
    }

    #[test]
    fn int_neg() {
        let code = &[
            0,
            0,
            Opcode::LoadI5 as u8,
            Opcode::IntNeg as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), -5);
    }

    // ------------------------------------------------------------------
    // Int compare
    // ------------------------------------------------------------------

    #[test]
    fn int_eq_true() {
        let code = &[
            0,
            0,
            Opcode::LoadI3 as u8,
            Opcode::LoadI3 as u8,
            Opcode::IntEq as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 1);
    }

    #[test]
    fn int_eq_false() {
        let code = &[
            0,
            0,
            Opcode::LoadI3 as u8,
            Opcode::LoadI4 as u8,
            Opcode::IntEq as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 0);
    }

    #[test]
    fn int_lt() {
        let code = &[
            0,
            0,
            Opcode::LoadI3 as u8,
            Opcode::LoadI5 as u8,
            Opcode::IntLt as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 1);
    }

    // ------------------------------------------------------------------
    // Params and locals
    // ------------------------------------------------------------------

    #[test]
    fn load_param0_return() {
        // Method takes 1 param, returns it
        let code = &[
            1,
            0, // 1 param, 0 locals
            Opcode::LoadParam0 as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_with_args(32, &[42]).unwrap(), 42);
    }

    #[test]
    fn store_load_local() {
        // Store 7 into local0, load it back
        let code = &[
            0,
            1, // 0 params, 1 local
            Opcode::LoadI5 as u8,
            Opcode::StoreLocal0 as u8,
            Opcode::LoadLocal0 as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 5);
    }

    // ------------------------------------------------------------------
    // Float operations
    // ------------------------------------------------------------------

    #[test]
    fn float_add() {
        let code = &[
            0,
            0,
            Opcode::LoadF0 as u8,
            Opcode::LoadF1 as u8,
            Opcode::FloatAdd as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        let result = interp.execute_method(32, &[]).unwrap();
        let f = f32::from_bits(result as u32);
        assert!((f - 1.0).abs() < 1e-6);
    }

    #[test]
    fn float_nan_eq() {
        // NullFloat == NullFloat should be true in Sedona
        let code = &[
            0,
            0,
            Opcode::LoadNullFloat as u8,
            Opcode::LoadNullFloat as u8,
            Opcode::FloatEq as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 1);
    }

    // ------------------------------------------------------------------
    // Casts
    // ------------------------------------------------------------------

    #[test]
    fn int_to_float_cast() {
        let code = &[
            0,
            0,
            Opcode::LoadI3 as u8,
            Opcode::IntToFloat as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        let result = interp.execute_method(32, &[]).unwrap();
        let f = f32::from_bits(result as u32);
        assert!((f - 3.0).abs() < 1e-6);
    }

    // ------------------------------------------------------------------
    // Near jump
    // ------------------------------------------------------------------

    #[test]
    fn jump_unconditional() {
        // Jump over LoadI3, land on LoadI5
        // Jump instruction at offset +3 from method start, size=2 bytes
        // Offset=1 means: end_of_instruction + 1 = (offset+5) + 1 = skip LoadI3, land on LoadI5
        let code = &[
            0,
            0,
            Opcode::LoadI1 as u8, // +2: push 1
            Opcode::Jump as u8,
            1u8,                     // +3: jump offset=1 from end(+5), land at +6
            Opcode::LoadI3 as u8,    // +5: skipped
            Opcode::LoadI5 as u8,    // +6: push 5
            Opcode::IntAdd as u8,    // +7: 1+5=6
            Opcode::ReturnPop as u8, // +8
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 6);
    }

    // ------------------------------------------------------------------
    // EqZero / NotEqZero
    // ------------------------------------------------------------------

    #[test]
    fn eq_zero() {
        let code = &[
            0,
            0,
            Opcode::LoadI0 as u8,
            Opcode::EqZero as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 1);
    }

    #[test]
    fn not_eq_zero() {
        let code = &[
            0,
            0,
            Opcode::LoadI5 as u8,
            Opcode::NotEqZero as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 1);
    }

    // ------------------------------------------------------------------
    // Stack manipulation
    // ------------------------------------------------------------------

    #[test]
    fn dup_add() {
        // Push 3, dup -> 3,3, add -> 6
        let code = &[
            0,
            0,
            Opcode::LoadI3 as u8,
            Opcode::Dup as u8,
            Opcode::IntAdd as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 6);
    }

    // ------------------------------------------------------------------
    // Return void
    // ------------------------------------------------------------------

    #[test]
    fn return_void() {
        let code = &[0, 0, Opcode::ReturnVoid as u8];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 0);
    }

    // ------------------------------------------------------------------
    // LoadIntU1
    // ------------------------------------------------------------------

    #[test]
    fn load_int_u1() {
        let code = &[0, 0, Opcode::LoadIntU1 as u8, 42, Opcode::ReturnPop as u8];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 42);
    }

    // ------------------------------------------------------------------
    // Nop
    // ------------------------------------------------------------------

    #[test]
    fn nop_passthrough() {
        let code = &[
            0,
            0,
            Opcode::Nop as u8,
            Opcode::LoadI1 as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 1);
    }

    // ------------------------------------------------------------------
    // Assert
    // ------------------------------------------------------------------

    #[test]
    fn assert_success_and_failure() {
        let code = &[
            0,
            0,
            // Assert true (line 1)
            Opcode::LoadI1 as u8,
            Opcode::Assert as u8,
            1,
            0,
            // Assert false (line 2)
            Opcode::LoadI0 as u8,
            Opcode::Assert as u8,
            2,
            0,
            Opcode::LoadI0 as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        interp.execute_method(32, &[]).unwrap();
        assert_eq!(interp.assert_successes, 1);
        assert_eq!(interp.assert_failures, 1);
    }

    // ------------------------------------------------------------------
    // LoadIntU2
    // ------------------------------------------------------------------

    #[test]
    fn load_int_u2() {
        let val: u16 = 1000;
        let bytes = val.to_le_bytes();
        let code = &[
            0,
            0,
            Opcode::LoadIntU2 as u8,
            bytes[0],
            bytes[1],
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_method(32, &[]).unwrap(), 1000);
    }

    // ------------------------------------------------------------------
    // Two params
    // ------------------------------------------------------------------

    #[test]
    fn add_two_params() {
        let code = &[
            2,
            0, // 2 params, 0 locals
            Opcode::LoadParam0 as u8,
            Opcode::LoadParam1 as u8,
            Opcode::IntAdd as u8,
            Opcode::ReturnPop as u8,
        ];
        let mut interp = make_interp(code);
        assert_eq!(interp.execute_with_args(32, &[10, 20]).unwrap(), 30);
    }
}
