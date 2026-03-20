//! VM execution stack with push/pop operations, wide (64-bit) support,
//! and call frame management.
//!
//! The Sedona VM uses a Cell-based stack where each cell is 4 bytes (i32).
//! Wide values (long/double) occupy TWO consecutive cells, stored in native
//! byte order (little-endian on x86/ARM).

use crate::vm_error::{VmError, VmResult};

/// Default stack size in cells (16KB / 4 bytes per cell = 4096 cells).
pub const DEFAULT_STACK_SIZE: usize = 4096;

/// Maximum call frame depth before we reject further calls.
const MAX_FRAME_DEPTH: usize = 512;

/// A call frame saved when entering a method.
///
/// In the C VM, frames are stored inline on the stack as 3 cells:
/// `[return_cp, old_fp, method_addr]`.  Here we keep them in a separate
/// `Vec` for safety (bounds-checked, no pointer aliasing).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CallFrame {
    /// Where to resume after return (code pointer / PC).
    pub return_pc: usize,
    /// Base of local variables for this frame (stack index).
    pub frame_pointer: usize,
    /// Block index of the called method.
    pub method_block: u16,
    /// Number of parameters passed.
    pub num_params: u8,
    /// Number of local variable slots.
    pub num_locals: u8,
}

/// The VM execution stack.
///
/// Cells are stored as `i32`; floats and pointers are reinterpreted via
/// bit casts.  Wide values occupy two adjacent cells: the lower 32 bits
/// at index `n` and the upper 32 bits at index `n+1` (matching the C VM's
/// `*(int64_t*)sp` on a little-endian machine).
pub struct VmStack {
    cells: Vec<i32>,
    /// Stack pointer: index of the *next free* slot (0 = empty).
    sp: usize,
    /// Separate call-frame stack.
    frames: Vec<CallFrame>,
}

impl VmStack {
    /// Create a new stack with the given capacity in cells.
    pub fn new(size: usize) -> Self {
        Self {
            cells: vec![0i32; size],
            sp: 0,
            frames: Vec::with_capacity(32),
        }
    }

    // ------------------------------------------------------------------
    // 32-bit push / pop
    // ------------------------------------------------------------------

    /// Push a 32-bit integer onto the stack.
    #[inline]
    pub fn push_i32(&mut self, val: i32) -> VmResult<()> {
        if self.sp >= self.cells.len() {
            return Err(VmError::StackOverflow);
        }
        self.cells[self.sp] = val;
        self.sp += 1;
        Ok(())
    }

    /// Pop a 32-bit integer from the stack.
    #[inline]
    pub fn pop_i32(&mut self) -> VmResult<i32> {
        if self.sp == 0 {
            return Err(VmError::StackUnderflow);
        }
        self.sp -= 1;
        Ok(self.cells[self.sp])
    }

    /// Peek at the top 32-bit integer without popping.
    #[inline]
    pub fn peek_i32(&self) -> VmResult<i32> {
        if self.sp == 0 {
            return Err(VmError::StackUnderflow);
        }
        Ok(self.cells[self.sp - 1])
    }

    // ------------------------------------------------------------------
    // Float (f32) push / pop — bit-reinterpreted
    // ------------------------------------------------------------------

    /// Push an f32 onto the stack (stored as its bit pattern in an i32 cell).
    #[inline]
    pub fn push_f32(&mut self, val: f32) -> VmResult<()> {
        self.push_i32(val.to_bits() as i32)
    }

    /// Pop an f32 from the stack.
    #[inline]
    pub fn pop_f32(&mut self) -> VmResult<f32> {
        let bits = self.pop_i32()? as u32;
        Ok(f32::from_bits(bits))
    }

    // ------------------------------------------------------------------
    // Wide (64-bit) push / pop — two cells
    // ------------------------------------------------------------------

    /// Push a 64-bit integer as two cells (lower word first, upper word second).
    #[inline]
    pub fn push_i64(&mut self, val: i64) -> VmResult<()> {
        if self.sp + 1 >= self.cells.len() {
            return Err(VmError::StackOverflow);
        }
        let lo = val as i32;
        let hi = (val >> 32) as i32;
        self.cells[self.sp] = lo;
        self.cells[self.sp + 1] = hi;
        self.sp += 2;
        Ok(())
    }

    /// Pop a 64-bit integer (two cells).
    #[inline]
    pub fn pop_i64(&mut self) -> VmResult<i64> {
        if self.sp < 2 {
            return Err(VmError::StackUnderflow);
        }
        self.sp -= 2;
        let lo = self.cells[self.sp] as u32 as u64;
        let hi = self.cells[self.sp + 1] as u32 as u64;
        Ok((hi << 32 | lo) as i64)
    }

    /// Push an f64 as two cells (bit-reinterpreted as i64).
    #[inline]
    pub fn push_f64(&mut self, val: f64) -> VmResult<()> {
        self.push_i64(val.to_bits() as i64)
    }

    /// Pop an f64 (two cells, bit-reinterpreted).
    #[inline]
    pub fn pop_f64(&mut self) -> VmResult<f64> {
        let bits = self.pop_i64()? as u64;
        Ok(f64::from_bits(bits))
    }

    // ------------------------------------------------------------------
    // Pointer / reference push / pop
    // ------------------------------------------------------------------

    /// Push a reference (usize) stored as an i32 cell.
    ///
    /// On a 32-bit target this is lossless. On 64-bit we truncate to the
    /// lower 32 bits — the VM's managed heap never exceeds 4 GB.
    #[inline]
    pub fn push_ref(&mut self, ptr: usize) -> VmResult<()> {
        self.push_i32(ptr as i32)
    }

    /// Pop a reference, zero-extending back to usize.
    #[inline]
    pub fn pop_ref(&mut self) -> VmResult<usize> {
        let v = self.pop_i32()?;
        Ok(v as u32 as usize)
    }

    // ------------------------------------------------------------------
    // Stack manipulation opcodes
    // ------------------------------------------------------------------

    /// Duplicate the top cell.  `[..., a] -> [..., a, a]`
    pub fn dup(&mut self) -> VmResult<()> {
        if self.sp == 0 {
            return Err(VmError::StackUnderflow);
        }
        if self.sp >= self.cells.len() {
            return Err(VmError::StackOverflow);
        }
        self.cells[self.sp] = self.cells[self.sp - 1];
        self.sp += 1;
        Ok(())
    }

    /// Duplicate the top two cells.  `[..., a, b] -> [..., a, b, a, b]`
    pub fn dup2(&mut self) -> VmResult<()> {
        if self.sp < 2 {
            return Err(VmError::StackUnderflow);
        }
        if self.sp + 1 >= self.cells.len() {
            return Err(VmError::StackOverflow);
        }
        self.cells[self.sp] = self.cells[self.sp - 2];
        self.cells[self.sp + 1] = self.cells[self.sp - 1];
        self.sp += 2;
        Ok(())
    }

    /// Duplicate top and insert 2 positions down.
    /// `[..., a, b] -> [..., b, a, b]`
    ///
    /// C VM: `++sp; *sp = *(sp-1); *(sp-1) = *(sp-2); *(sp-2) = *sp;`
    pub fn dupdown2(&mut self) -> VmResult<()> {
        if self.sp < 2 {
            return Err(VmError::StackUnderflow);
        }
        if self.sp >= self.cells.len() {
            return Err(VmError::StackOverflow);
        }
        let top = self.cells[self.sp - 1];
        // shift existing top-2 entries up by one
        self.cells[self.sp] = top;
        self.cells[self.sp - 1] = self.cells[self.sp - 2];
        self.cells[self.sp - 2] = top;
        self.sp += 1;
        Ok(())
    }

    /// Duplicate top and insert 3 positions down.
    /// `[..., a, b, c] -> [..., c, a, b, c]`
    ///
    /// C VM: `++sp; *sp = *(sp-1); *(sp-1) = *(sp-2); *(sp-2) = *(sp-3); *(sp-3) = *sp;`
    pub fn dupdown3(&mut self) -> VmResult<()> {
        if self.sp < 3 {
            return Err(VmError::StackUnderflow);
        }
        if self.sp >= self.cells.len() {
            return Err(VmError::StackOverflow);
        }
        let top = self.cells[self.sp - 1];
        self.cells[self.sp] = top;
        self.cells[self.sp - 1] = self.cells[self.sp - 2];
        self.cells[self.sp - 2] = self.cells[self.sp - 3];
        self.cells[self.sp - 3] = top;
        self.sp += 1;
        Ok(())
    }

    /// Discard the top cell.
    pub fn pop(&mut self) -> VmResult<()> {
        if self.sp == 0 {
            return Err(VmError::StackUnderflow);
        }
        self.sp -= 1;
        Ok(())
    }

    /// Discard the top 2 cells.
    pub fn pop2(&mut self) -> VmResult<()> {
        if self.sp < 2 {
            return Err(VmError::StackUnderflow);
        }
        self.sp -= 2;
        Ok(())
    }

    /// Discard the top 3 cells.
    pub fn pop3(&mut self) -> VmResult<()> {
        if self.sp < 3 {
            return Err(VmError::StackUnderflow);
        }
        self.sp -= 3;
        Ok(())
    }

    /// Swap the top two cells.  `[..., a, b] -> [..., b, a]`
    pub fn swap(&mut self) -> VmResult<()> {
        if self.sp < 2 {
            return Err(VmError::StackUnderflow);
        }
        self.cells.swap(self.sp - 1, self.sp - 2);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Direct access (for parameters and locals by absolute index)
    // ------------------------------------------------------------------

    /// Read a 32-bit cell at the given absolute stack index.
    #[inline]
    pub fn get(&self, index: usize) -> VmResult<i32> {
        if index >= self.cells.len() {
            return Err(VmError::StackOverflow);
        }
        Ok(self.cells[index])
    }

    /// Write a 32-bit cell at the given absolute stack index.
    #[inline]
    pub fn set(&mut self, index: usize, val: i32) -> VmResult<()> {
        if index >= self.cells.len() {
            return Err(VmError::StackOverflow);
        }
        self.cells[index] = val;
        Ok(())
    }

    /// Read a 64-bit value from two cells at `index` and `index+1`.
    #[inline]
    pub fn get_wide(&self, index: usize) -> VmResult<i64> {
        if index + 1 >= self.cells.len() {
            return Err(VmError::StackOverflow);
        }
        let lo = self.cells[index] as u32 as u64;
        let hi = self.cells[index + 1] as u32 as u64;
        Ok((hi << 32 | lo) as i64)
    }

    /// Write a 64-bit value into two cells at `index` and `index+1`.
    #[inline]
    pub fn set_wide(&mut self, index: usize, val: i64) -> VmResult<()> {
        if index + 1 >= self.cells.len() {
            return Err(VmError::StackOverflow);
        }
        self.cells[index] = val as i32;
        self.cells[index + 1] = (val >> 32) as i32;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Call frame management
    // ------------------------------------------------------------------

    /// Push a new call frame.
    pub fn push_frame(&mut self, frame: CallFrame) -> VmResult<()> {
        if self.frames.len() >= MAX_FRAME_DEPTH {
            return Err(VmError::StackOverflow);
        }
        self.frames.push(frame);
        Ok(())
    }

    /// Pop the top call frame.
    pub fn pop_frame(&mut self) -> VmResult<CallFrame> {
        self.frames.pop().ok_or(VmError::StackUnderflow)
    }

    /// Reference to the current (top-most) call frame.
    pub fn current_frame(&self) -> VmResult<&CallFrame> {
        self.frames.last().ok_or(VmError::StackUnderflow)
    }

    /// The frame pointer (base index) of the current call frame,
    /// or 0 if there are no frames.
    pub fn frame_base(&self) -> usize {
        self.frames
            .last()
            .map(|f| f.frame_pointer)
            .unwrap_or(0)
    }

    // ------------------------------------------------------------------
    // State queries
    // ------------------------------------------------------------------

    /// Current stack pointer (index of the next free slot).
    #[inline]
    pub fn sp(&self) -> usize {
        self.sp
    }

    /// Set the stack pointer directly (used during return unwinding).
    #[inline]
    pub fn set_sp(&mut self, sp: usize) {
        self.sp = sp;
    }

    /// Number of cells currently on the stack.
    #[inline]
    pub fn depth(&self) -> usize {
        self.sp
    }

    /// Number of active call frames.
    #[inline]
    pub fn frame_depth(&self) -> usize {
        self.frames.len()
    }

    /// True if the data stack is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.sp == 0
    }
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- basic i32 push/pop ----

    #[test]
    fn push_pop_i32_roundtrip() {
        let mut s = VmStack::new(16);
        s.push_i32(42).unwrap();
        s.push_i32(-1).unwrap();
        s.push_i32(0).unwrap();
        assert_eq!(s.pop_i32().unwrap(), 0);
        assert_eq!(s.pop_i32().unwrap(), -1);
        assert_eq!(s.pop_i32().unwrap(), 42);
    }

    #[test]
    fn push_pop_i32_extremes() {
        let mut s = VmStack::new(16);
        s.push_i32(i32::MIN).unwrap();
        s.push_i32(i32::MAX).unwrap();
        assert_eq!(s.pop_i32().unwrap(), i32::MAX);
        assert_eq!(s.pop_i32().unwrap(), i32::MIN);
    }

    #[test]
    fn peek_i32() {
        let mut s = VmStack::new(16);
        s.push_i32(99).unwrap();
        assert_eq!(s.peek_i32().unwrap(), 99);
        assert_eq!(s.depth(), 1); // peek doesn't change depth
    }

    // ---- f32 push/pop ----

    #[test]
    fn push_pop_f32_roundtrip() {
        let mut s = VmStack::new(16);
        s.push_f32(3.14).unwrap();
        let v = s.pop_f32().unwrap();
        assert!((v - 3.14).abs() < 1e-5);
    }

    #[test]
    fn push_pop_f32_special_values() {
        let mut s = VmStack::new(16);
        // NaN
        s.push_f32(f32::NAN).unwrap();
        assert!(s.pop_f32().unwrap().is_nan());
        // Infinity
        s.push_f32(f32::INFINITY).unwrap();
        assert_eq!(s.pop_f32().unwrap(), f32::INFINITY);
        // Negative infinity
        s.push_f32(f32::NEG_INFINITY).unwrap();
        assert_eq!(s.pop_f32().unwrap(), f32::NEG_INFINITY);
        // Negative zero
        s.push_f32(-0.0f32).unwrap();
        let v = s.pop_f32().unwrap();
        assert!(v.is_sign_negative() && v == 0.0);
    }

    // ---- i64 (wide) push/pop ----

    #[test]
    fn push_pop_i64_roundtrip() {
        let mut s = VmStack::new(16);
        s.push_i64(0x0000_0001_FFFF_FFFFi64).unwrap();
        assert_eq!(s.pop_i64().unwrap(), 0x0000_0001_FFFF_FFFFi64);
    }

    #[test]
    fn push_pop_i64_extremes() {
        let mut s = VmStack::new(16);
        s.push_i64(i64::MIN).unwrap();
        s.push_i64(i64::MAX).unwrap();
        assert_eq!(s.pop_i64().unwrap(), i64::MAX);
        assert_eq!(s.pop_i64().unwrap(), i64::MIN);
    }

    #[test]
    fn push_pop_i64_zero() {
        let mut s = VmStack::new(16);
        s.push_i64(0).unwrap();
        assert_eq!(s.pop_i64().unwrap(), 0);
    }

    #[test]
    fn i64_occupies_two_cells() {
        let mut s = VmStack::new(16);
        s.push_i64(0xDEAD_BEEF_CAFE_BABEu64 as i64).unwrap();
        assert_eq!(s.depth(), 2);
    }

    // ---- f64 (wide) push/pop ----

    #[test]
    fn push_pop_f64_roundtrip() {
        let mut s = VmStack::new(16);
        s.push_f64(std::f64::consts::PI).unwrap();
        let v = s.pop_f64().unwrap();
        assert!((v - std::f64::consts::PI).abs() < 1e-15);
    }

    #[test]
    fn push_pop_f64_special() {
        let mut s = VmStack::new(16);
        s.push_f64(f64::NAN).unwrap();
        assert!(s.pop_f64().unwrap().is_nan());
        s.push_f64(f64::INFINITY).unwrap();
        assert_eq!(s.pop_f64().unwrap(), f64::INFINITY);
    }

    // ---- ref push/pop ----

    #[test]
    fn push_pop_ref_roundtrip() {
        let mut s = VmStack::new(16);
        s.push_ref(0x1234).unwrap();
        assert_eq!(s.pop_ref().unwrap(), 0x1234);
    }

    #[test]
    fn push_pop_ref_zero() {
        let mut s = VmStack::new(16);
        s.push_ref(0).unwrap();
        assert_eq!(s.pop_ref().unwrap(), 0);
    }

    // ---- overflow / underflow ----

    #[test]
    fn stack_overflow_detection() {
        let mut s = VmStack::new(4);
        for i in 0..4 {
            s.push_i32(i).unwrap();
        }
        assert_eq!(s.push_i32(999), Err(VmError::StackOverflow));
    }

    #[test]
    fn stack_underflow_pop() {
        let mut s = VmStack::new(4);
        assert_eq!(s.pop_i32(), Err(VmError::StackUnderflow));
    }

    #[test]
    fn stack_underflow_peek() {
        let s = VmStack::new(4);
        assert_eq!(s.peek_i32(), Err(VmError::StackUnderflow));
    }

    #[test]
    fn wide_overflow() {
        let mut s = VmStack::new(3);
        s.push_i32(1).unwrap();
        s.push_i32(2).unwrap();
        // only 1 slot left, need 2 for wide
        assert_eq!(s.push_i64(999), Err(VmError::StackOverflow));
    }

    #[test]
    fn wide_underflow() {
        let mut s = VmStack::new(16);
        s.push_i32(1).unwrap(); // only 1 cell, need 2
        assert_eq!(s.pop_i64(), Err(VmError::StackUnderflow));
    }

    // ---- dup ----

    #[test]
    fn dup_basic() {
        let mut s = VmStack::new(16);
        s.push_i32(7).unwrap();
        s.dup().unwrap();
        assert_eq!(s.depth(), 2);
        assert_eq!(s.pop_i32().unwrap(), 7);
        assert_eq!(s.pop_i32().unwrap(), 7);
    }

    #[test]
    fn dup_empty_underflow() {
        let mut s = VmStack::new(16);
        assert_eq!(s.dup(), Err(VmError::StackUnderflow));
    }

    // ---- dup2 ----

    #[test]
    fn dup2_basic() {
        let mut s = VmStack::new(16);
        s.push_i32(10).unwrap();
        s.push_i32(20).unwrap();
        s.dup2().unwrap();
        assert_eq!(s.depth(), 4);
        assert_eq!(s.pop_i32().unwrap(), 20);
        assert_eq!(s.pop_i32().unwrap(), 10);
        assert_eq!(s.pop_i32().unwrap(), 20);
        assert_eq!(s.pop_i32().unwrap(), 10);
    }

    // ---- swap ----

    #[test]
    fn swap_basic() {
        let mut s = VmStack::new(16);
        s.push_i32(1).unwrap();
        s.push_i32(2).unwrap();
        s.swap().unwrap();
        assert_eq!(s.pop_i32().unwrap(), 1);
        assert_eq!(s.pop_i32().unwrap(), 2);
    }

    #[test]
    fn swap_underflow() {
        let mut s = VmStack::new(16);
        s.push_i32(1).unwrap();
        assert_eq!(s.swap(), Err(VmError::StackUnderflow));
    }

    // ---- dupdown2 ----

    #[test]
    fn dupdown2_basic() {
        // [a, b] -> [b, a, b]
        let mut s = VmStack::new(16);
        s.push_i32(100).unwrap(); // a
        s.push_i32(200).unwrap(); // b
        s.dupdown2().unwrap();
        assert_eq!(s.depth(), 3);
        assert_eq!(s.pop_i32().unwrap(), 200); // top = b
        assert_eq!(s.pop_i32().unwrap(), 100); // a
        assert_eq!(s.pop_i32().unwrap(), 200); // b inserted below
    }

    // ---- dupdown3 ----

    #[test]
    fn dupdown3_basic() {
        // [a, b, c] -> [c, a, b, c]
        let mut s = VmStack::new(16);
        s.push_i32(1).unwrap(); // a
        s.push_i32(2).unwrap(); // b
        s.push_i32(3).unwrap(); // c
        s.dupdown3().unwrap();
        assert_eq!(s.depth(), 4);
        assert_eq!(s.pop_i32().unwrap(), 3); // c (top)
        assert_eq!(s.pop_i32().unwrap(), 2); // b
        assert_eq!(s.pop_i32().unwrap(), 1); // a
        assert_eq!(s.pop_i32().unwrap(), 3); // c (inserted)
    }

    // ---- pop, pop2, pop3 ----

    #[test]
    fn pop_discard() {
        let mut s = VmStack::new(16);
        s.push_i32(1).unwrap();
        s.push_i32(2).unwrap();
        s.pop().unwrap();
        assert_eq!(s.depth(), 1);
        assert_eq!(s.pop_i32().unwrap(), 1);
    }

    #[test]
    fn pop2_discard() {
        let mut s = VmStack::new(16);
        s.push_i32(1).unwrap();
        s.push_i32(2).unwrap();
        s.push_i32(3).unwrap();
        s.pop2().unwrap();
        assert_eq!(s.depth(), 1);
        assert_eq!(s.pop_i32().unwrap(), 1);
    }

    #[test]
    fn pop3_discard() {
        let mut s = VmStack::new(16);
        s.push_i32(1).unwrap();
        s.push_i32(2).unwrap();
        s.push_i32(3).unwrap();
        s.push_i32(4).unwrap();
        s.pop3().unwrap();
        assert_eq!(s.depth(), 1);
        assert_eq!(s.pop_i32().unwrap(), 1);
    }

    #[test]
    fn pop_underflow() {
        let mut s = VmStack::new(16);
        assert_eq!(s.pop(), Err(VmError::StackUnderflow));
    }

    #[test]
    fn pop2_underflow() {
        let mut s = VmStack::new(16);
        s.push_i32(1).unwrap();
        assert_eq!(s.pop2(), Err(VmError::StackUnderflow));
    }

    #[test]
    fn pop3_underflow() {
        let mut s = VmStack::new(16);
        s.push_i32(1).unwrap();
        s.push_i32(2).unwrap();
        assert_eq!(s.pop3(), Err(VmError::StackUnderflow));
    }

    // ---- direct get/set ----

    #[test]
    fn get_set_direct() {
        let mut s = VmStack::new(16);
        s.set(5, 42).unwrap();
        assert_eq!(s.get(5).unwrap(), 42);
    }

    #[test]
    fn get_out_of_bounds() {
        let s = VmStack::new(4);
        assert_eq!(s.get(4), Err(VmError::StackOverflow));
    }

    #[test]
    fn set_out_of_bounds() {
        let mut s = VmStack::new(4);
        assert_eq!(s.set(4, 0), Err(VmError::StackOverflow));
    }

    // ---- wide get/set ----

    #[test]
    fn get_set_wide() {
        let mut s = VmStack::new(16);
        let val = 0x1234_5678_9ABC_DEF0i64;
        s.set_wide(3, val).unwrap();
        assert_eq!(s.get_wide(3).unwrap(), val);
    }

    #[test]
    fn get_wide_out_of_bounds() {
        let s = VmStack::new(4);
        // index 3 + 1 = 4 which is >= len 4
        assert_eq!(s.get_wide(3), Err(VmError::StackOverflow));
    }

    // ---- call frame management ----

    #[test]
    fn push_pop_frame() {
        let mut s = VmStack::new(16);
        let f = CallFrame {
            return_pc: 100,
            frame_pointer: 4,
            method_block: 7,
            num_params: 2,
            num_locals: 3,
        };
        s.push_frame(f).unwrap();
        assert_eq!(s.frame_depth(), 1);
        let popped = s.pop_frame().unwrap();
        assert_eq!(popped.return_pc, 100);
        assert_eq!(popped.frame_pointer, 4);
        assert_eq!(popped.method_block, 7);
        assert_eq!(popped.num_params, 2);
        assert_eq!(popped.num_locals, 3);
    }

    #[test]
    fn current_frame_ref() {
        let mut s = VmStack::new(16);
        let f = CallFrame {
            return_pc: 50,
            frame_pointer: 10,
            method_block: 1,
            num_params: 0,
            num_locals: 0,
        };
        s.push_frame(f).unwrap();
        let c = s.current_frame().unwrap();
        assert_eq!(c.return_pc, 50);
    }

    #[test]
    fn frame_base_after_push() {
        let mut s = VmStack::new(16);
        assert_eq!(s.frame_base(), 0);
        let f = CallFrame {
            return_pc: 0,
            frame_pointer: 8,
            method_block: 0,
            num_params: 0,
            num_locals: 0,
        };
        s.push_frame(f).unwrap();
        assert_eq!(s.frame_base(), 8);
    }

    #[test]
    fn nested_frames() {
        let mut s = VmStack::new(64);
        for i in 0..3 {
            let f = CallFrame {
                return_pc: i * 10,
                frame_pointer: i * 5,
                method_block: i as u16,
                num_params: 1,
                num_locals: 2,
            };
            s.push_frame(f).unwrap();
        }
        assert_eq!(s.frame_depth(), 3);
        for i in (0..3).rev() {
            let f = s.pop_frame().unwrap();
            assert_eq!(f.return_pc, i * 10);
            assert_eq!(f.frame_pointer, i * 5);
        }
        assert_eq!(s.frame_depth(), 0);
    }

    #[test]
    fn frame_underflow() {
        let mut s = VmStack::new(16);
        assert_eq!(s.pop_frame(), Err(VmError::StackUnderflow));
    }

    #[test]
    fn current_frame_underflow() {
        let s = VmStack::new(16);
        assert_eq!(s.current_frame().err(), Some(VmError::StackUnderflow));
    }

    // ---- empty state ----

    #[test]
    fn empty_stack_state() {
        let s = VmStack::new(DEFAULT_STACK_SIZE);
        assert!(s.is_empty());
        assert_eq!(s.depth(), 0);
        assert_eq!(s.sp(), 0);
        assert_eq!(s.frame_depth(), 0);
        assert_eq!(s.frame_base(), 0);
    }

    #[test]
    fn set_sp_directly() {
        let mut s = VmStack::new(16);
        s.push_i32(1).unwrap();
        s.push_i32(2).unwrap();
        s.push_i32(3).unwrap();
        s.set_sp(1);
        assert_eq!(s.sp(), 1);
        assert_eq!(s.depth(), 1);
        assert_eq!(s.pop_i32().unwrap(), 1);
    }

    // ---- mixed wide and narrow ----

    #[test]
    fn mixed_i32_i64_interleave() {
        let mut s = VmStack::new(32);
        s.push_i32(11).unwrap();
        s.push_i64(0xAAAA_BBBB_CCCC_DDDDu64 as i64).unwrap();
        s.push_i32(22).unwrap();
        assert_eq!(s.depth(), 4); // 1 + 2 + 1
        assert_eq!(s.pop_i32().unwrap(), 22);
        assert_eq!(s.pop_i64().unwrap(), 0xAAAA_BBBB_CCCC_DDDDu64 as i64);
        assert_eq!(s.pop_i32().unwrap(), 11);
    }

    #[test]
    fn dup_overflow() {
        let mut s = VmStack::new(2);
        s.push_i32(1).unwrap();
        s.push_i32(2).unwrap();
        assert_eq!(s.dup(), Err(VmError::StackOverflow));
    }

    #[test]
    fn dup2_overflow() {
        let mut s = VmStack::new(3);
        s.push_i32(1).unwrap();
        s.push_i32(2).unwrap();
        // need 2 more slots, only 1 available
        assert_eq!(s.dup2(), Err(VmError::StackOverflow));
    }
}
