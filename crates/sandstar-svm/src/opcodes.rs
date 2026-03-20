//! Sedona VM bytecode opcodes.
//!
//! Complete opcode enum matching the C definitions in `scode.h`.
//! Each variant preserves the exact numeric value from the C `#define`.

use std::fmt;

/// Total number of opcodes defined in the Sedona VM.
pub const NUM_OPCODES: usize = 240;

/// All Sedona VM bytecode opcodes.
///
/// Numeric values match the C `#define` constants in `scode.h` exactly.
/// Grouped by category following the C code structure.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Opcode {
    // ====================================================================
    // Literals
    // ====================================================================
    Nop = 0,
    /// Push -1 onto stack
    LoadIM1 = 1,
    /// Push 0 onto stack
    LoadI0 = 2,
    /// Push 1 onto stack
    LoadI1 = 3,
    /// Push 2 onto stack
    LoadI2 = 4,
    /// Push 3 onto stack
    LoadI3 = 5,
    /// Push 4 onto stack
    LoadI4 = 6,
    /// Push 5 onto stack
    LoadI5 = 7,
    /// Push unsigned byte operand onto stack
    LoadIntU1 = 8,
    /// Push unsigned u16 operand onto stack
    LoadIntU2 = 9,
    /// Push long 0 onto stack (wide)
    LoadL0 = 10,
    /// Push long 1 onto stack (wide)
    LoadL1 = 11,
    /// Push float 0.0 onto stack
    LoadF0 = 12,
    /// Push float 1.0 onto stack
    LoadF1 = 13,
    /// Push double 0.0 onto stack (wide)
    LoadD0 = 14,
    /// Push double 1.0 onto stack (wide)
    LoadD1 = 15,
    /// Push null (0) onto stack
    LoadNull = 16,
    /// Push null bool (2) onto stack
    LoadNullBool = 17,
    /// Push null float (NaN sentinel) onto stack
    LoadNullFloat = 18,
    /// Push null double (NaN sentinel) onto stack (wide)
    LoadNullDouble = 19,
    /// Load 32-bit int from code block (u16 block index operand)
    LoadInt = 20,
    /// Load 32-bit float from code block (u16 block index operand)
    LoadFloat = 21,
    /// Load 64-bit long from code block (u16 block index operand, wide)
    LoadLong = 22,
    /// Load 64-bit double from code block (u16 block index operand, wide)
    LoadDouble = 23,
    /// Load string pointer from code block (u16 block index operand)
    LoadStr = 24,
    /// Load buffer pointer from code block (u16 block index operand)
    LoadBuf = 25,
    /// Load type pointer from code block (u16 block index operand)
    LoadType = 26,
    /// Load slot pointer from code block (u16 block index operand)
    LoadSlot = 27,
    /// Load define constant (IR only, not in scode image)
    LoadDefine = 28,

    // ====================================================================
    // Load params
    // ====================================================================
    /// Load param 0 onto stack
    LoadParam0 = 29,
    /// Load param 1 onto stack
    LoadParam1 = 30,
    /// Load param 2 onto stack
    LoadParam2 = 31,
    /// Load param 3 onto stack
    LoadParam3 = 32,
    /// Load param N onto stack (u8 index operand)
    LoadParam = 33,
    /// Load wide param N onto stack (u8 index operand, 64-bit)
    LoadParamWide = 34,

    // ====================================================================
    // Store params
    // ====================================================================
    /// Store top of stack to param N (u8 index operand)
    StoreParam = 35,
    /// Store wide top of stack to param N (u8 index operand, 64-bit)
    StoreParamWide = 36,

    // ====================================================================
    // Load locals
    // ====================================================================
    /// Load local 0 onto stack
    LoadLocal0 = 37,
    /// Load local 1 onto stack
    LoadLocal1 = 38,
    /// Load local 2 onto stack
    LoadLocal2 = 39,
    /// Load local 3 onto stack
    LoadLocal3 = 40,
    /// Load local 4 onto stack
    LoadLocal4 = 41,
    /// Load local 5 onto stack
    LoadLocal5 = 42,
    /// Load local 6 onto stack
    LoadLocal6 = 43,
    /// Load local 7 onto stack
    LoadLocal7 = 44,
    /// Load local N onto stack (u8 index operand)
    LoadLocal = 45,
    /// Load wide local N onto stack (u8 index operand, 64-bit)
    LoadLocalWide = 46,

    // ====================================================================
    // Store locals
    // ====================================================================
    /// Store top of stack to local 0
    StoreLocal0 = 47,
    /// Store top of stack to local 1
    StoreLocal1 = 48,
    /// Store top of stack to local 2
    StoreLocal2 = 49,
    /// Store top of stack to local 3
    StoreLocal3 = 50,
    /// Store top of stack to local 4
    StoreLocal4 = 51,
    /// Store top of stack to local 5
    StoreLocal5 = 52,
    /// Store top of stack to local 6
    StoreLocal6 = 53,
    /// Store top of stack to local 7
    StoreLocal7 = 54,
    /// Store top of stack to local N (u8 index operand)
    StoreLocal = 55,
    /// Store wide top of stack to local N (u8 index operand, 64-bit)
    StoreLocalWide = 56,

    // ====================================================================
    // Int compare
    // ====================================================================
    IntEq = 57,
    IntNotEq = 58,
    IntGt = 59,
    IntGtEq = 60,
    IntLt = 61,
    IntLtEq = 62,

    // ====================================================================
    // Int math
    // ====================================================================
    IntMul = 63,
    IntDiv = 64,
    IntMod = 65,
    IntAdd = 66,
    IntSub = 67,
    IntOr = 68,
    IntXor = 69,
    IntAnd = 70,
    IntNot = 71,
    IntNeg = 72,
    IntShiftL = 73,
    IntShiftR = 74,
    IntInc = 75,
    IntDec = 76,

    // ====================================================================
    // Long compare
    // ====================================================================
    LongEq = 77,
    LongNotEq = 78,
    LongGt = 79,
    LongGtEq = 80,
    LongLt = 81,
    LongLtEq = 82,

    // ====================================================================
    // Long math
    // ====================================================================
    LongMul = 83,
    LongDiv = 84,
    LongMod = 85,
    LongAdd = 86,
    LongSub = 87,
    LongOr = 88,
    LongXor = 89,
    LongAnd = 90,
    LongNot = 91,
    LongNeg = 92,
    LongShiftL = 93,
    LongShiftR = 94,

    // ====================================================================
    // Float compare
    // ====================================================================
    FloatEq = 95,
    FloatNotEq = 96,
    FloatGt = 97,
    FloatGtEq = 98,
    FloatLt = 99,
    FloatLtEq = 100,

    // ====================================================================
    // Float math
    // ====================================================================
    FloatMul = 101,
    FloatDiv = 102,
    FloatAdd = 103,
    FloatSub = 104,
    FloatNeg = 105,

    // ====================================================================
    // Double compare
    // ====================================================================
    DoubleEq = 106,
    DoubleNotEq = 107,
    DoubleGt = 108,
    DoubleGtEq = 109,
    DoubleLt = 110,
    DoubleLtEq = 111,

    // ====================================================================
    // Double math
    // ====================================================================
    DoubleMul = 112,
    DoubleDiv = 113,
    DoubleAdd = 114,
    DoubleSub = 115,
    DoubleNeg = 116,

    // ====================================================================
    // Casts
    // ====================================================================
    IntToFloat = 117,
    IntToLong = 118,
    IntToDouble = 119,
    LongToInt = 120,
    LongToFloat = 121,
    LongToDouble = 122,
    FloatToInt = 123,
    FloatToLong = 124,
    FloatToDouble = 125,
    DoubleToInt = 126,
    DoubleToLong = 127,
    DoubleToFloat = 128,

    // ====================================================================
    // Object compare
    // ====================================================================
    ObjEq = 129,
    ObjNotEq = 130,

    // ====================================================================
    // General purpose compare
    // ====================================================================
    EqZero = 131,
    NotEqZero = 132,

    // ====================================================================
    // Stack manipulation
    // ====================================================================
    Pop = 133,
    Pop2 = 134,
    Pop3 = 135,
    Dup = 136,
    Dup2 = 137,
    DupDown2 = 138,
    DupDown3 = 139,
    Dup2Down2 = 140,
    Dup2Down3 = 141,

    // ====================================================================
    // Branching (near -- 1-byte signed offset)
    // ====================================================================
    /// Unconditional near jump (i8 offset operand)
    Jump = 142,
    /// Jump if top of stack != 0 (i8 offset operand)
    JumpNonZero = 143,
    /// Jump if top of stack == 0 (i8 offset operand)
    JumpZero = 144,
    /// Foreach loop iteration (i8 offset operand)
    Foreach = 145,

    // ====================================================================
    // Branching (far -- 2-byte signed offset)
    // ====================================================================
    /// Unconditional far jump (i16 offset operand)
    JumpFar = 146,
    /// Far jump if top of stack != 0 (i16 offset operand)
    JumpFarNonZero = 147,
    /// Far jump if top of stack == 0 (i16 offset operand)
    JumpFarZero = 148,
    /// Foreach loop iteration, far (i16 offset operand)
    ForeachFar = 149,

    // ====================================================================
    // Int compare branching (near -- 1-byte signed offset)
    // ====================================================================
    JumpIntEq = 150,
    JumpIntNotEq = 151,
    JumpIntGt = 152,
    JumpIntGtEq = 153,
    JumpIntLt = 154,
    JumpIntLtEq = 155,

    // ====================================================================
    // Int compare branching (far -- 2-byte signed offset)
    // ====================================================================
    JumpFarIntEq = 156,
    JumpFarIntNotEq = 157,
    JumpFarIntGt = 158,
    JumpFarIntGtEq = 159,
    JumpFarIntLt = 160,
    JumpFarIntLtEq = 161,

    // ====================================================================
    // Storage -- load static data base address
    // ====================================================================
    /// Push static data base address onto stack
    LoadDataAddr = 162,

    // ====================================================================
    // 8-bit storage (bytes, bools)
    // ====================================================================
    /// Load 8-bit field with u8 offset
    Load8BitFieldU1 = 163,
    /// Load 8-bit field with u16 offset
    Load8BitFieldU2 = 164,
    /// Load 8-bit field with u32 offset
    Load8BitFieldU4 = 165,
    /// Load 8-bit array element (index on stack)
    Load8BitArray = 166,
    /// Store 8-bit field with u8 offset
    Store8BitFieldU1 = 167,
    /// Store 8-bit field with u16 offset
    Store8BitFieldU2 = 168,
    /// Store 8-bit field with u32 offset
    Store8BitFieldU4 = 169,
    /// Store 8-bit array element (index on stack)
    Store8BitArray = 170,
    /// Pointer arithmetic for 8-bit array
    Add8BitArray = 171,

    // ====================================================================
    // 16-bit storage (shorts)
    // ====================================================================
    Load16BitFieldU1 = 172,
    Load16BitFieldU2 = 173,
    Load16BitFieldU4 = 174,
    Load16BitArray = 175,
    Store16BitFieldU1 = 176,
    Store16BitFieldU2 = 177,
    Store16BitFieldU4 = 178,
    Store16BitArray = 179,
    Add16BitArray = 180,

    // ====================================================================
    // 32-bit storage (int/float)
    // ====================================================================
    Load32BitFieldU1 = 181,
    Load32BitFieldU2 = 182,
    Load32BitFieldU4 = 183,
    Load32BitArray = 184,
    Store32BitFieldU1 = 185,
    Store32BitFieldU2 = 186,
    Store32BitFieldU4 = 187,
    Store32BitArray = 188,
    Add32BitArray = 189,

    // ====================================================================
    // 64-bit storage (long/double)
    // ====================================================================
    Load64BitFieldU1 = 190,
    Load64BitFieldU2 = 191,
    Load64BitFieldU4 = 192,
    Load64BitArray = 193,
    Store64BitFieldU1 = 194,
    Store64BitFieldU2 = 195,
    Store64BitFieldU4 = 196,
    Store64BitArray = 197,
    Add64BitArray = 198,

    // ====================================================================
    // Ref storage (pointers -- variable width)
    // ====================================================================
    LoadRefFieldU1 = 199,
    LoadRefFieldU2 = 200,
    LoadRefFieldU4 = 201,
    LoadRefArray = 202,
    StoreRefFieldU1 = 203,
    StoreRefFieldU2 = 204,
    StoreRefFieldU4 = 205,
    StoreRefArray = 206,
    AddRefArray = 207,

    // ====================================================================
    // Const storage (block index)
    // ====================================================================
    LoadConstFieldU1 = 208,
    LoadConstFieldU2 = 209,
    LoadConstStatic = 210,
    LoadConstArray = 211,

    // ====================================================================
    // Inline storage (pointer addition)
    // ====================================================================
    LoadInlineFieldU1 = 212,
    LoadInlineFieldU2 = 213,
    LoadInlineFieldU4 = 214,

    // ====================================================================
    // Param0 + inline storage
    // ====================================================================
    LoadParam0InlineFieldU1 = 215,
    LoadParam0InlineFieldU2 = 216,
    LoadParam0InlineFieldU4 = 217,

    // ====================================================================
    // Static + inline storage
    // ====================================================================
    LoadDataInlineFieldU1 = 218,
    LoadDataInlineFieldU2 = 219,
    LoadDataInlineFieldU4 = 220,

    // ====================================================================
    // Call control
    // ====================================================================
    /// Non-virtual method call (u16 block index operand)
    Call = 221,
    /// Virtual method call (u16 method index + u8 num params)
    CallVirtual = 222,
    /// Native method call returning 32-bit (u8 kit, u8 method, u8 params)
    CallNative = 223,
    /// Native method call returning 64-bit (u8 kit, u8 method, u8 params)
    CallNativeWide = 224,
    /// Native method call returning void (u8 kit, u8 method, u8 params)
    CallNativeVoid = 225,
    /// Return from void method
    ReturnVoid = 226,
    /// Return 32-bit value
    ReturnPop = 227,
    /// Return 64-bit value
    ReturnPopWide = 228,
    /// Load param0 then fall through to Call
    LoadParam0Call = 229,

    // ====================================================================
    // Misc
    // ====================================================================
    /// Initialize inline object array pointers
    InitArray = 230,
    /// Initialize virtual table pointer (u16 block index operand)
    InitVirt = 231,
    /// Initialize component type ID (u16 block index operand)
    InitComp = 232,
    /// SizeOf operator (IR only, never in scode image)
    SizeOf = 233,
    /// Assert with line number (u16 line number operand)
    Assert = 234,
    /// Switch/jump table (u16 num entries, then u16*num jump offsets)
    Switch = 235,
    /// Debug metadata slot name (u16 block index operand)
    MetaSlot = 236,
    /// Type cast (Java bytecode only, never in scode image)
    Cast = 237,
    /// Load array literal (replaced by LoadBuf, never in scode image)
    LoadArrayLiteral = 238,
    /// Load slot ID (IR only, never in scode image)
    LoadSlotId = 239,
}

impl TryFrom<u8> for Opcode {
    type Error = u8;

    /// Convert a raw bytecode byte to an `Opcode`.
    ///
    /// Returns `Err(byte)` if the byte does not correspond to a valid opcode.
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value >= NUM_OPCODES as u8 {
            return Err(value);
        }
        // SAFETY: All values 0..240 are defined in the enum with #[repr(u8)],
        // and we've checked the bounds above.
        Ok(unsafe { std::mem::transmute::<u8, Opcode>(value) })
    }
}

impl fmt::Display for Opcode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl Opcode {
    /// Returns `true` for opcodes that produce a 64-bit (wide) return value.
    ///
    /// This includes `ReturnPopWide` and `CallNativeWide`.
    pub fn is_wide_return(&self) -> bool {
        matches!(self, Opcode::ReturnPopWide | Opcode::CallNativeWide)
    }

    /// Returns `true` for jump and branch opcodes.
    pub fn is_branch(&self) -> bool {
        matches!(
            self,
            Opcode::Jump
                | Opcode::JumpNonZero
                | Opcode::JumpZero
                | Opcode::Foreach
                | Opcode::JumpFar
                | Opcode::JumpFarNonZero
                | Opcode::JumpFarZero
                | Opcode::ForeachFar
                | Opcode::JumpIntEq
                | Opcode::JumpIntNotEq
                | Opcode::JumpIntGt
                | Opcode::JumpIntGtEq
                | Opcode::JumpIntLt
                | Opcode::JumpIntLtEq
                | Opcode::JumpFarIntEq
                | Opcode::JumpFarIntNotEq
                | Opcode::JumpFarIntGt
                | Opcode::JumpFarIntGtEq
                | Opcode::JumpFarIntLt
                | Opcode::JumpFarIntLtEq
        )
    }

    /// Returns the number of operand bytes that follow this opcode in the bytecode stream.
    ///
    /// - 0: opcode only (e.g., `Nop`, `IntAdd`, `Pop`)
    /// - 1: 1-byte operand (e.g., `LoadIntU1`, `LoadParam`, near jumps)
    /// - 2: 2-byte operand (e.g., `LoadIntU2`, `Call`, far jumps, `Assert`)
    /// - 3: 3-byte operand (e.g., `CallNative` -- kit + method + params)
    /// - 4: 4-byte operand (e.g., `*FieldU4` storage opcodes)
    ///
    /// Note: `Switch` returns 2 for the initial `num` field; the actual size is
    /// variable (2 + num*2). `CallVirtual` returns 3 (u16 index + u8 params).
    /// `LoadParam0Call` returns 2 (same as `Call`, which it falls through to).
    pub fn operand_bytes(&self) -> usize {
        match self {
            // 0 bytes -- no operand
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
            | Opcode::LoadDataAddr
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
            | Opcode::LoadConstArray
            | Opcode::ReturnVoid
            | Opcode::ReturnPop
            | Opcode::ReturnPopWide
            | Opcode::InitArray
            | Opcode::LoadArrayLiteral
            | Opcode::Cast
            | Opcode::SizeOf
            | Opcode::LoadDefine => 0,

            // 1 byte -- u8 operand (param/local index, near jump i8 offset, field U1 offset)
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
            | Opcode::Foreach
            | Opcode::JumpIntEq
            | Opcode::JumpIntNotEq
            | Opcode::JumpIntGt
            | Opcode::JumpIntGtEq
            | Opcode::JumpIntLt
            | Opcode::JumpIntLtEq
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
            | Opcode::LoadDataInlineFieldU1 => 1,

            // 2 bytes -- u16 operand (block index, far jump i16 offset, field U2 offset)
            Opcode::LoadIntU2
            | Opcode::LoadInt
            | Opcode::LoadFloat
            | Opcode::LoadLong
            | Opcode::LoadDouble
            | Opcode::LoadStr
            | Opcode::LoadBuf
            | Opcode::LoadType
            | Opcode::LoadSlot
            | Opcode::JumpFar
            | Opcode::JumpFarNonZero
            | Opcode::JumpFarZero
            | Opcode::ForeachFar
            | Opcode::JumpFarIntEq
            | Opcode::JumpFarIntNotEq
            | Opcode::JumpFarIntGt
            | Opcode::JumpFarIntGtEq
            | Opcode::JumpFarIntLt
            | Opcode::JumpFarIntLtEq
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
            | Opcode::LoadConstStatic
            | Opcode::LoadInlineFieldU2
            | Opcode::LoadParam0InlineFieldU2
            | Opcode::LoadDataInlineFieldU2
            | Opcode::Call
            | Opcode::LoadParam0Call
            | Opcode::InitVirt
            | Opcode::InitComp
            | Opcode::Assert
            | Opcode::Switch
            | Opcode::MetaSlot
            | Opcode::LoadSlotId => 2,

            // 3 bytes -- u16 + u8 (CallVirtual: u16 method index + u8 num params)
            //            u8 + u8 + u8 (CallNative*: kit + method + params)
            Opcode::CallVirtual
            | Opcode::CallNative
            | Opcode::CallNativeWide
            | Opcode::CallNativeVoid => 3,

            // 4 bytes -- u32 operand (field U4 storage)
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
            | Opcode::LoadDataInlineFieldU4 => 4,
        }
    }

    /// Returns the opcode name as a static string, matching the C `OpcodeNames` array.
    pub fn name(&self) -> &'static str {
        OPCODE_NAMES[*self as usize]
    }
}

/// Opcode name lookup table, indexed by opcode byte value.
static OPCODE_NAMES: [&str; NUM_OPCODES] = [
    "Nop",                     // 0
    "LoadIM1",                 // 1
    "LoadI0",                  // 2
    "LoadI1",                  // 3
    "LoadI2",                  // 4
    "LoadI3",                  // 5
    "LoadI4",                  // 6
    "LoadI5",                  // 7
    "LoadIntU1",               // 8
    "LoadIntU2",               // 9
    "LoadL0",                  // 10
    "LoadL1",                  // 11
    "LoadF0",                  // 12
    "LoadF1",                  // 13
    "LoadD0",                  // 14
    "LoadD1",                  // 15
    "LoadNull",                // 16
    "LoadNullBool",            // 17
    "LoadNullFloat",           // 18
    "LoadNullDouble",          // 19
    "LoadInt",                 // 20
    "LoadFloat",               // 21
    "LoadLong",                // 22
    "LoadDouble",              // 23
    "LoadStr",                 // 24
    "LoadBuf",                 // 25
    "LoadType",                // 26
    "LoadSlot",                // 27
    "LoadDefine",              // 28
    "LoadParam0",              // 29
    "LoadParam1",              // 30
    "LoadParam2",              // 31
    "LoadParam3",              // 32
    "LoadParam",               // 33
    "LoadParamWide",           // 34
    "StoreParam",              // 35
    "StoreParamWide",          // 36
    "LoadLocal0",              // 37
    "LoadLocal1",              // 38
    "LoadLocal2",              // 39
    "LoadLocal3",              // 40
    "LoadLocal4",              // 41
    "LoadLocal5",              // 42
    "LoadLocal6",              // 43
    "LoadLocal7",              // 44
    "LoadLocal",               // 45
    "LoadLocalWide",           // 46
    "StoreLocal0",             // 47
    "StoreLocal1",             // 48
    "StoreLocal2",             // 49
    "StoreLocal3",             // 50
    "StoreLocal4",             // 51
    "StoreLocal5",             // 52
    "StoreLocal6",             // 53
    "StoreLocal7",             // 54
    "StoreLocal",              // 55
    "StoreLocalWide",          // 56
    "IntEq",                   // 57
    "IntNotEq",                // 58
    "IntGt",                   // 59
    "IntGtEq",                 // 60
    "IntLt",                   // 61
    "IntLtEq",                 // 62
    "IntMul",                  // 63
    "IntDiv",                  // 64
    "IntMod",                  // 65
    "IntAdd",                  // 66
    "IntSub",                  // 67
    "IntOr",                   // 68
    "IntXor",                  // 69
    "IntAnd",                  // 70
    "IntNot",                  // 71
    "IntNeg",                  // 72
    "IntShiftL",               // 73
    "IntShiftR",               // 74
    "IntInc",                  // 75
    "IntDec",                  // 76
    "LongEq",                  // 77
    "LongNotEq",               // 78
    "LongGt",                  // 79
    "LongGtEq",                // 80
    "LongLt",                  // 81
    "LongLtEq",                // 82
    "LongMul",                 // 83
    "LongDiv",                 // 84
    "LongMod",                 // 85
    "LongAdd",                 // 86
    "LongSub",                 // 87
    "LongOr",                  // 88
    "LongXor",                 // 89
    "LongAnd",                 // 90
    "LongNot",                 // 91
    "LongNeg",                 // 92
    "LongShiftL",              // 93
    "LongShiftR",              // 94
    "FloatEq",                 // 95
    "FloatNotEq",              // 96
    "FloatGt",                 // 97
    "FloatGtEq",               // 98
    "FloatLt",                 // 99
    "FloatLtEq",               // 100
    "FloatMul",                // 101
    "FloatDiv",                // 102
    "FloatAdd",                // 103
    "FloatSub",                // 104
    "FloatNeg",                // 105
    "DoubleEq",                // 106
    "DoubleNotEq",             // 107
    "DoubleGt",                // 108
    "DoubleGtEq",              // 109
    "DoubleLt",                // 110
    "DoubleLtEq",              // 111
    "DoubleMul",               // 112
    "DoubleDiv",               // 113
    "DoubleAdd",               // 114
    "DoubleSub",               // 115
    "DoubleNeg",               // 116
    "IntToFloat",              // 117
    "IntToLong",               // 118
    "IntToDouble",             // 119
    "LongToInt",               // 120
    "LongToFloat",             // 121
    "LongToDouble",            // 122
    "FloatToInt",              // 123
    "FloatToLong",             // 124
    "FloatToDouble",           // 125
    "DoubleToInt",             // 126
    "DoubleToLong",            // 127
    "DoubleToFloat",           // 128
    "ObjEq",                   // 129
    "ObjNotEq",                // 130
    "EqZero",                  // 131
    "NotEqZero",               // 132
    "Pop",                     // 133
    "Pop2",                    // 134
    "Pop3",                    // 135
    "Dup",                     // 136
    "Dup2",                    // 137
    "DupDown2",                // 138
    "DupDown3",                // 139
    "Dup2Down2",               // 140
    "Dup2Down3",               // 141
    "Jump",                    // 142
    "JumpNonZero",             // 143
    "JumpZero",                // 144
    "Foreach",                 // 145
    "JumpFar",                 // 146
    "JumpFarNonZero",          // 147
    "JumpFarZero",             // 148
    "ForeachFar",              // 149
    "JumpIntEq",               // 150
    "JumpIntNotEq",            // 151
    "JumpIntGt",               // 152
    "JumpIntGtEq",             // 153
    "JumpIntLt",               // 154
    "JumpIntLtEq",             // 155
    "JumpFarIntEq",            // 156
    "JumpFarIntNotEq",         // 157
    "JumpFarIntGt",            // 158
    "JumpFarIntGtEq",          // 159
    "JumpFarIntLt",            // 160
    "JumpFarIntLtEq",          // 161
    "LoadDataAddr",            // 162
    "Load8BitFieldU1",         // 163
    "Load8BitFieldU2",         // 164
    "Load8BitFieldU4",         // 165
    "Load8BitArray",           // 166
    "Store8BitFieldU1",        // 167
    "Store8BitFieldU2",        // 168
    "Store8BitFieldU4",        // 169
    "Store8BitArray",          // 170
    "Add8BitArray",            // 171
    "Load16BitFieldU1",        // 172
    "Load16BitFieldU2",        // 173
    "Load16BitFieldU4",        // 174
    "Load16BitArray",          // 175
    "Store16BitFieldU1",       // 176
    "Store16BitFieldU2",       // 177
    "Store16BitFieldU4",       // 178
    "Store16BitArray",         // 179
    "Add16BitArray",           // 180
    "Load32BitFieldU1",        // 181
    "Load32BitFieldU2",        // 182
    "Load32BitFieldU4",        // 183
    "Load32BitArray",          // 184
    "Store32BitFieldU1",       // 185
    "Store32BitFieldU2",       // 186
    "Store32BitFieldU4",       // 187
    "Store32BitArray",         // 188
    "Add32BitArray",           // 189
    "Load64BitFieldU1",        // 190
    "Load64BitFieldU2",        // 191
    "Load64BitFieldU4",        // 192
    "Load64BitArray",          // 193
    "Store64BitFieldU1",       // 194
    "Store64BitFieldU2",       // 195
    "Store64BitFieldU4",       // 196
    "Store64BitArray",         // 197
    "Add64BitArray",           // 198
    "LoadRefFieldU1",          // 199
    "LoadRefFieldU2",          // 200
    "LoadRefFieldU4",          // 201
    "LoadRefArray",            // 202
    "StoreRefFieldU1",         // 203
    "StoreRefFieldU2",         // 204
    "StoreRefFieldU4",         // 205
    "StoreRefArray",           // 206
    "AddRefArray",             // 207
    "LoadConstFieldU1",        // 208
    "LoadConstFieldU2",        // 209
    "LoadConstStatic",         // 210
    "LoadConstArray",          // 211
    "LoadInlineFieldU1",       // 212
    "LoadInlineFieldU2",       // 213
    "LoadInlineFieldU4",       // 214
    "LoadParam0InlineFieldU1", // 215
    "LoadParam0InlineFieldU2", // 216
    "LoadParam0InlineFieldU4", // 217
    "LoadDataInlineFieldU1",   // 218
    "LoadDataInlineFieldU2",   // 219
    "LoadDataInlineFieldU4",   // 220
    "Call",                    // 221
    "CallVirtual",             // 222
    "CallNative",              // 223
    "CallNativeWide",          // 224
    "CallNativeVoid",          // 225
    "ReturnVoid",              // 226
    "ReturnPop",               // 227
    "ReturnPopWide",           // 228
    "LoadParam0Call",          // 229
    "InitArray",               // 230
    "InitVirt",                // 231
    "InitComp",                // 232
    "SizeOf",                  // 233
    "Assert",                  // 234
    "Switch",                  // 235
    "MetaSlot",                // 236
    "Cast",                    // 237
    "LoadArrayLiteral",        // 238
    "LoadSlotId",              // 239
];

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_opcode_count() {
        assert_eq!(NUM_OPCODES, 240);
    }

    #[test]
    fn test_try_from_valid() {
        assert_eq!(Opcode::try_from(0), Ok(Opcode::Nop));
        assert_eq!(Opcode::try_from(1), Ok(Opcode::LoadIM1));
        assert_eq!(Opcode::try_from(16), Ok(Opcode::LoadNull));
        assert_eq!(Opcode::try_from(66), Ok(Opcode::IntAdd));
        assert_eq!(Opcode::try_from(142), Ok(Opcode::Jump));
        assert_eq!(Opcode::try_from(221), Ok(Opcode::Call));
        assert_eq!(Opcode::try_from(239), Ok(Opcode::LoadSlotId));
    }

    #[test]
    fn test_try_from_invalid() {
        assert_eq!(Opcode::try_from(240), Err(240));
        assert_eq!(Opcode::try_from(255), Err(255));
    }

    #[test]
    fn test_numeric_values_match_c_defines() {
        assert_eq!(Opcode::Nop as u8, 0);
        assert_eq!(Opcode::LoadIntU1 as u8, 8);
        assert_eq!(Opcode::LoadIntU2 as u8, 9);
        assert_eq!(Opcode::LoadNull as u8, 16);
        assert_eq!(Opcode::LoadInt as u8, 20);
        assert_eq!(Opcode::LoadParam0 as u8, 29);
        assert_eq!(Opcode::StoreParam as u8, 35);
        assert_eq!(Opcode::LoadLocal0 as u8, 37);
        assert_eq!(Opcode::StoreLocal0 as u8, 47);
        assert_eq!(Opcode::IntEq as u8, 57);
        assert_eq!(Opcode::IntAdd as u8, 66);
        assert_eq!(Opcode::LongEq as u8, 77);
        assert_eq!(Opcode::FloatEq as u8, 95);
        assert_eq!(Opcode::DoubleEq as u8, 106);
        assert_eq!(Opcode::IntToFloat as u8, 117);
        assert_eq!(Opcode::DoubleToFloat as u8, 128);
        assert_eq!(Opcode::ObjEq as u8, 129);
        assert_eq!(Opcode::EqZero as u8, 131);
        assert_eq!(Opcode::Pop as u8, 133);
        assert_eq!(Opcode::Dup as u8, 136);
        assert_eq!(Opcode::Jump as u8, 142);
        assert_eq!(Opcode::JumpFar as u8, 146);
        assert_eq!(Opcode::JumpIntEq as u8, 150);
        assert_eq!(Opcode::JumpFarIntEq as u8, 156);
        assert_eq!(Opcode::LoadDataAddr as u8, 162);
        assert_eq!(Opcode::Load8BitFieldU1 as u8, 163);
        assert_eq!(Opcode::Load16BitFieldU1 as u8, 172);
        assert_eq!(Opcode::Load32BitFieldU1 as u8, 181);
        assert_eq!(Opcode::Load64BitFieldU1 as u8, 190);
        assert_eq!(Opcode::LoadRefFieldU1 as u8, 199);
        assert_eq!(Opcode::LoadConstFieldU1 as u8, 208);
        assert_eq!(Opcode::LoadInlineFieldU1 as u8, 212);
        assert_eq!(Opcode::LoadParam0InlineFieldU1 as u8, 215);
        assert_eq!(Opcode::LoadDataInlineFieldU1 as u8, 218);
        assert_eq!(Opcode::Call as u8, 221);
        assert_eq!(Opcode::CallVirtual as u8, 222);
        assert_eq!(Opcode::CallNative as u8, 223);
        assert_eq!(Opcode::CallNativeWide as u8, 224);
        assert_eq!(Opcode::CallNativeVoid as u8, 225);
        assert_eq!(Opcode::ReturnVoid as u8, 226);
        assert_eq!(Opcode::ReturnPop as u8, 227);
        assert_eq!(Opcode::ReturnPopWide as u8, 228);
        assert_eq!(Opcode::LoadParam0Call as u8, 229);
        assert_eq!(Opcode::InitArray as u8, 230);
        assert_eq!(Opcode::InitVirt as u8, 231);
        assert_eq!(Opcode::InitComp as u8, 232);
        assert_eq!(Opcode::SizeOf as u8, 233);
        assert_eq!(Opcode::Assert as u8, 234);
        assert_eq!(Opcode::Switch as u8, 235);
        assert_eq!(Opcode::MetaSlot as u8, 236);
        assert_eq!(Opcode::Cast as u8, 237);
        assert_eq!(Opcode::LoadArrayLiteral as u8, 238);
        assert_eq!(Opcode::LoadSlotId as u8, 239);
    }

    #[test]
    fn test_all_values_0_to_239_are_valid() {
        for i in 0u8..240 {
            assert!(
                Opcode::try_from(i).is_ok(),
                "Opcode {} should be valid",
                i
            );
        }
    }

    #[test]
    fn test_roundtrip_u8() {
        for i in 0u8..240 {
            let op = Opcode::try_from(i).unwrap();
            assert_eq!(op as u8, i);
        }
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", Opcode::Nop), "Nop");
        assert_eq!(format!("{}", Opcode::IntAdd), "IntAdd");
        assert_eq!(format!("{}", Opcode::CallVirtual), "CallVirtual");
        assert_eq!(
            format!("{}", Opcode::LoadParam0InlineFieldU2),
            "LoadParam0InlineFieldU2"
        );
    }

    #[test]
    fn test_name_matches_display() {
        for i in 0u8..240 {
            let op = Opcode::try_from(i).unwrap();
            assert_eq!(op.name(), &format!("{}", op));
        }
    }

    #[test]
    fn test_is_wide_return() {
        assert!(Opcode::ReturnPopWide.is_wide_return());
        assert!(Opcode::CallNativeWide.is_wide_return());
        assert!(!Opcode::ReturnPop.is_wide_return());
        assert!(!Opcode::ReturnVoid.is_wide_return());
        assert!(!Opcode::CallNative.is_wide_return());
        assert!(!Opcode::CallNativeVoid.is_wide_return());
    }

    #[test]
    fn test_is_branch() {
        // Near jumps
        assert!(Opcode::Jump.is_branch());
        assert!(Opcode::JumpNonZero.is_branch());
        assert!(Opcode::JumpZero.is_branch());
        assert!(Opcode::Foreach.is_branch());
        // Far jumps
        assert!(Opcode::JumpFar.is_branch());
        assert!(Opcode::JumpFarNonZero.is_branch());
        assert!(Opcode::JumpFarZero.is_branch());
        assert!(Opcode::ForeachFar.is_branch());
        // Int compare near jumps
        assert!(Opcode::JumpIntEq.is_branch());
        assert!(Opcode::JumpIntLtEq.is_branch());
        // Int compare far jumps
        assert!(Opcode::JumpFarIntEq.is_branch());
        assert!(Opcode::JumpFarIntLtEq.is_branch());
        // Non-branch opcodes
        assert!(!Opcode::Nop.is_branch());
        assert!(!Opcode::IntAdd.is_branch());
        assert!(!Opcode::Call.is_branch());
        assert!(!Opcode::ReturnPop.is_branch());
        assert!(!Opcode::Switch.is_branch());
    }

    #[test]
    fn test_operand_bytes_zero() {
        assert_eq!(Opcode::Nop.operand_bytes(), 0);
        assert_eq!(Opcode::LoadIM1.operand_bytes(), 0);
        assert_eq!(Opcode::LoadI0.operand_bytes(), 0);
        assert_eq!(Opcode::LoadNull.operand_bytes(), 0);
        assert_eq!(Opcode::LoadParam0.operand_bytes(), 0);
        assert_eq!(Opcode::LoadLocal0.operand_bytes(), 0);
        assert_eq!(Opcode::StoreLocal0.operand_bytes(), 0);
        assert_eq!(Opcode::IntAdd.operand_bytes(), 0);
        assert_eq!(Opcode::FloatMul.operand_bytes(), 0);
        assert_eq!(Opcode::Pop.operand_bytes(), 0);
        assert_eq!(Opcode::Dup.operand_bytes(), 0);
        assert_eq!(Opcode::ReturnVoid.operand_bytes(), 0);
        assert_eq!(Opcode::ReturnPop.operand_bytes(), 0);
        assert_eq!(Opcode::LoadDataAddr.operand_bytes(), 0);
        assert_eq!(Opcode::Load8BitArray.operand_bytes(), 0);
        assert_eq!(Opcode::InitArray.operand_bytes(), 0);
    }

    #[test]
    fn test_operand_bytes_one() {
        assert_eq!(Opcode::LoadIntU1.operand_bytes(), 1);
        assert_eq!(Opcode::LoadParam.operand_bytes(), 1);
        assert_eq!(Opcode::LoadParamWide.operand_bytes(), 1);
        assert_eq!(Opcode::StoreParam.operand_bytes(), 1);
        assert_eq!(Opcode::LoadLocal.operand_bytes(), 1);
        assert_eq!(Opcode::StoreLocal.operand_bytes(), 1);
        assert_eq!(Opcode::Jump.operand_bytes(), 1);
        assert_eq!(Opcode::JumpZero.operand_bytes(), 1);
        assert_eq!(Opcode::JumpIntEq.operand_bytes(), 1);
        assert_eq!(Opcode::Load8BitFieldU1.operand_bytes(), 1);
        assert_eq!(Opcode::Store32BitFieldU1.operand_bytes(), 1);
    }

    #[test]
    fn test_operand_bytes_two() {
        assert_eq!(Opcode::LoadIntU2.operand_bytes(), 2);
        assert_eq!(Opcode::LoadInt.operand_bytes(), 2);
        assert_eq!(Opcode::LoadFloat.operand_bytes(), 2);
        assert_eq!(Opcode::LoadStr.operand_bytes(), 2);
        assert_eq!(Opcode::Call.operand_bytes(), 2);
        assert_eq!(Opcode::LoadParam0Call.operand_bytes(), 2);
        assert_eq!(Opcode::JumpFar.operand_bytes(), 2);
        assert_eq!(Opcode::JumpFarIntEq.operand_bytes(), 2);
        assert_eq!(Opcode::InitVirt.operand_bytes(), 2);
        assert_eq!(Opcode::Assert.operand_bytes(), 2);
        assert_eq!(Opcode::Switch.operand_bytes(), 2);
        assert_eq!(Opcode::MetaSlot.operand_bytes(), 2);
    }

    #[test]
    fn test_operand_bytes_three() {
        assert_eq!(Opcode::CallVirtual.operand_bytes(), 3);
        assert_eq!(Opcode::CallNative.operand_bytes(), 3);
        assert_eq!(Opcode::CallNativeWide.operand_bytes(), 3);
        assert_eq!(Opcode::CallNativeVoid.operand_bytes(), 3);
    }

    #[test]
    fn test_operand_bytes_four() {
        assert_eq!(Opcode::Load8BitFieldU4.operand_bytes(), 4);
        assert_eq!(Opcode::Store8BitFieldU4.operand_bytes(), 4);
        assert_eq!(Opcode::Load16BitFieldU4.operand_bytes(), 4);
        assert_eq!(Opcode::Load32BitFieldU4.operand_bytes(), 4);
        assert_eq!(Opcode::Load64BitFieldU4.operand_bytes(), 4);
        assert_eq!(Opcode::LoadRefFieldU4.operand_bytes(), 4);
        assert_eq!(Opcode::StoreRefFieldU4.operand_bytes(), 4);
        assert_eq!(Opcode::LoadInlineFieldU4.operand_bytes(), 4);
        assert_eq!(Opcode::LoadParam0InlineFieldU4.operand_bytes(), 4);
        assert_eq!(Opcode::LoadDataInlineFieldU4.operand_bytes(), 4);
    }

    #[test]
    fn test_copy_clone() {
        let op = Opcode::IntAdd;
        let op2 = op; // Copy
        let op3 = op.clone(); // Clone
        assert_eq!(op, op2);
        assert_eq!(op, op3);
    }

    #[test]
    fn test_enum_size() {
        assert_eq!(std::mem::size_of::<Opcode>(), 1);
    }

    #[test]
    fn test_opcode_names_table_length() {
        assert_eq!(OPCODE_NAMES.len(), NUM_OPCODES);
    }

    #[test]
    fn test_every_branch_has_nonzero_operand() {
        for i in 0u8..240 {
            let op = Opcode::try_from(i).unwrap();
            if op.is_branch() {
                assert!(
                    op.operand_bytes() >= 1,
                    "{:?} is a branch but has 0 operand bytes",
                    op
                );
            }
        }
    }
}
