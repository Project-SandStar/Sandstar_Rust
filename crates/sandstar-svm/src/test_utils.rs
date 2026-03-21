//! Test scode builder for creating minimal bytecode sequences.
//!
//! Provides [`ScodeBuilder`] which assembles valid scode images in memory,
//! useful for testing individual opcodes without a real `.scode` file.

use crate::image_loader::{
    ScodeImage, SCODE_BLOCK_SIZE, SCODE_HEADER_SIZE, SCODE_MAGIC, SCODE_MAJOR_VER,
    SCODE_MINOR_VER,
};
use crate::vm_memory::VmMemory;

/// Builder for creating minimal scode images for testing.
///
/// # Example
///
/// ```ignore
/// let image = ScodeBuilder::new()
///     .op(Opcode::LoadI0)
///     .op(Opcode::LoadI1)
///     .op(Opcode::IntAdd)
///     .build();
/// ```
pub struct ScodeBuilder {
    code: Vec<u8>,
    data_size: usize,
}

impl ScodeBuilder {
    /// Create a new builder with an empty code section.
    pub fn new() -> Self {
        Self {
            code: Vec::new(),
            data_size: 256,
        }
    }

    /// Emit a single opcode (no operands).
    pub fn op(mut self, opcode: crate::opcodes::Opcode) -> Self {
        self.code.push(opcode as u8);
        self
    }

    /// Emit an opcode followed by a 1-byte unsigned operand.
    pub fn op_u8(mut self, opcode: crate::opcodes::Opcode, val: u8) -> Self {
        self.code.push(opcode as u8);
        self.code.push(val);
        self
    }

    /// Emit an opcode followed by a 2-byte unsigned operand (little-endian).
    pub fn op_u16(mut self, opcode: crate::opcodes::Opcode, val: u16) -> Self {
        self.code.push(opcode as u8);
        self.code.extend_from_slice(&val.to_le_bytes());
        self
    }

    /// Emit an opcode followed by a signed 2-byte operand (little-endian).
    pub fn op_i16(mut self, opcode: crate::opcodes::Opcode, val: i16) -> Self {
        self.code.push(opcode as u8);
        self.code.extend_from_slice(&val.to_le_bytes());
        self
    }

    /// Emit an opcode followed by a 4-byte unsigned operand (little-endian).
    pub fn op_u32(mut self, opcode: crate::opcodes::Opcode, val: u32) -> Self {
        self.code.push(opcode as u8);
        self.code.extend_from_slice(&val.to_le_bytes());
        self
    }

    /// Emit raw bytes into the code section.
    pub fn raw(mut self, bytes: &[u8]) -> Self {
        self.code.extend_from_slice(bytes);
        self
    }

    /// Emit a single raw byte.
    pub fn byte(mut self, b: u8) -> Self {
        self.code.push(b);
        self
    }

    /// Set the data segment size (default is 256 bytes).
    pub fn data_size(mut self, size: usize) -> Self {
        self.data_size = size;
        self
    }

    /// Current code offset (useful for calculating branch targets).
    ///
    /// This is the offset within the code section (after the header).
    /// The absolute offset in the final image is `SCODE_HEADER_SIZE + offset()`.
    pub fn offset(&self) -> usize {
        self.code.len()
    }

    /// Build the assembled code into a valid [`ScodeImage`].
    ///
    /// Creates a 32-byte header followed by the code bytes. The image_size
    /// field is set to `header_size + code_len`, and the data_size field
    /// controls the writable data segment allocated by [`VmMemory::from_image`].
    pub fn build(self) -> ScodeImage {
        let image_size = SCODE_HEADER_SIZE + self.code.len();
        let mut buf = vec![0u8; image_size];

        // Magic (LE)
        buf[0..4].copy_from_slice(&SCODE_MAGIC.to_le_bytes());
        // Version
        buf[4] = SCODE_MAJOR_VER;
        buf[5] = SCODE_MINOR_VER;
        // Block size
        buf[6] = SCODE_BLOCK_SIZE;
        // Ref size (4 = 32-bit)
        buf[7] = 4;
        // Image size (LE)
        buf[8..12].copy_from_slice(&(image_size as u32).to_le_bytes());
        // Data size (LE)
        buf[12..16].copy_from_slice(&(self.data_size as u32).to_le_bytes());
        // main_method = block 8 (offset 32 = first byte after header)
        let main_block = (SCODE_HEADER_SIZE / SCODE_BLOCK_SIZE as usize) as u16;
        buf[16..18].copy_from_slice(&main_block.to_le_bytes());
        // tests_bix = 0 (no tests)
        buf[18..20].copy_from_slice(&0u16.to_le_bytes());
        // reserved_a (20..24) = 0
        // resume_method = 0
        buf[24..26].copy_from_slice(&0u16.to_le_bytes());
        // reserved_b (26..28) = 0
        // reserved_c (28..32) = 0

        // Append code bytes after the header
        buf[SCODE_HEADER_SIZE..].copy_from_slice(&self.code);

        ScodeImage::load_from_bytes(&buf).expect("ScodeBuilder produced invalid scode image")
    }

    /// Build into a [`VmMemory`] (convenience wrapper around `build()`).
    pub fn build_memory(self) -> VmMemory {
        let image = self.build();
        VmMemory::from_image(&image).expect("ScodeBuilder produced image that VmMemory rejected")
    }
}

impl Default for ScodeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_loader::{SCODE_HEADER_SIZE, SCODE_MAGIC};
    use crate::opcodes::Opcode;

    #[test]
    fn build_creates_valid_image() {
        let image = ScodeBuilder::new().build();
        assert_eq!(image.header.magic, SCODE_MAGIC);
        assert_eq!(image.header.major_ver, 1);
        assert_eq!(image.header.minor_ver, 5);
        assert_eq!(image.header.block_size, 4);
        assert_eq!(image.header.ref_size, 4);
        assert_eq!(image.header.image_size as usize, SCODE_HEADER_SIZE);
        assert_eq!(image.header.data_size, 256);
    }

    #[test]
    fn build_with_code_has_correct_size() {
        let image = ScodeBuilder::new()
            .op(Opcode::Nop)
            .op(Opcode::LoadI0)
            .build();
        assert_eq!(image.len(), SCODE_HEADER_SIZE + 2);
        assert_eq!(image.header.image_size as usize, SCODE_HEADER_SIZE + 2);
    }

    #[test]
    fn op_emits_correct_byte() {
        let image = ScodeBuilder::new().op(Opcode::LoadI0).build();
        // LoadI0 = 2, located right after header
        assert_eq!(image.get_u8(SCODE_HEADER_SIZE), Some(Opcode::LoadI0 as u8));
    }

    #[test]
    fn op_u8_emits_opcode_and_operand() {
        let image = ScodeBuilder::new()
            .op_u8(Opcode::LoadIntU1, 42)
            .build();
        assert_eq!(
            image.get_u8(SCODE_HEADER_SIZE),
            Some(Opcode::LoadIntU1 as u8)
        );
        assert_eq!(image.get_u8(SCODE_HEADER_SIZE + 1), Some(42));
    }

    #[test]
    fn op_u16_emits_little_endian() {
        let image = ScodeBuilder::new()
            .op_u16(Opcode::LoadIntU2, 0x1234)
            .build();
        assert_eq!(
            image.get_u8(SCODE_HEADER_SIZE),
            Some(Opcode::LoadIntU2 as u8)
        );
        // Little-endian: low byte first
        assert_eq!(image.get_u8(SCODE_HEADER_SIZE + 1), Some(0x34));
        assert_eq!(image.get_u8(SCODE_HEADER_SIZE + 2), Some(0x12));
    }

    #[test]
    fn op_i16_emits_signed_correctly() {
        let image = ScodeBuilder::new()
            .op_i16(Opcode::Nop, -3) // using Nop as placeholder
            .build();
        let lo = image.get_u8(SCODE_HEADER_SIZE + 1).unwrap();
        let hi = image.get_u8(SCODE_HEADER_SIZE + 2).unwrap();
        let reconstructed = i16::from_le_bytes([lo, hi]);
        assert_eq!(reconstructed, -3);
    }

    #[test]
    fn op_u32_emits_four_bytes() {
        let image = ScodeBuilder::new()
            .op_u32(Opcode::Nop, 0xDEAD_BEEF)
            .build();
        assert_eq!(
            image.get_u32(SCODE_HEADER_SIZE + 1),
            Some(0xDEAD_BEEF)
        );
    }

    #[test]
    fn raw_bytes_emitted_directly() {
        let image = ScodeBuilder::new()
            .raw(&[0xAA, 0xBB, 0xCC])
            .build();
        assert_eq!(image.get_u8(SCODE_HEADER_SIZE), Some(0xAA));
        assert_eq!(image.get_u8(SCODE_HEADER_SIZE + 1), Some(0xBB));
        assert_eq!(image.get_u8(SCODE_HEADER_SIZE + 2), Some(0xCC));
    }

    #[test]
    fn byte_emits_single_byte() {
        let image = ScodeBuilder::new().byte(0xFF).build();
        assert_eq!(image.get_u8(SCODE_HEADER_SIZE), Some(0xFF));
    }

    #[test]
    fn offset_tracks_code_position() {
        let b = ScodeBuilder::new();
        assert_eq!(b.offset(), 0);

        let b = b.op(Opcode::Nop); // 1 byte
        assert_eq!(b.offset(), 1);

        let b = b.op_u8(Opcode::LoadIntU1, 10); // 2 more bytes
        assert_eq!(b.offset(), 3);

        let b = b.op_u16(Opcode::LoadIntU2, 0x1234); // 3 more bytes
        assert_eq!(b.offset(), 6);

        let b = b.op_u32(Opcode::Nop, 0); // 5 more bytes
        assert_eq!(b.offset(), 11);
    }

    #[test]
    fn data_size_is_configurable() {
        let image = ScodeBuilder::new().data_size(1024).build();
        assert_eq!(image.header.data_size, 1024);
    }

    #[test]
    fn build_memory_creates_usable_memory() {
        let mem = ScodeBuilder::new()
            .op(Opcode::LoadI0)
            .op(Opcode::LoadI1)
            .data_size(512)
            .build_memory();
        // Should be able to read the code bytes from memory
        let byte = mem.code_u8(SCODE_HEADER_SIZE).unwrap();
        assert_eq!(byte, Opcode::LoadI0 as u8);
        let byte2 = mem.code_u8(SCODE_HEADER_SIZE + 1).unwrap();
        assert_eq!(byte2, Opcode::LoadI1 as u8);
    }

    #[test]
    fn main_method_points_to_code_start() {
        let image = ScodeBuilder::new().op(Opcode::Nop).build();
        // main_method block index should convert to SCODE_HEADER_SIZE offset
        let main_offset = image.block_to_offset(image.header.main_method);
        assert_eq!(main_offset, SCODE_HEADER_SIZE);
    }

    #[test]
    fn chained_operations_produce_correct_sequence() {
        let image = ScodeBuilder::new()
            .op(Opcode::LoadI1)     // push 1
            .op(Opcode::LoadI2)     // push 2
            .op(Opcode::IntAdd)     // pop 2, push 3
            .build();
        let base = SCODE_HEADER_SIZE;
        assert_eq!(image.get_u8(base), Some(Opcode::LoadI1 as u8));
        assert_eq!(image.get_u8(base + 1), Some(Opcode::LoadI2 as u8));
        assert_eq!(image.get_u8(base + 2), Some(Opcode::IntAdd as u8));
        assert_eq!(image.len(), base + 3);
    }

    #[test]
    fn default_data_size_is_256() {
        let image = ScodeBuilder::new().build();
        assert_eq!(image.header.data_size, 256);
    }

    #[test]
    fn empty_builder_produces_header_only() {
        let image = ScodeBuilder::new().build();
        assert_eq!(image.len(), SCODE_HEADER_SIZE);
        assert_eq!(image.header.image_size as usize, SCODE_HEADER_SIZE);
    }
}
