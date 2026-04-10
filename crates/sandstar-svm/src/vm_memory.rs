//! VM memory model — holds scode (read-only) and data (read-write) segments.
//!
//! This mirrors the C VM's memory layout where:
//! - `codeBaseAddr` points to the scode image (methods, constants, type metadata)
//! - `dataBaseAddr` points to a separately allocated, zero-initialized data segment
//!   (component instances, static fields, heap)
//!
//! All multi-byte reads/writes use **little-endian** byte order, matching the
//! scode image format and our target platforms (ARM LE, x86).
//!
//! Field accessor methods mirror the C macros/functions from `vm.c`:
//! `getByte`, `getShort`, `getInt`, `getFloat`, `getWide`, `getRef`, `getConst`,
//! `getInline`, and their `set*` counterparts.

use crate::image_loader::ScodeImage;
use crate::vm_config::AddressWidth;
use crate::vm_error::{VmError, VmResult};

/// VM memory model — holds scode (read-only) and data (read-write) segments.
#[derive(Debug)]
pub struct VmMemory {
    /// The scode image (code + constant data, read-only at runtime)
    code: Vec<u8>,
    /// Writable data segment (component instances, static fields, heap)
    data: Vec<u8>,
    /// Block size in bytes (typically 4)
    block_size: u8,
    /// Address width for scode references (Block16 = classic, Byte32 = extended).
    address_width: AddressWidth,
}

impl VmMemory {
    /// Initialize from a loaded scode image.
    ///
    /// The code segment is cloned from the image. The data segment is allocated
    /// to `header.data_size` bytes, zero-initialized (matching C `vmInit`).
    pub fn from_image(image: &ScodeImage) -> VmResult<Self> {
        let data_size = image.header.data_size;
        if data_size == 0 {
            return Err(VmError::BadImage("data_size is 0 in scode header".into()));
        }

        Ok(VmMemory {
            code: image.code.clone(),
            data: vec![0u8; data_size as usize],
            block_size: image.header.block_size,
            address_width: AddressWidth::Block16,
        })
    }

    /// Create from an scode image with an explicit address width.
    ///
    /// Use [`AddressWidth::Byte32`] to lift the 256KB scode limit.
    pub fn from_image_extended(image: &ScodeImage, width: AddressWidth) -> VmResult<Self> {
        let mut mem = Self::from_image(image)?;
        mem.address_width = width;
        Ok(mem)
    }

    /// Read an address from the code or data segment, respecting the
    /// configured [`AddressWidth`].
    ///
    /// - **Block16**: reads a `u16` at `offset`, multiplies by `block_size`
    ///   (classic Sedona — 256KB max).
    /// - **Byte32**: reads a `u32` at `offset` as a raw byte address
    ///   (extended — 4GB max).
    #[inline]
    pub fn read_addr(&self, segment: &[u8], offset: usize) -> VmResult<usize> {
        match self.address_width {
            AddressWidth::Block16 => {
                let end = offset.checked_add(2).ok_or(VmError::NullPointer)?;
                let slice = segment.get(offset..end).ok_or(VmError::NullPointer)?;
                let block = u16::from_le_bytes([slice[0], slice[1]]);
                Ok(self.block_to_addr(block))
            }
            AddressWidth::Byte32 => {
                let end = offset.checked_add(4).ok_or(VmError::NullPointer)?;
                let slice = segment.get(offset..end).ok_or(VmError::NullPointer)?;
                Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]) as usize)
            }
        }
    }

    /// Current address width.
    #[inline]
    pub fn address_width(&self) -> AddressWidth {
        self.address_width
    }

    // =======================================================================
    // Code segment accessors (read-only)
    // =======================================================================

    /// Read a `u8` from the code segment at an absolute byte offset.
    #[inline]
    pub fn code_u8(&self, offset: usize) -> VmResult<u8> {
        self.code
            .get(offset)
            .copied()
            .ok_or(VmError::PcOutOfBounds {
                pc: offset,
                code_len: self.code.len(),
            })
    }

    /// Read a little-endian `u16` from the code segment.
    #[inline]
    pub fn code_u16(&self, offset: usize) -> VmResult<u16> {
        let end = offset.checked_add(2).ok_or(VmError::PcOutOfBounds {
            pc: offset,
            code_len: self.code.len(),
        })?;
        let slice = self.code.get(offset..end).ok_or(VmError::PcOutOfBounds {
            pc: offset,
            code_len: self.code.len(),
        })?;
        Ok(u16::from_le_bytes([slice[0], slice[1]]))
    }

    /// Read a little-endian `u32` from the code segment.
    #[inline]
    pub fn code_u32(&self, offset: usize) -> VmResult<u32> {
        let end = offset.checked_add(4).ok_or(VmError::PcOutOfBounds {
            pc: offset,
            code_len: self.code.len(),
        })?;
        let slice = self.code.get(offset..end).ok_or(VmError::PcOutOfBounds {
            pc: offset,
            code_len: self.code.len(),
        })?;
        Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    /// Read a little-endian `i32` from the code segment.
    #[inline]
    pub fn code_i32(&self, offset: usize) -> VmResult<i32> {
        self.code_u32(offset).map(|v| v as i32)
    }

    // =======================================================================
    // Data segment helpers
    // =======================================================================

    /// Compute the absolute data offset from a base address and signed offset.
    /// Returns `Err(NullPointer)` if the result is out of bounds.
    #[inline]
    fn data_addr(&self, base: usize, offset: i32) -> VmResult<usize> {
        let addr = if offset >= 0 {
            base.checked_add(offset as usize)
        } else {
            base.checked_sub((-offset) as usize)
        };
        addr.ok_or(VmError::NullPointer)
    }

    /// Bounds-checked immutable slice into the data segment.
    #[inline]
    fn data_read(&self, addr: usize, len: usize) -> VmResult<&[u8]> {
        let end = addr.checked_add(len).ok_or(VmError::NullPointer)?;
        self.data.get(addr..end).ok_or(VmError::NullPointer)
    }

    /// Bounds-checked mutable slice into the data segment.
    #[inline]
    fn data_write(&mut self, addr: usize, len: usize) -> VmResult<&mut [u8]> {
        let end = addr.checked_add(len).ok_or(VmError::NullPointer)?;
        self.data.get_mut(addr..end).ok_or(VmError::NullPointer)
    }

    // =======================================================================
    // Data segment getters — mirror C getByte/getShort/getInt/getFloat/getWide
    // =======================================================================

    /// `getByte(self, offset)` — read `u8` from `data[base + offset]`.
    #[inline]
    pub fn get_byte(&self, base: usize, offset: i32) -> VmResult<u8> {
        let addr = self.data_addr(base, offset)?;
        let s = self.data_read(addr, 1)?;
        Ok(s[0])
    }

    /// `getShort(self, offset)` — read little-endian `u16` from `data[base + offset]`.
    #[inline]
    pub fn get_short(&self, base: usize, offset: i32) -> VmResult<u16> {
        let addr = self.data_addr(base, offset)?;
        let s = self.data_read(addr, 2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }

    /// `getInt(self, offset)` — read little-endian `i32` from `data[base + offset]`.
    #[inline]
    pub fn get_int(&self, base: usize, offset: i32) -> VmResult<i32> {
        let addr = self.data_addr(base, offset)?;
        let s = self.data_read(addr, 4)?;
        Ok(i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    /// `getFloat(self, offset)` — read little-endian `f32` from `data[base + offset]`.
    #[inline]
    pub fn get_float(&self, base: usize, offset: i32) -> VmResult<f32> {
        let addr = self.data_addr(base, offset)?;
        let s = self.data_read(addr, 4)?;
        Ok(f32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    /// `getWide(self, offset)` — read little-endian `i64` from `data[base + offset]`.
    #[inline]
    pub fn get_wide(&self, base: usize, offset: i32) -> VmResult<i64> {
        let addr = self.data_addr(base, offset)?;
        let s = self.data_read(addr, 8)?;
        Ok(i64::from_le_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ]))
    }

    // =======================================================================
    // Data segment setters — mirror C setByte/setShort/setInt/setFloat/setWide
    // =======================================================================

    /// `setByte(self, offset, val)` — write `u8` to `data[base + offset]`.
    #[inline]
    pub fn set_byte(&mut self, base: usize, offset: i32, val: u8) -> VmResult<()> {
        let addr = self.data_addr(base, offset)?;
        let s = self.data_write(addr, 1)?;
        s[0] = val;
        Ok(())
    }

    /// `setShort(self, offset, val)` — write little-endian `u16`.
    #[inline]
    pub fn set_short(&mut self, base: usize, offset: i32, val: u16) -> VmResult<()> {
        let addr = self.data_addr(base, offset)?;
        let s = self.data_write(addr, 2)?;
        s.copy_from_slice(&val.to_le_bytes());
        Ok(())
    }

    /// `setInt(self, offset, val)` — write little-endian `i32`.
    #[inline]
    pub fn set_int(&mut self, base: usize, offset: i32, val: i32) -> VmResult<()> {
        let addr = self.data_addr(base, offset)?;
        let s = self.data_write(addr, 4)?;
        s.copy_from_slice(&val.to_le_bytes());
        Ok(())
    }

    /// `setFloat(self, offset, val)` — write little-endian `f32`.
    #[inline]
    pub fn set_float(&mut self, base: usize, offset: i32, val: f32) -> VmResult<()> {
        let addr = self.data_addr(base, offset)?;
        let s = self.data_write(addr, 4)?;
        s.copy_from_slice(&val.to_le_bytes());
        Ok(())
    }

    /// `setWide(self, offset, val)` — write little-endian `i64`.
    #[inline]
    pub fn set_wide(&mut self, base: usize, offset: i32, val: i64) -> VmResult<()> {
        let addr = self.data_addr(base, offset)?;
        let s = self.data_write(addr, 8)?;
        s.copy_from_slice(&val.to_le_bytes());
        Ok(())
    }

    // =======================================================================
    // Higher-level accessors
    // =======================================================================

    /// `getConst(vm, self, offset)` — follow a const reference from the data
    /// segment into the code segment.
    ///
    /// In C: reads a `u16` block index from `self+offset`, then returns
    /// `codeBaseAddr + bix * blockSize`. Here we return the byte offset into
    /// the code segment.
    #[inline]
    pub fn get_const(&self, base: usize, offset: i32) -> VmResult<usize> {
        let bix = self.get_short(base, offset)?;
        Ok(self.block_to_addr(bix))
    }

    /// `getRef(self, offset)` — read a reference (data segment offset).
    ///
    /// In the C VM, refs are `void*` pointers. In our safe Rust model, refs
    /// are stored as `u32` offsets into the data segment (matching the 32-bit
    /// Sedona pointer size).
    #[inline]
    pub fn get_ref(&self, base: usize, offset: i32) -> VmResult<usize> {
        let addr = self.data_addr(base, offset)?;
        let s = self.data_read(addr, 4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
    }

    /// `setRef(self, offset, val)` — set a reference (data segment offset).
    #[inline]
    pub fn set_ref(&mut self, base: usize, offset: i32, val: usize) -> VmResult<()> {
        let addr = self.data_addr(base, offset)?;
        let s = self.data_write(addr, 4)?;
        s.copy_from_slice(&(val as u32).to_le_bytes());
        Ok(())
    }

    /// `getInline(self, offset)` — get pointer to inline data: `base + offset`.
    ///
    /// In C this returns `(uint8_t*)self + offset`. Here we return the computed
    /// data segment offset without bounds checking (the caller validates later).
    #[inline]
    pub fn get_inline(&self, base: usize, offset: i32) -> usize {
        if offset >= 0 {
            base.wrapping_add(offset as usize)
        } else {
            base.wrapping_sub((-offset) as usize)
        }
    }

    /// Convert a block index to a byte offset: `block * block_size`.
    ///
    /// Matches the C macro `block2addr(cb, block) = cb + (block << 2)`.
    #[inline]
    pub fn block_to_addr(&self, block: u16) -> usize {
        (block as usize) * (self.block_size as usize)
    }

    // =======================================================================
    // Raw access for native methods
    // =======================================================================

    /// Get an immutable slice of the data segment.
    #[inline]
    pub fn data_slice(&self, offset: usize, len: usize) -> VmResult<&[u8]> {
        self.data_read(offset, len)
    }

    /// Get a mutable slice of the data segment.
    #[inline]
    pub fn data_slice_mut(&mut self, offset: usize, len: usize) -> VmResult<&mut [u8]> {
        self.data_write(offset, len)
    }

    /// Get an immutable slice of the code segment.
    #[inline]
    pub fn code_slice(&self, offset: usize, len: usize) -> VmResult<&[u8]> {
        let end = offset.checked_add(len).ok_or(VmError::PcOutOfBounds {
            pc: offset,
            code_len: self.code.len(),
        })?;
        self.code.get(offset..end).ok_or(VmError::PcOutOfBounds {
            pc: offset,
            code_len: self.code.len(),
        })
    }

    /// Data segment size in bytes.
    #[inline]
    pub fn data_len(&self) -> usize {
        self.data.len()
    }

    /// Code segment size in bytes.
    #[inline]
    pub fn code_len(&self) -> usize {
        self.code.len()
    }

    /// Block size in bytes.
    #[inline]
    pub fn block_size(&self) -> u8 {
        self.block_size
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_loader::{
        ScodeImage, SCODE_BLOCK_SIZE, SCODE_MAGIC, SCODE_MAJOR_VER, SCODE_MINOR_VER,
    };
    use crate::vm_config::AddressWidth;

    /// Build a minimal valid scode image with given code_size and data_size.
    fn make_image(code_size: u32, data_size: u32) -> ScodeImage {
        let mut buf = vec![0u8; code_size as usize];
        // header
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
        ScodeImage::load_from_bytes(&buf).expect("make_image failed")
    }

    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    #[test]
    fn from_image_basic() {
        let image = make_image(128, 256);
        let mem = VmMemory::from_image(&image).unwrap();
        assert_eq!(mem.code_len(), 128);
        assert_eq!(mem.data_len(), 256);
        assert_eq!(mem.block_size(), SCODE_BLOCK_SIZE);
    }

    #[test]
    fn from_image_data_zeroed() {
        let image = make_image(64, 128);
        let mem = VmMemory::from_image(&image).unwrap();
        // All data bytes should be zero (matching C memset(0))
        for i in 0..128 {
            assert_eq!(mem.get_byte(i, 0).unwrap(), 0, "byte at {i} not zero");
        }
    }

    #[test]
    fn from_image_zero_data_size_rejected() {
        // Manually create an image with data_size=0
        let mut buf = vec![0u8; 64];
        buf[0..4].copy_from_slice(&SCODE_MAGIC.to_le_bytes());
        buf[4] = SCODE_MAJOR_VER;
        buf[5] = SCODE_MINOR_VER;
        buf[6] = SCODE_BLOCK_SIZE;
        buf[7] = 4;
        buf[8..12].copy_from_slice(&64u32.to_le_bytes());
        buf[12..16].copy_from_slice(&0u32.to_le_bytes()); // data_size = 0
        buf[16..18].copy_from_slice(&7u16.to_le_bytes());
        let image = ScodeImage::load_from_bytes(&buf).unwrap();
        let err = VmMemory::from_image(&image).unwrap_err();
        assert!(matches!(err, VmError::BadImage(_)));
    }

    // -----------------------------------------------------------------------
    // Code segment reads
    // -----------------------------------------------------------------------

    #[test]
    fn code_u8_reads_header() {
        let image = make_image(128, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        assert_eq!(mem.code_u8(4).unwrap(), SCODE_MAJOR_VER);
        assert_eq!(mem.code_u8(5).unwrap(), SCODE_MINOR_VER);
        assert_eq!(mem.code_u8(6).unwrap(), SCODE_BLOCK_SIZE);
    }

    #[test]
    fn code_u16_reads_little_endian() {
        let image = make_image(128, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        // main_method = 7 at offset 16
        assert_eq!(mem.code_u16(16).unwrap(), 7);
    }

    #[test]
    fn code_u32_reads_magic() {
        let image = make_image(128, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        assert_eq!(mem.code_u32(0).unwrap(), SCODE_MAGIC);
    }

    #[test]
    fn code_i32_reads_signed() {
        let image = make_image(128, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        // image_size = 128 at offset 8
        assert_eq!(mem.code_i32(8).unwrap(), 128);
    }

    #[test]
    fn code_u8_out_of_bounds() {
        let image = make_image(64, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        let err = mem.code_u8(64).unwrap_err();
        assert!(matches!(err, VmError::PcOutOfBounds { .. }));
    }

    #[test]
    fn code_u16_out_of_bounds() {
        let image = make_image(64, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        // offset 63 — only 1 byte left, need 2
        let err = mem.code_u16(63).unwrap_err();
        assert!(matches!(err, VmError::PcOutOfBounds { .. }));
    }

    #[test]
    fn code_u32_out_of_bounds() {
        let image = make_image(64, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        let err = mem.code_u32(61).unwrap_err();
        assert!(matches!(err, VmError::PcOutOfBounds { .. }));
    }

    // -----------------------------------------------------------------------
    // Data segment get/set roundtrips
    // -----------------------------------------------------------------------

    #[test]
    fn byte_roundtrip() {
        let image = make_image(64, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();
        mem.set_byte(10, 0, 0xAB).unwrap();
        assert_eq!(mem.get_byte(10, 0).unwrap(), 0xAB);
        mem.set_byte(10, 5, 0xFF).unwrap();
        assert_eq!(mem.get_byte(10, 5).unwrap(), 0xFF);
    }

    #[test]
    fn short_roundtrip() {
        let image = make_image(64, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();
        mem.set_short(20, 0, 0x1234).unwrap();
        assert_eq!(mem.get_short(20, 0).unwrap(), 0x1234);
        mem.set_short(20, 4, 0xFFFF).unwrap();
        assert_eq!(mem.get_short(20, 4).unwrap(), 0xFFFF);
    }

    #[test]
    fn int_roundtrip() {
        let image = make_image(64, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();
        mem.set_int(0, 0, 42).unwrap();
        assert_eq!(mem.get_int(0, 0).unwrap(), 42);
        mem.set_int(0, 4, -1).unwrap();
        assert_eq!(mem.get_int(0, 4).unwrap(), -1);
        mem.set_int(0, 8, i32::MIN).unwrap();
        assert_eq!(mem.get_int(0, 8).unwrap(), i32::MIN);
        mem.set_int(0, 12, i32::MAX).unwrap();
        assert_eq!(mem.get_int(0, 12).unwrap(), i32::MAX);
    }

    #[test]
    fn float_roundtrip() {
        let image = make_image(64, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();
        mem.set_float(0, 0, 3.14).unwrap();
        assert_eq!(mem.get_float(0, 0).unwrap(), 3.14);
        mem.set_float(0, 4, -0.0).unwrap();
        assert_eq!(mem.get_float(0, 4).unwrap(), -0.0);
        // Positive infinity
        mem.set_float(0, 8, f32::INFINITY).unwrap();
        assert_eq!(mem.get_float(0, 8).unwrap(), f32::INFINITY);
    }

    #[test]
    fn float_nan_roundtrip() {
        let image = make_image(64, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();
        mem.set_float(0, 0, f32::NAN).unwrap();
        assert!(mem.get_float(0, 0).unwrap().is_nan());

        // Sedona NULLFLOAT (0x7fc00000) is a specific NaN
        let null_float = f32::from_bits(0x7fc00000);
        mem.set_float(0, 4, null_float).unwrap();
        assert!(mem.get_float(0, 4).unwrap().is_nan());
        assert_eq!(mem.get_float(0, 4).unwrap().to_bits(), 0x7fc00000);
    }

    #[test]
    fn wide_roundtrip() {
        let image = make_image(64, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();
        mem.set_wide(0, 0, 123456789012345i64).unwrap();
        assert_eq!(mem.get_wide(0, 0).unwrap(), 123456789012345i64);
    }

    #[test]
    fn wide_min_max() {
        let image = make_image(64, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();
        mem.set_wide(0, 0, i64::MIN).unwrap();
        assert_eq!(mem.get_wide(0, 0).unwrap(), i64::MIN);
        mem.set_wide(0, 8, i64::MAX).unwrap();
        assert_eq!(mem.get_wide(0, 8).unwrap(), i64::MAX);
    }

    #[test]
    fn wide_zero() {
        let image = make_image(64, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();
        mem.set_wide(0, 0, 0i64).unwrap();
        assert_eq!(mem.get_wide(0, 0).unwrap(), 0i64);
    }

    // -----------------------------------------------------------------------
    // Negative offsets
    // -----------------------------------------------------------------------

    #[test]
    fn negative_offset_get_byte() {
        let image = make_image(64, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();
        mem.set_byte(10, 0, 0x42).unwrap();
        // Read from base=15, offset=-5 => addr=10
        assert_eq!(mem.get_byte(15, -5).unwrap(), 0x42);
    }

    #[test]
    fn negative_offset_get_int() {
        let image = make_image(64, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();
        mem.set_int(20, 0, 999).unwrap();
        // Read from base=30, offset=-10 => addr=20
        assert_eq!(mem.get_int(30, -10).unwrap(), 999);
    }

    #[test]
    fn negative_offset_underflow_returns_error() {
        let image = make_image(64, 128);
        let mem = VmMemory::from_image(&image).unwrap();
        // base=5, offset=-10 => underflow
        let err = mem.get_byte(5, -10).unwrap_err();
        assert!(matches!(err, VmError::NullPointer));
    }

    // -----------------------------------------------------------------------
    // Out-of-bounds data access
    // -----------------------------------------------------------------------

    #[test]
    fn data_get_byte_out_of_bounds() {
        let image = make_image(64, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        let err = mem.get_byte(64, 0).unwrap_err();
        assert!(matches!(err, VmError::NullPointer));
    }

    #[test]
    fn data_set_int_out_of_bounds() {
        let image = make_image(64, 64);
        let mut mem = VmMemory::from_image(&image).unwrap();
        // offset 61: need 4 bytes but only 3 remain
        let err = mem.set_int(61, 0, 42).unwrap_err();
        assert!(matches!(err, VmError::NullPointer));
    }

    #[test]
    fn data_get_wide_out_of_bounds() {
        let image = make_image(64, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        // offset 60: need 8 bytes but only 4 remain
        let err = mem.get_wide(60, 0).unwrap_err();
        assert!(matches!(err, VmError::NullPointer));
    }

    // -----------------------------------------------------------------------
    // get_const
    // -----------------------------------------------------------------------

    #[test]
    fn get_const_follows_reference() {
        let image = make_image(128, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();

        // Store block index 10 at data[base=0, offset=2] as u16
        // getConst reads a u16 block index, multiplies by block_size
        mem.set_short(0, 2, 10).unwrap();
        let code_offset = mem.get_const(0, 2).unwrap();
        assert_eq!(code_offset, 10 * 4); // block_size = 4
    }

    #[test]
    fn get_const_block_zero() {
        let image = make_image(128, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();
        mem.set_short(0, 0, 0).unwrap();
        assert_eq!(mem.get_const(0, 0).unwrap(), 0);
    }

    #[test]
    fn get_const_max_block() {
        let image = make_image(128, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();
        mem.set_short(0, 0, 0xFFFF).unwrap();
        let offset = mem.get_const(0, 0).unwrap();
        assert_eq!(offset, 0xFFFF * 4);
    }

    // -----------------------------------------------------------------------
    // get_ref / set_ref
    // -----------------------------------------------------------------------

    #[test]
    fn ref_roundtrip() {
        let image = make_image(64, 256);
        let mut mem = VmMemory::from_image(&image).unwrap();
        mem.set_ref(0, 0, 200).unwrap();
        assert_eq!(mem.get_ref(0, 0).unwrap(), 200);
    }

    #[test]
    fn ref_zero_is_null() {
        let image = make_image(64, 128);
        let mem = VmMemory::from_image(&image).unwrap();
        // Data is zeroed, so ref at 0 should be 0 (null pointer in Sedona)
        assert_eq!(mem.get_ref(0, 0).unwrap(), 0);
    }

    #[test]
    fn set_ref_overwrites() {
        let image = make_image(64, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();
        mem.set_ref(10, 0, 100).unwrap();
        assert_eq!(mem.get_ref(10, 0).unwrap(), 100);
        mem.set_ref(10, 0, 50).unwrap();
        assert_eq!(mem.get_ref(10, 0).unwrap(), 50);
    }

    // -----------------------------------------------------------------------
    // get_inline
    // -----------------------------------------------------------------------

    #[test]
    fn get_inline_returns_base_plus_offset() {
        let image = make_image(64, 128);
        let mem = VmMemory::from_image(&image).unwrap();
        assert_eq!(mem.get_inline(100, 20), 120);
        assert_eq!(mem.get_inline(50, 0), 50);
    }

    #[test]
    fn get_inline_negative_offset() {
        let image = make_image(64, 128);
        let mem = VmMemory::from_image(&image).unwrap();
        assert_eq!(mem.get_inline(100, -10), 90);
    }

    // -----------------------------------------------------------------------
    // block_to_addr
    // -----------------------------------------------------------------------

    #[test]
    fn block_to_addr_calculation() {
        let image = make_image(64, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        assert_eq!(mem.block_to_addr(0), 0);
        assert_eq!(mem.block_to_addr(1), 4);
        assert_eq!(mem.block_to_addr(7), 28);
        assert_eq!(mem.block_to_addr(100), 400);
        assert_eq!(mem.block_to_addr(0xFFFF), 65535 * 4);
    }

    // -----------------------------------------------------------------------
    // data_slice / data_slice_mut
    // -----------------------------------------------------------------------

    #[test]
    fn data_slice_reads_range() {
        let image = make_image(64, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();
        mem.set_byte(10, 0, 0xAA).unwrap();
        mem.set_byte(11, 0, 0xBB).unwrap();
        let slice = mem.data_slice(10, 2).unwrap();
        assert_eq!(slice, &[0xAA, 0xBB]);
    }

    #[test]
    fn data_slice_mut_writes_range() {
        let image = make_image(64, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();
        {
            let slice = mem.data_slice_mut(20, 4).unwrap();
            slice.copy_from_slice(&[1, 2, 3, 4]);
        }
        assert_eq!(mem.get_byte(20, 0).unwrap(), 1);
        assert_eq!(mem.get_byte(20, 1).unwrap(), 2);
        assert_eq!(mem.get_byte(20, 2).unwrap(), 3);
        assert_eq!(mem.get_byte(20, 3).unwrap(), 4);
    }

    #[test]
    fn data_slice_out_of_bounds() {
        let image = make_image(64, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        let err = mem.data_slice(60, 10).unwrap_err();
        assert!(matches!(err, VmError::NullPointer));
    }

    #[test]
    fn data_slice_mut_out_of_bounds() {
        let image = make_image(64, 64);
        let mut mem = VmMemory::from_image(&image).unwrap();
        let err = mem.data_slice_mut(60, 10).unwrap_err();
        assert!(matches!(err, VmError::NullPointer));
    }

    #[test]
    fn data_slice_zero_length() {
        let image = make_image(64, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        let slice = mem.data_slice(0, 0).unwrap();
        assert!(slice.is_empty());
    }

    // -----------------------------------------------------------------------
    // code_slice
    // -----------------------------------------------------------------------

    #[test]
    fn code_slice_reads_header() {
        let image = make_image(128, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        let magic_bytes = mem.code_slice(0, 4).unwrap();
        assert_eq!(magic_bytes, &SCODE_MAGIC.to_le_bytes());
    }

    #[test]
    fn code_slice_out_of_bounds() {
        let image = make_image(64, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        let err = mem.code_slice(60, 10).unwrap_err();
        assert!(matches!(err, VmError::PcOutOfBounds { .. }));
    }

    // -----------------------------------------------------------------------
    // Mixed scenarios
    // -----------------------------------------------------------------------

    #[test]
    fn multiple_fields_at_different_offsets() {
        let image = make_image(64, 256);
        let mut mem = VmMemory::from_image(&image).unwrap();

        // Simulate a component at base=100 with fields:
        // offset 0: typeId (byte) = 4
        // offset 2: constRef (short/block) = 5
        // offset 4: intField = -42
        // offset 8: floatField = 72.5
        // offset 12: refField -> 200

        mem.set_byte(100, 0, 4).unwrap();
        mem.set_short(100, 2, 5).unwrap();
        mem.set_int(100, 4, -42).unwrap();
        mem.set_float(100, 8, 72.5).unwrap();
        mem.set_ref(100, 12, 200).unwrap();

        assert_eq!(mem.get_byte(100, 0).unwrap(), 4);
        assert_eq!(mem.get_short(100, 2).unwrap(), 5);
        assert_eq!(mem.get_int(100, 4).unwrap(), -42);
        assert_eq!(mem.get_float(100, 8).unwrap(), 72.5);
        assert_eq!(mem.get_ref(100, 12).unwrap(), 200);
        assert_eq!(mem.get_const(100, 2).unwrap(), 5 * 4);
    }

    #[test]
    fn overlapping_writes_correct_byte_order() {
        let image = make_image(64, 128);
        let mut mem = VmMemory::from_image(&image).unwrap();

        // Write i32 = 0x04030201 at offset 0
        mem.set_int(0, 0, 0x04030201).unwrap();

        // LE: byte[0]=0x01, byte[1]=0x02, byte[2]=0x03, byte[3]=0x04
        assert_eq!(mem.get_byte(0, 0).unwrap(), 0x01);
        assert_eq!(mem.get_byte(0, 1).unwrap(), 0x02);
        assert_eq!(mem.get_byte(0, 2).unwrap(), 0x03);
        assert_eq!(mem.get_byte(0, 3).unwrap(), 0x04);

        // Short at offset 0 = 0x0201
        assert_eq!(mem.get_short(0, 0).unwrap(), 0x0201);
        // Short at offset 2 = 0x0403
        assert_eq!(mem.get_short(0, 2).unwrap(), 0x0403);
    }

    #[test]
    fn data_len_and_code_len() {
        let image = make_image(256, 512);
        let mem = VmMemory::from_image(&image).unwrap();
        assert_eq!(mem.data_len(), 512);
        assert_eq!(mem.code_len(), 256);
    }

    // -----------------------------------------------------------------------
    // Extended addressing (AddressWidth)
    // -----------------------------------------------------------------------

    #[test]
    fn default_address_width_is_block16() {
        let image = make_image(128, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        assert_eq!(mem.address_width(), AddressWidth::Block16);
    }

    #[test]
    fn from_image_extended_sets_width() {
        let image = make_image(128, 64);
        let mem = VmMemory::from_image_extended(&image, AddressWidth::Byte32).unwrap();
        assert_eq!(mem.address_width(), AddressWidth::Byte32);
    }

    #[test]
    fn read_addr_block16() {
        let image = make_image(128, 128);
        let mem = VmMemory::from_image(&image).unwrap();
        // Place a u16 block index = 10 in a buffer
        let mut buf = vec![0u8; 16];
        buf[0..2].copy_from_slice(&10u16.to_le_bytes());
        let addr = mem.read_addr(&buf, 0).unwrap();
        assert_eq!(addr, 10 * 4); // block_size = 4
    }

    #[test]
    fn read_addr_byte32() {
        let image = make_image(128, 128);
        let mem = VmMemory::from_image_extended(&image, AddressWidth::Byte32).unwrap();
        let mut buf = vec![0u8; 16];
        buf[0..4].copy_from_slice(&12345u32.to_le_bytes());
        let addr = mem.read_addr(&buf, 0).unwrap();
        assert_eq!(addr, 12345);
    }

    #[test]
    fn read_addr_block16_out_of_bounds() {
        let image = make_image(64, 64);
        let mem = VmMemory::from_image(&image).unwrap();
        let buf = vec![0u8; 1]; // too small for u16
        assert!(mem.read_addr(&buf, 0).is_err());
    }

    #[test]
    fn read_addr_byte32_out_of_bounds() {
        let image = make_image(64, 64);
        let mem = VmMemory::from_image_extended(&image, AddressWidth::Byte32).unwrap();
        let buf = vec![0u8; 3]; // too small for u32
        assert!(mem.read_addr(&buf, 0).is_err());
    }
}
