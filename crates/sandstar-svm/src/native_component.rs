//! Kit 0 Component reflection native methods — pure Rust implementations.
//!
//! Replaces the C implementations in `sys_Component.c`.  These methods provide
//! runtime reflection on Sedona component slots: reading/writing property values
//! by slot index, and invoking action methods.
//!
//! # Slot resolution
//!
//! Each component instance lives in the **data** segment.  Its slot metadata
//! lives in the **code** segment.  The resolution chain is:
//!
//! 1. `self` (params[0]) = data segment offset of the component instance
//! 2. `slot` (params[1]) = code segment offset of the Slot descriptor
//! 3. From the Slot descriptor (code segment):
//!    - `code[slot + 4..slot + 6]` = u16 block index of the slot's Type
//!    - `code[slot + 6..slot + 8]` = u16 handle (field offset in data segment)
//! 4. Type descriptor at `type_bix * block_size` (code segment):
//!    - `code[type_offset + 0]` = u8 type ID (0=void, 1=bool, ..., 8=buf)
//! 5. The actual value is at `data[self + handle]`
//!
//! # Method ID mapping (from nativetable.c)
//!
//! | ID | Method                         | Width  |
//! |----|-------------------------------|--------|
//! | 22 | Component.invokeVoid(Slot)    | normal |
//! | 23 | Component.invokeBool(Slot,..) | normal |
//! | 24 | Component.invokeInt(Slot,..)  | normal |
//! | 25 | Component.invokeLong(Slot,..) | normal |
//! | 26 | Component.invokeFloat(Slot,..)| normal |
//! | 27 | Component.invokeDouble(Slot,.)| normal |
//! | 28 | Component.invokeBuf(Slot,..)  | normal |
//! | 29 | Component.getBool(Slot)       | normal |
//! | 30 | Component.getInt(Slot)        | normal |
//! | 31 | Component.getLong(Slot)       | wide   |
//! | 32 | Component.getFloat(Slot)      | normal |
//! | 33 | Component.getDouble(Slot)     | wide   |
//! | 34 | Component.getBuf(Slot)        | normal |
//! | 35 | Component.doSetBool(Slot,..)  | normal |
//! | 36 | Component.doSetInt(Slot,..)   | normal |
//! | 37 | Component.doSetLong(Slot,..)  | normal |
//! | 38 | Component.doSetFloat(Slot,..) | normal |
//! | 39 | Component.doSetDouble(Slot,..)| normal |
//! | 40 | Type.malloc(type)             | normal |
//! | 55 | Test.doMain()                 | normal |

use crate::native_table::{NativeContext, NativeTable};
use crate::vm_error::{VmError, VmResult};

// ────────────────────────────────────────────────────────────────
// Type ID constants (from sedona.h / scode.h)
// ────────────────────────────────────────────────────────────────

const VOID_ID: u8 = 0;
const BOOL_ID: u8 = 1;
const BYTE_ID: u8 = 2;
const SHORT_ID: u8 = 3;
const INT_ID: u8 = 4;
const LONG_ID: u8 = 5;
const FLOAT_ID: u8 = 6;
const DOUBLE_ID: u8 = 7;
const BUF_ID: u8 = 8;

// ────────────────────────────────────────────────────────────────
// Slot resolution helper
// ────────────────────────────────────────────────────────────────

/// Resolve a component slot: returns `(type_id, handle)`.
///
/// - `slot_code_offset` — code segment byte offset of the Slot descriptor
///   (passed as params[1], originally a pointer in C)
/// - Returns the slot's type ID and the field handle (byte offset within the
///   component's data region).
///
/// The Slot descriptor layout in the code segment is:
///   offset 0: (unused here)
///   offset 2: name block index (u16)
///   offset 4: type block index (u16) — points to the Type descriptor
///   offset 6: handle (u16) — field byte offset in data segment
///
/// The Type descriptor layout:
///   offset 0: type_id (u8)
fn resolve_slot(ctx: &NativeContext<'_>, slot_code_offset: usize) -> VmResult<(u8, u16)> {
    let code = ctx.code.ok_or_else(|| VmError::NativeError {
        kit: 0,
        method: 0,
        message: "Component reflection requires code segment access".into(),
    })?;
    let block_size = ctx.block_size as usize;

    // Read type block index from Slot descriptor at code[slot + 4..slot + 6]
    let type_bix_offset = slot_code_offset + 4;
    if type_bix_offset + 2 > code.len() {
        return Err(VmError::PcOutOfBounds {
            pc: type_bix_offset,
            code_len: code.len(),
        });
    }
    let type_bix = u16::from_le_bytes([code[type_bix_offset], code[type_bix_offset + 1]]);

    // Read handle from Slot descriptor at code[slot + 6..slot + 8]
    let handle_offset = slot_code_offset + 6;
    if handle_offset + 2 > code.len() {
        return Err(VmError::PcOutOfBounds {
            pc: handle_offset,
            code_len: code.len(),
        });
    }
    let handle = u16::from_le_bytes([code[handle_offset], code[handle_offset + 1]]);

    // Resolve Type descriptor: type_code_offset = type_bix * block_size
    let type_code_offset = (type_bix as usize) * block_size;
    if type_code_offset >= code.len() {
        return Err(VmError::PcOutOfBounds {
            pc: type_code_offset,
            code_len: code.len(),
        });
    }
    let type_id = code[type_code_offset];

    Ok((type_id, handle))
}

// ────────────────────────────────────────────────────────────────
// Data segment helpers (read from NativeContext.memory)
// ────────────────────────────────────────────────────────────────

#[inline]
fn data_u8(memory: &[u8], addr: usize) -> VmResult<u8> {
    memory.get(addr).copied().ok_or(VmError::NullPointer)
}

#[inline]
fn data_u16(memory: &[u8], addr: usize) -> VmResult<u16> {
    let end = addr.checked_add(2).ok_or(VmError::NullPointer)?;
    let s = memory.get(addr..end).ok_or(VmError::NullPointer)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

#[inline]
fn data_i16(memory: &[u8], addr: usize) -> VmResult<i16> {
    data_u16(memory, addr).map(|v| v as i16)
}

#[inline]
fn data_i32(memory: &[u8], addr: usize) -> VmResult<i32> {
    let end = addr.checked_add(4).ok_or(VmError::NullPointer)?;
    let s = memory.get(addr..end).ok_or(VmError::NullPointer)?;
    Ok(i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

#[inline]
fn data_i64(memory: &[u8], addr: usize) -> VmResult<i64> {
    let end = addr.checked_add(8).ok_or(VmError::NullPointer)?;
    let s = memory.get(addr..end).ok_or(VmError::NullPointer)?;
    Ok(i64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

#[inline]
fn set_data_u8(memory: &mut [u8], addr: usize, val: u8) -> VmResult<()> {
    let b = memory.get_mut(addr).ok_or(VmError::NullPointer)?;
    *b = val;
    Ok(())
}

#[inline]
fn set_data_u16(memory: &mut [u8], addr: usize, val: u16) -> VmResult<()> {
    let end = addr.checked_add(2).ok_or(VmError::NullPointer)?;
    let s = memory.get_mut(addr..end).ok_or(VmError::NullPointer)?;
    s.copy_from_slice(&val.to_le_bytes());
    Ok(())
}

#[inline]
fn set_data_i32(memory: &mut [u8], addr: usize, val: i32) -> VmResult<()> {
    let end = addr.checked_add(4).ok_or(VmError::NullPointer)?;
    let s = memory.get_mut(addr..end).ok_or(VmError::NullPointer)?;
    s.copy_from_slice(&val.to_le_bytes());
    Ok(())
}

#[inline]
fn set_data_i64(memory: &mut [u8], addr: usize, val: i64) -> VmResult<()> {
    let end = addr.checked_add(8).ok_or(VmError::NullPointer)?;
    let s = memory.get_mut(addr..end).ok_or(VmError::NullPointer)?;
    s.copy_from_slice(&val.to_le_bytes());
    Ok(())
}

// ────────────────────────────────────────────────────────────────
// Getters
// ────────────────────────────────────────────────────────────────

/// `bool Component.getBool(Slot)` — read a bool property from a component slot.
///
/// params[0] = self (data segment offset of component)
/// params[1] = slot (code segment offset of Slot descriptor)
pub fn component_get_bool(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let (type_id, handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != BOOL_ID {
        return Err(VmError::TypeMismatch {
            expected: BOOL_ID,
            got: type_id,
        });
    }

    let val = data_u8(ctx.memory, self_addr + handle as usize)?;
    Ok(val as i32)
}

/// `int Component.getInt(Slot)` — read a byte/short/int property.
///
/// Dispatches based on the slot's type ID: ByteTypeId, ShortTypeId, or IntTypeId.
pub fn component_get_int(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let (type_id, handle) = resolve_slot(ctx, slot_offset)?;
    let addr = self_addr + handle as usize;

    match type_id {
        BYTE_ID => Ok(data_u8(ctx.memory, addr)? as i32),
        SHORT_ID => Ok(data_i16(ctx.memory, addr)? as i32),
        INT_ID => data_i32(ctx.memory, addr),
        _ => Err(VmError::TypeMismatch {
            expected: INT_ID,
            got: type_id,
        }),
    }
}

/// `long Component.getLong(Slot)` — read a long (i64) property.
pub fn component_get_long(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i64> {
    let self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let (type_id, handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != LONG_ID {
        return Err(VmError::TypeMismatch {
            expected: LONG_ID,
            got: type_id,
        });
    }

    data_i64(ctx.memory, self_addr + handle as usize)
}

/// `int Component.getFloat(Slot)` — read a float property as raw i32 bits.
///
/// Returns the float as integer bits to avoid NaN weirdnesses (matching C behavior).
pub fn component_get_float(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let (type_id, handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != FLOAT_ID {
        return Err(VmError::TypeMismatch {
            expected: FLOAT_ID,
            got: type_id,
        });
    }

    // Read as int bits (avoids NaN canonicalization)
    data_i32(ctx.memory, self_addr + handle as usize)
}

/// `long Component.getDouble(Slot)` — read a double property as raw i64 bits.
pub fn component_get_double(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i64> {
    let self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let (type_id, handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != DOUBLE_ID {
        return Err(VmError::TypeMismatch {
            expected: DOUBLE_ID,
            got: type_id,
        });
    }

    data_i64(ctx.memory, self_addr + handle as usize)
}

/// `Buf Component.getBuf(Slot)` — get pointer to inline buffer.
///
/// Returns data segment offset of the inline buffer (self + handle).
pub fn component_get_buf(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let (type_id, handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != BUF_ID {
        return Err(VmError::TypeMismatch {
            expected: BUF_ID,
            got: type_id,
        });
    }

    // getInline: return base + offset (pointer to inline data)
    Ok((self_addr + handle as usize) as i32)
}

// ────────────────────────────────────────────────────────────────
// Setters — return 1 (true) if value changed, 0 (false) if unchanged
// ────────────────────────────────────────────────────────────────

/// `bool Component.doSetBool(Slot, bool)` — write a bool, return true if changed.
pub fn component_set_bool(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let val = params[2] as u8;
    let (type_id, handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != BOOL_ID {
        return Err(VmError::TypeMismatch {
            expected: BOOL_ID,
            got: type_id,
        });
    }

    let addr = self_addr + handle as usize;
    let old = data_u8(ctx.memory, addr)?;
    if old == val {
        return Ok(0); // no change
    }
    set_data_u8(ctx.memory, addr, val)?;
    Ok(1) // changed
}

/// `bool Component.doSetInt(Slot, int)` — write byte/short/int, return true if changed.
pub fn component_set_int(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let val = params[2];
    let (type_id, handle) = resolve_slot(ctx, slot_offset)?;
    let addr = self_addr + handle as usize;

    match type_id {
        BYTE_ID => {
            let old = data_u8(ctx.memory, addr)? as i32;
            if old == val {
                return Ok(0);
            }
            set_data_u8(ctx.memory, addr, val as u8)?;
        }
        SHORT_ID => {
            let old = data_i16(ctx.memory, addr)? as i32;
            if old == val {
                return Ok(0);
            }
            set_data_u16(ctx.memory, addr, val as u16)?;
        }
        INT_ID => {
            let old = data_i32(ctx.memory, addr)?;
            if old == val {
                return Ok(0);
            }
            set_data_i32(ctx.memory, addr, val)?;
        }
        _ => {
            return Err(VmError::TypeMismatch {
                expected: INT_ID,
                got: type_id,
            })
        }
    }

    Ok(1) // changed
}

/// `bool Component.doSetLong(Slot, long)` — write i64, return true if changed.
///
/// params: [self, slot, val_lo, val_hi] where val = val_lo | (val_hi << 32)
pub fn component_set_long(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let val = ((params[3] as i64) << 32) | (params[2] as u32 as i64);
    let (type_id, handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != LONG_ID {
        return Err(VmError::TypeMismatch {
            expected: LONG_ID,
            got: type_id,
        });
    }

    let addr = self_addr + handle as usize;
    let old = data_i64(ctx.memory, addr)?;
    if old == val {
        return Ok(0);
    }
    set_data_i64(ctx.memory, addr, val)?;
    Ok(1)
}

/// `bool Component.doSetFloat(Slot, float)` — write float, compare bits for changed.
///
/// Bit comparison ensures NaN == NaN (Sedona spec), matching the C implementation.
pub fn component_set_float(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let new_bits = params[2]; // float value as raw i32 bits
    let (type_id, handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != FLOAT_ID {
        return Err(VmError::TypeMismatch {
            expected: FLOAT_ID,
            got: type_id,
        });
    }

    let addr = self_addr + handle as usize;
    let old_bits = data_i32(ctx.memory, addr)?;

    // Compare bits: NaN == NaN in Sedona
    if old_bits == new_bits {
        return Ok(0);
    }

    // Write as float (matching C's setFloat)
    set_data_i32(ctx.memory, addr, new_bits)?;
    Ok(1)
}

/// `bool Component.doSetDouble(Slot, double)` — write f64, return true if changed.
///
/// params: [self, slot, val_lo, val_hi] where val = val_lo | (val_hi << 32)
pub fn component_set_double(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let val = ((params[3] as i64) << 32) | (params[2] as u32 as i64);
    let (type_id, handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != DOUBLE_ID {
        return Err(VmError::TypeMismatch {
            expected: DOUBLE_ID,
            got: type_id,
        });
    }

    let addr = self_addr + handle as usize;
    let old = data_i64(ctx.memory, addr)?;
    if old == val {
        return Ok(0);
    }
    set_data_i64(ctx.memory, addr, val)?;
    Ok(1)
}

// ────────────────────────────────────────────────────────────────
// Invoke methods — stubs until VM call-back is wired up
// ────────────────────────────────────────────────────────────────
//
// These methods require calling back into the VM interpreter (vm->call in C),
// which needs the interpreter to be available through NativeContext.
// For now they validate the type ID and return 0.
// TODO: Wire up when the full VM interpreter integration is complete.

/// `void Component.invokeVoid(Slot)` — look up vtable, call void action.
pub fn component_invoke_void(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let (type_id, _handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != VOID_ID {
        return Err(VmError::TypeMismatch {
            expected: VOID_ID,
            got: type_id,
        });
    }

    // TODO: look up vtable method and call via vm->call
    Ok(0)
}

/// `void Component.invokeBool(Slot, bool)` — invoke action with bool arg.
pub fn component_invoke_bool(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let _val = params[2];
    let (type_id, _handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != BOOL_ID {
        return Err(VmError::TypeMismatch {
            expected: BOOL_ID,
            got: type_id,
        });
    }

    // TODO: look up vtable method and call via vm->call
    Ok(0)
}

/// `void Component.invokeInt(Slot, int)` — invoke action with int arg.
pub fn component_invoke_int(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let _val = params[2];
    let (type_id, _handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != INT_ID {
        return Err(VmError::TypeMismatch {
            expected: INT_ID,
            got: type_id,
        });
    }

    // TODO: look up vtable method and call via vm->call
    Ok(0)
}

/// `void Component.invokeLong(Slot, long)` — invoke action with long arg.
pub fn component_invoke_long(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let (type_id, _handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != LONG_ID {
        return Err(VmError::TypeMismatch {
            expected: LONG_ID,
            got: type_id,
        });
    }

    // TODO: look up vtable method and call via vm->call
    Ok(0)
}

/// `void Component.invokeFloat(Slot, float)` — invoke action with float arg.
pub fn component_invoke_float(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let _val = params[2];
    let (type_id, _handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != FLOAT_ID {
        return Err(VmError::TypeMismatch {
            expected: FLOAT_ID,
            got: type_id,
        });
    }

    // TODO: look up vtable method and call via vm->call
    Ok(0)
}

/// `void Component.invokeDouble(Slot, double)` — invoke action with double arg.
pub fn component_invoke_double(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let (type_id, _handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != DOUBLE_ID {
        return Err(VmError::TypeMismatch {
            expected: DOUBLE_ID,
            got: type_id,
        });
    }

    // TODO: look up vtable method and call via vm->call
    Ok(0)
}

/// `void Component.invokeBuf(Slot, Buf)` — invoke action with buffer arg.
pub fn component_invoke_buf(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _self_addr = params[0] as usize;
    let slot_offset = params[1] as usize;
    let _val = params[2];
    let (type_id, _handle) = resolve_slot(ctx, slot_offset)?;

    if type_id != BUF_ID {
        return Err(VmError::TypeMismatch {
            expected: BUF_ID,
            got: type_id,
        });
    }

    // TODO: look up vtable method and call via vm->call
    Ok(0)
}

// ────────────────────────────────────────────────────────────────
// Type.malloc and Test.doMain — stubs
// ────────────────────────────────────────────────────────────────

/// `Obj Type.malloc(Type)` — allocate instance of given type.
///
/// Stub: returns 0 (null). Full implementation requires heap management
/// that will be added when the pure-Rust VM manages its own data segment.
pub fn type_malloc(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    // TODO: implement heap allocation when data segment management is complete
    Ok(0)
}

/// `void Test.doMain()` — run the scode test suite.
///
/// Stub: returns 0. The full implementation needs to invoke the test entry
/// point in the scode image, which requires the VM interpreter to be wired up.
pub fn test_do_main(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    // TODO: invoke scode test entry point when VM interpreter is integrated
    Ok(0)
}

// ────────────────────────────────────────────────────────────────
// Registration
// ────────────────────────────────────────────────────────────────

/// Register all Kit 0 Component/Type/Test native methods into the table.
///
/// Slots 22-39: Component.invoke* and Component.get*/doSet*
/// Slot 40: Type.malloc
/// Slot 55: Test.doMain
pub fn register_kit0_component(table: &mut NativeTable) {
    // ── Invoke methods (slots 22-28) ──
    table.register(0, 22, component_invoke_void);
    table.register(0, 23, component_invoke_bool);
    table.register(0, 24, component_invoke_int);
    table.register(0, 25, component_invoke_long);
    table.register(0, 26, component_invoke_float);
    table.register(0, 27, component_invoke_double);
    table.register(0, 28, component_invoke_buf);

    // ── Getters (slots 29-34) ──
    table.register(0, 29, component_get_bool);
    table.register(0, 30, component_get_int);
    table.register_wide(0, 31, component_get_long);
    table.register(0, 32, component_get_float);
    table.register_wide(0, 33, component_get_double);
    table.register(0, 34, component_get_buf);

    // ── Setters (slots 35-39) ──
    table.register(0, 35, component_set_bool);
    table.register(0, 36, component_set_int);
    table.register(0, 37, component_set_long);
    table.register(0, 38, component_set_float);
    table.register(0, 39, component_set_double);

    // ── Type.malloc (slot 40) ──
    table.register(0, 40, type_malloc);

    // ── Test.doMain (slot 55) ──
    table.register(0, 55, test_do_main);
}

// ════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_table::{NativeContext, NativeEntry, NativeTable};

    /// Block size used in tests (matches SCODE_BLOCK_SIZE = 4).
    const TEST_BLOCK_SIZE: u8 = 4;

    /// Build a mock code segment with a Slot descriptor and Type descriptor.
    ///
    /// Layout:
    ///   block 0 (offset 0..3): unused padding
    ///   block 1 (offset 4..7): Type descriptor — type_id at byte 4
    ///   block 2 (offset 8..15): Slot descriptor
    ///     - offset 8..9: unused (id)
    ///     - offset 10..11: unused (name bix)
    ///     - offset 12..13: type block index = 1 (points to block 1)
    ///     - offset 14..15: handle (field byte offset)
    ///
    /// Returns (code, slot_code_offset).
    fn make_slot_code(type_id: u8, handle: u16) -> (Vec<u8>, usize) {
        let mut code = vec![0u8; 32];

        // Type descriptor at block 1 (offset 4)
        code[4] = type_id;

        // Slot descriptor at block 2 (offset 8)
        let slot_offset: usize = 8;
        // type block index at slot + 4 = offset 12
        code[12..14].copy_from_slice(&1u16.to_le_bytes()); // block index 1
                                                           // handle at slot + 6 = offset 14
        code[14..16].copy_from_slice(&handle.to_le_bytes());

        (code, slot_offset)
    }

    /// Create a NativeContext with both data and code segments.
    fn make_ctx<'a>(data: &'a mut Vec<u8>, code: &'a [u8]) -> NativeContext<'a> {
        NativeContext::with_code(data, code, TEST_BLOCK_SIZE)
    }

    // ── resolve_slot tests ──────────────────────────────────────

    #[test]
    fn resolve_slot_basic() {
        let (code, slot_offset) = make_slot_code(INT_ID, 20);
        let mut data = vec![0u8; 64];
        let ctx = make_ctx(&mut data, &code);
        let (tid, handle) = resolve_slot(&ctx, slot_offset).unwrap();
        assert_eq!(tid, INT_ID);
        assert_eq!(handle, 20);
    }

    #[test]
    fn resolve_slot_bool_type() {
        let (code, slot_offset) = make_slot_code(BOOL_ID, 10);
        let mut data = vec![0u8; 64];
        let ctx = make_ctx(&mut data, &code);
        let (tid, handle) = resolve_slot(&ctx, slot_offset).unwrap();
        assert_eq!(tid, BOOL_ID);
        assert_eq!(handle, 10);
    }

    #[test]
    fn resolve_slot_no_code_returns_error() {
        let mut data = vec![0u8; 64];
        let ctx = NativeContext::new(&mut data);
        let err = resolve_slot(&ctx, 0).unwrap_err();
        assert!(matches!(err, VmError::NativeError { .. }));
    }

    #[test]
    fn resolve_slot_out_of_bounds_type_bix() {
        // Slot with type_bix pointing beyond code segment
        let mut code = vec![0u8; 16];
        let slot_offset: usize = 8;
        // type block index = 0xFF (offset 0xFF * 4 = 1020, beyond code len 16)
        code[12..14].copy_from_slice(&0xFFu16.to_le_bytes());
        code[14..16].copy_from_slice(&0u16.to_le_bytes());

        let mut data = vec![0u8; 64];
        let ctx = make_ctx(&mut data, &code);
        let err = resolve_slot(&ctx, slot_offset).unwrap_err();
        assert!(matches!(err, VmError::PcOutOfBounds { .. }));
    }

    // ── getBool tests ───────────────────────────────────────────

    #[test]
    fn get_bool_reads_true() {
        let (code, slot_offset) = make_slot_code(BOOL_ID, 10);
        let mut data = vec![0u8; 64];
        data[10] = 1; // component at addr 0, field at offset 10
        let mut ctx = make_ctx(&mut data, &code);

        let result = component_get_bool(&mut ctx, &[0, slot_offset as i32]).unwrap();
        assert_eq!(result, 1);
    }

    #[test]
    fn get_bool_reads_false() {
        let (code, slot_offset) = make_slot_code(BOOL_ID, 10);
        let mut data = vec![0u8; 64];
        data[10] = 0;
        let mut ctx = make_ctx(&mut data, &code);

        let result = component_get_bool(&mut ctx, &[0, slot_offset as i32]).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn get_bool_wrong_type_returns_error() {
        let (code, slot_offset) = make_slot_code(INT_ID, 10);
        let mut data = vec![0u8; 64];
        let mut ctx = make_ctx(&mut data, &code);

        let err = component_get_bool(&mut ctx, &[0, slot_offset as i32]).unwrap_err();
        assert!(matches!(
            err,
            VmError::TypeMismatch {
                expected: BOOL_ID,
                ..
            }
        ));
    }

    // ── getInt tests ────────────────────────────────────────────

    #[test]
    fn get_int_reads_byte() {
        let (code, slot_offset) = make_slot_code(BYTE_ID, 10);
        let mut data = vec![0u8; 64];
        data[10] = 42;
        let mut ctx = make_ctx(&mut data, &code);

        let result = component_get_int(&mut ctx, &[0, slot_offset as i32]).unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn get_int_reads_short() {
        let (code, slot_offset) = make_slot_code(SHORT_ID, 10);
        let mut data = vec![0u8; 64];
        data[10..12].copy_from_slice(&1234i16.to_le_bytes());
        let mut ctx = make_ctx(&mut data, &code);

        let result = component_get_int(&mut ctx, &[0, slot_offset as i32]).unwrap();
        assert_eq!(result, 1234);
    }

    #[test]
    fn get_int_reads_negative_short() {
        let (code, slot_offset) = make_slot_code(SHORT_ID, 10);
        let mut data = vec![0u8; 64];
        data[10..12].copy_from_slice(&(-500i16).to_le_bytes());
        let mut ctx = make_ctx(&mut data, &code);

        let result = component_get_int(&mut ctx, &[0, slot_offset as i32]).unwrap();
        assert_eq!(result, -500);
    }

    #[test]
    fn get_int_reads_int() {
        let (code, slot_offset) = make_slot_code(INT_ID, 10);
        let mut data = vec![0u8; 64];
        data[10..14].copy_from_slice(&(-42i32).to_le_bytes());
        let mut ctx = make_ctx(&mut data, &code);

        let result = component_get_int(&mut ctx, &[0, slot_offset as i32]).unwrap();
        assert_eq!(result, -42);
    }

    #[test]
    fn get_int_wrong_type_returns_error() {
        let (code, slot_offset) = make_slot_code(FLOAT_ID, 10);
        let mut data = vec![0u8; 64];
        let mut ctx = make_ctx(&mut data, &code);

        let err = component_get_int(&mut ctx, &[0, slot_offset as i32]).unwrap_err();
        assert!(matches!(err, VmError::TypeMismatch { .. }));
    }

    // ── getFloat tests ──────────────────────────────────────────

    #[test]
    fn get_float_reads_value() {
        let (code, slot_offset) = make_slot_code(FLOAT_ID, 10);
        let mut data = vec![0u8; 64];
        let val: f32 = 72.5;
        data[10..14].copy_from_slice(&val.to_le_bytes());
        let mut ctx = make_ctx(&mut data, &code);

        let bits = component_get_float(&mut ctx, &[0, slot_offset as i32]).unwrap();
        assert_eq!(f32::from_bits(bits as u32), 72.5);
    }

    #[test]
    fn get_float_preserves_nan_bits() {
        let (code, slot_offset) = make_slot_code(FLOAT_ID, 10);
        let mut data = vec![0u8; 64];
        let null_float_bits: u32 = 0x7fc00000; // Sedona NULLFLOAT
        data[10..14].copy_from_slice(&null_float_bits.to_le_bytes());
        let mut ctx = make_ctx(&mut data, &code);

        let bits = component_get_float(&mut ctx, &[0, slot_offset as i32]).unwrap();
        assert_eq!(bits as u32, 0x7fc00000);
    }

    // ── getLong tests ───────────────────────────────────────────

    #[test]
    fn get_long_reads_value() {
        let (code, slot_offset) = make_slot_code(LONG_ID, 10);
        let mut data = vec![0u8; 64];
        let val: i64 = 123_456_789_012_345;
        data[10..18].copy_from_slice(&val.to_le_bytes());
        let mut ctx = make_ctx(&mut data, &code);

        let result = component_get_long(&mut ctx, &[0, slot_offset as i32]).unwrap();
        assert_eq!(result, val);
    }

    #[test]
    fn get_long_wrong_type_returns_error() {
        let (code, slot_offset) = make_slot_code(INT_ID, 10);
        let mut data = vec![0u8; 64];
        let mut ctx = make_ctx(&mut data, &code);

        let err = component_get_long(&mut ctx, &[0, slot_offset as i32]).unwrap_err();
        assert!(matches!(
            err,
            VmError::TypeMismatch {
                expected: LONG_ID,
                ..
            }
        ));
    }

    // ── getDouble tests ─────────────────────────────────────────

    #[test]
    fn get_double_reads_value() {
        let (code, slot_offset) = make_slot_code(DOUBLE_ID, 10);
        let mut data = vec![0u8; 64];
        let val: f64 = 3.14159265358979;
        data[10..18].copy_from_slice(&val.to_le_bytes());
        let mut ctx = make_ctx(&mut data, &code);

        let bits = component_get_double(&mut ctx, &[0, slot_offset as i32]).unwrap();
        assert_eq!(f64::from_bits(bits as u64), val);
    }

    // ── getBuf tests ────────────────────────────────────────────

    #[test]
    fn get_buf_returns_inline_address() {
        let (code, slot_offset) = make_slot_code(BUF_ID, 20);
        let mut data = vec![0u8; 64];
        let mut ctx = make_ctx(&mut data, &code);

        // Component at addr 0, handle=20 → inline buffer at 0+20=20
        let result = component_get_buf(&mut ctx, &[0, slot_offset as i32]).unwrap();
        assert_eq!(result, 20);
    }

    #[test]
    fn get_buf_with_nonzero_self() {
        let (code, slot_offset) = make_slot_code(BUF_ID, 8);
        let mut data = vec![0u8; 64];
        let mut ctx = make_ctx(&mut data, &code);

        // Component at addr 16, handle=8 → inline buffer at 16+8=24
        let result = component_get_buf(&mut ctx, &[16, slot_offset as i32]).unwrap();
        assert_eq!(result, 24);
    }

    // ── setBool tests ───────────────────────────────────────────

    #[test]
    fn set_bool_detects_change() {
        let (code, slot_offset) = make_slot_code(BOOL_ID, 10);
        let mut data = vec![0u8; 64];
        data[10] = 0;
        let mut ctx = make_ctx(&mut data, &code);

        let changed = component_set_bool(&mut ctx, &[0, slot_offset as i32, 1]).unwrap();
        assert_eq!(changed, 1);
        assert_eq!(data[10], 1);
    }

    #[test]
    fn set_bool_no_change() {
        let (code, slot_offset) = make_slot_code(BOOL_ID, 10);
        let mut data = vec![0u8; 64];
        data[10] = 1;
        let mut ctx = make_ctx(&mut data, &code);

        let changed = component_set_bool(&mut ctx, &[0, slot_offset as i32, 1]).unwrap();
        assert_eq!(changed, 0);
    }

    // ── setInt tests ────────────────────────────────────────────

    #[test]
    fn set_int_byte_change() {
        let (code, slot_offset) = make_slot_code(BYTE_ID, 10);
        let mut data = vec![0u8; 64];
        let mut ctx = make_ctx(&mut data, &code);

        let changed = component_set_int(&mut ctx, &[0, slot_offset as i32, 42]).unwrap();
        assert_eq!(changed, 1);
        assert_eq!(data[10], 42);
    }

    #[test]
    fn set_int_byte_no_change() {
        let (code, slot_offset) = make_slot_code(BYTE_ID, 10);
        let mut data = vec![0u8; 64];
        data[10] = 42;
        let mut ctx = make_ctx(&mut data, &code);

        let changed = component_set_int(&mut ctx, &[0, slot_offset as i32, 42]).unwrap();
        assert_eq!(changed, 0);
    }

    #[test]
    fn set_int_short_change() {
        let (code, slot_offset) = make_slot_code(SHORT_ID, 10);
        let mut data = vec![0u8; 64];
        let mut ctx = make_ctx(&mut data, &code);

        let changed = component_set_int(&mut ctx, &[0, slot_offset as i32, 1234]).unwrap();
        assert_eq!(changed, 1);
        let stored = i16::from_le_bytes([data[10], data[11]]);
        assert_eq!(stored, 1234);
    }

    #[test]
    fn set_int_int_change() {
        let (code, slot_offset) = make_slot_code(INT_ID, 10);
        let mut data = vec![0u8; 64];
        let mut ctx = make_ctx(&mut data, &code);

        let changed = component_set_int(&mut ctx, &[0, slot_offset as i32, -42]).unwrap();
        assert_eq!(changed, 1);
        let stored = i32::from_le_bytes([data[10], data[11], data[12], data[13]]);
        assert_eq!(stored, -42);
    }

    // ── setFloat tests ──────────────────────────────────────────

    #[test]
    fn set_float_change() {
        let (code, slot_offset) = make_slot_code(FLOAT_ID, 10);
        let mut data = vec![0u8; 64];
        let mut ctx = make_ctx(&mut data, &code);

        let new_bits = 72.5_f32.to_bits() as i32;
        let changed = component_set_float(&mut ctx, &[0, slot_offset as i32, new_bits]).unwrap();
        assert_eq!(changed, 1);
    }

    #[test]
    fn set_float_nan_equals_nan() {
        // Sedona spec: NaN == NaN via bit comparison
        let (code, slot_offset) = make_slot_code(FLOAT_ID, 10);
        let mut data = vec![0u8; 64];
        let nan_bits: i32 = 0x7fc00000_u32 as i32;
        data[10..14].copy_from_slice(&nan_bits.to_le_bytes());
        let mut ctx = make_ctx(&mut data, &code);

        // Setting same NaN bits should detect no change
        let changed = component_set_float(&mut ctx, &[0, slot_offset as i32, nan_bits]).unwrap();
        assert_eq!(changed, 0);
    }

    #[test]
    fn set_float_different_nan_bits_is_change() {
        // Different NaN bit patterns should count as a change
        let (code, slot_offset) = make_slot_code(FLOAT_ID, 10);
        let mut data = vec![0u8; 64];
        let nan1_bits: i32 = 0x7fc00000_u32 as i32;
        let nan2_bits: i32 = 0x7fc00001_u32 as i32;
        data[10..14].copy_from_slice(&nan1_bits.to_le_bytes());
        let mut ctx = make_ctx(&mut data, &code);

        let changed = component_set_float(&mut ctx, &[0, slot_offset as i32, nan2_bits]).unwrap();
        assert_eq!(changed, 1);
    }

    // ── setLong tests ───────────────────────────────────────────

    #[test]
    fn set_long_change() {
        let (code, slot_offset) = make_slot_code(LONG_ID, 10);
        let mut data = vec![0u8; 64];
        let mut ctx = make_ctx(&mut data, &code);

        let val: i64 = 0x0000_0002_0000_0001;
        let lo = val as i32;
        let hi = (val >> 32) as i32;
        let changed = component_set_long(&mut ctx, &[0, slot_offset as i32, lo, hi]).unwrap();
        assert_eq!(changed, 1);

        let stored = i64::from_le_bytes([
            data[10], data[11], data[12], data[13], data[14], data[15], data[16], data[17],
        ]);
        assert_eq!(stored, val);
    }

    #[test]
    fn set_long_no_change() {
        let (code, slot_offset) = make_slot_code(LONG_ID, 10);
        let mut data = vec![0u8; 64];
        let val: i64 = 999;
        data[10..18].copy_from_slice(&val.to_le_bytes());
        let mut ctx = make_ctx(&mut data, &code);

        let lo = val as i32;
        let hi = (val >> 32) as i32;
        let changed = component_set_long(&mut ctx, &[0, slot_offset as i32, lo, hi]).unwrap();
        assert_eq!(changed, 0);
    }

    // ── setDouble tests ─────────────────────────────────────────

    #[test]
    fn set_double_change() {
        let (code, slot_offset) = make_slot_code(DOUBLE_ID, 10);
        let mut data = vec![0u8; 64];
        let mut ctx = make_ctx(&mut data, &code);

        let val_bits = 3.14_f64.to_bits() as i64;
        let lo = val_bits as i32;
        let hi = (val_bits >> 32) as i32;
        let changed = component_set_double(&mut ctx, &[0, slot_offset as i32, lo, hi]).unwrap();
        assert_eq!(changed, 1);
    }

    // ── invoke stub tests ───────────────────────────────────────

    #[test]
    fn invoke_void_returns_zero() {
        let (code, slot_offset) = make_slot_code(VOID_ID, 10);
        let mut data = vec![0u8; 64];
        let mut ctx = make_ctx(&mut data, &code);

        let result = component_invoke_void(&mut ctx, &[0, slot_offset as i32]).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn invoke_void_wrong_type_returns_error() {
        let (code, slot_offset) = make_slot_code(INT_ID, 10);
        let mut data = vec![0u8; 64];
        let mut ctx = make_ctx(&mut data, &code);

        let err = component_invoke_void(&mut ctx, &[0, slot_offset as i32]).unwrap_err();
        assert!(matches!(err, VmError::TypeMismatch { .. }));
    }

    #[test]
    fn invoke_bool_returns_zero() {
        let (code, slot_offset) = make_slot_code(BOOL_ID, 10);
        let mut data = vec![0u8; 64];
        let mut ctx = make_ctx(&mut data, &code);

        let result = component_invoke_bool(&mut ctx, &[0, slot_offset as i32, 1]).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn invoke_float_returns_zero() {
        let (code, slot_offset) = make_slot_code(FLOAT_ID, 10);
        let mut data = vec![0u8; 64];
        let mut ctx = make_ctx(&mut data, &code);

        let bits = 1.0_f32.to_bits() as i32;
        let result = component_invoke_float(&mut ctx, &[0, slot_offset as i32, bits]).unwrap();
        assert_eq!(result, 0);
    }

    // ── Type.malloc stub test ───────────────────────────────────

    #[test]
    fn type_malloc_returns_null() {
        let mut data = vec![0u8; 64];
        let mut ctx = NativeContext::new(&mut data);
        let result = type_malloc(&mut ctx, &[42]).unwrap();
        assert_eq!(result, 0);
    }

    // ── Test.doMain stub test ───────────────────────────────────

    #[test]
    fn test_do_main_returns_zero() {
        let mut data = vec![0u8; 64];
        let mut ctx = NativeContext::new(&mut data);
        let result = test_do_main(&mut ctx, &[]).unwrap();
        assert_eq!(result, 0);
    }

    // ── Registration tests ──────────────────────────────────────

    #[test]
    fn register_kit0_component_correct_slot_count() {
        let mut table = NativeTable::new();
        // Pre-allocate kit 0 with 60 stubs (matching with_defaults)
        for id in 0..60u16 {
            table.register_stub(0, id);
        }
        register_kit0_component(&mut table);

        // Verify specific slots are implemented (not stubs)
        assert!(
            table.is_implemented(0, 22),
            "invokeVoid should be implemented"
        );
        assert!(table.is_implemented(0, 29), "getBool should be implemented");
        assert!(table.is_implemented(0, 30), "getInt should be implemented");
        assert!(table.is_implemented(0, 31), "getLong should be implemented");
        assert!(
            table.is_implemented(0, 32),
            "getFloat should be implemented"
        );
        assert!(
            table.is_implemented(0, 33),
            "getDouble should be implemented"
        );
        assert!(table.is_implemented(0, 34), "getBuf should be implemented");
        assert!(
            table.is_implemented(0, 35),
            "doSetBool should be implemented"
        );
        assert!(
            table.is_implemented(0, 36),
            "doSetInt should be implemented"
        );
        assert!(
            table.is_implemented(0, 37),
            "doSetLong should be implemented"
        );
        assert!(
            table.is_implemented(0, 38),
            "doSetFloat should be implemented"
        );
        assert!(
            table.is_implemented(0, 39),
            "doSetDouble should be implemented"
        );
        assert!(
            table.is_implemented(0, 40),
            "Type.malloc should be implemented"
        );
        assert!(
            table.is_implemented(0, 55),
            "Test.doMain should be implemented"
        );
    }

    #[test]
    fn register_kit0_component_total_count() {
        let mut table = NativeTable::new();
        for id in 0..60u16 {
            table.register_stub(0, id);
        }
        register_kit0_component(&mut table);

        // 7 invoke + 6 getters + 5 setters + Type.malloc + Test.doMain = 20
        assert_eq!(table.implemented_count(0), 20);
    }

    #[test]
    fn register_kit0_wide_methods_are_wide() {
        let mut table = NativeTable::new();
        for id in 0..60u16 {
            table.register_stub(0, id);
        }
        register_kit0_component(&mut table);

        // getLong (31) and getDouble (33) should be Wide entries
        let entry31 = table.lookup(0, 31).unwrap();
        assert!(
            matches!(entry31, NativeEntry::Wide(_)),
            "getLong should be Wide, got {entry31:?}"
        );
        let entry33 = table.lookup(0, 33).unwrap();
        assert!(
            matches!(entry33, NativeEntry::Wide(_)),
            "getDouble should be Wide, got {entry33:?}"
        );
    }

    // ── Component at nonzero base address ───────────────────────

    #[test]
    fn get_int_at_nonzero_component_base() {
        let (code, slot_offset) = make_slot_code(INT_ID, 4);
        let mut data = vec![0u8; 128];
        // Component at base 100, field at offset 4 → addr 104
        data[104..108].copy_from_slice(&999i32.to_le_bytes());
        let mut ctx = make_ctx(&mut data, &code);

        let result = component_get_int(&mut ctx, &[100, slot_offset as i32]).unwrap();
        assert_eq!(result, 999);
    }

    #[test]
    fn set_bool_at_nonzero_component_base() {
        let (code, slot_offset) = make_slot_code(BOOL_ID, 2);
        let mut data = vec![0u8; 128];
        let mut ctx = make_ctx(&mut data, &code);

        let changed = component_set_bool(&mut ctx, &[50, slot_offset as i32, 1]).unwrap();
        assert_eq!(changed, 1);
        assert_eq!(data[52], 1); // 50 + 2 = 52
    }
}
