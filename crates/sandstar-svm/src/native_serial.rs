//! Serial kit native methods — pure Rust stub implementations.
//!
//! The Sedona `serial` kit provides `SerialPort` natives for UART access.
//! In the pure Rust VM these are stubs that log operations and return
//! reasonable defaults.  A future phase can optionally wire them to real
//! serial ports via the HAL layer.
//!
//! # Kit ID
//!
//! The serial kit is **not** present in the current scode image
//! (nativetable.c only defines kits 0, 2, 4, 9, 100).  We register
//! these under kit 3 as a placeholder — the actual kit ID will be
//! assigned by sedonac when the serial kit is compiled into an image.
//!
//! # Method ID mapping
//!
//! | ID | Method                                        | Return |
//! |----|-----------------------------------------------|--------|
//! |  0 | SerialPort.doOpen(port,baud,data,stop,par,rts)| i32    |
//! |  1 | SerialPort.doClose()                          | i32    |
//! |  2 | SerialPort.doRead(buf, off, len)              | i32    |
//! |  3 | SerialPort.doWrite(buf, off, len)             | i32    |
//! |  4 | SerialPort.doReadByte()                       | i32    |
//! |  5 | SerialPort.doWriteByte(b)                     | i32    |

use crate::native_table::{NativeContext, NativeTable};
use crate::vm_error::VmResult;

/// Default kit ID for the serial kit (placeholder — not in current scode).
pub const SERIAL_KIT_ID: u8 = 3;

/// Number of native methods in the serial kit.
pub const SERIAL_METHOD_COUNT: u16 = 6;

// ────────────────────────────────────────────────────────────────────
// Native method implementations
// ────────────────────────────────────────────────────────────────────

/// `SerialPort.doOpen(port, baud, dataBits, stopBits, parity, rtscts) -> bool`
///
/// Opens a serial port.  In stub mode, logs the request and returns
/// success (1) without actually opening anything.
///
/// Parameters (from Sedona stack):
///   - params[0]: port number (int)
///   - params[1]: baud rate (int)
///   - params[2]: data bits (int, typically 8)
///   - params[3]: stop bits (int, typically 1)
///   - params[4]: parity (int: 0=none, 1=odd, 2=even)
///   - params[5]: rtscts flow control (bool as int)
pub fn serial_do_open(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let port = params.first().copied().unwrap_or(0);
    let baud = params.get(1).copied().unwrap_or(9600);
    let _ = (port, baud); // suppress unused warnings
    Ok(1) // success
}

/// `SerialPort.doClose() -> void`
///
/// Closes a serial port.  Stub returns 0 (void).
pub fn serial_do_close(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    // stub: no-op
    Ok(0)
}

/// `SerialPort.doRead(buf, off, len) -> int`
///
/// Reads bytes from the serial port into `buf[off..off+len]`.
/// Stub returns 0 (no data available).
///
/// Parameters:
///   - params[0]: buf pointer (handle)
///   - params[1]: offset into buffer
///   - params[2]: max bytes to read
pub fn serial_do_read(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let len = params.get(2).copied().unwrap_or(0);
    let _ = len; // stub: no data
    Ok(0) // no data available
}

/// `SerialPort.doWrite(buf, off, len) -> int`
///
/// Writes `len` bytes from `buf[off..]` to the serial port.
/// Stub returns `len` (pretends all bytes were written).
///
/// Parameters:
///   - params[0]: buf pointer (handle)
///   - params[1]: offset into buffer
///   - params[2]: number of bytes to write
pub fn serial_do_write(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let len = params.get(2).copied().unwrap_or(0);
    // stub: pretend all bytes written
    Ok(len) // pretend all bytes written
}

/// `SerialPort.doReadByte() -> int`
///
/// Reads a single byte from the serial port.
/// Returns -1 if no data is available.
pub fn serial_do_read_byte(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    // stub: no data available
    Ok(-1) // no data
}

/// `SerialPort.doWriteByte(b) -> bool`
///
/// Writes a single byte to the serial port.
/// Stub returns 1 (success).
///
/// Parameters:
///   - params[0]: byte value to write (int)
pub fn serial_do_write_byte(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let _byte = params.first().copied().unwrap_or(0);
    // stub: success
    Ok(1) // success
}

// ────────────────────────────────────────────────────────────────────
// Registration
// ────────────────────────────────────────────────────────────────────

/// Register all serial kit native methods in a [`NativeTable`].
///
/// Uses [`SERIAL_KIT_ID`] (3) as the kit ID.  Call this to add serial
/// support to the dispatch table.  When a real scode image includes
/// the serial kit, the kit ID should be updated to match the image.
pub fn register_serial(table: &mut NativeTable) {
    register_serial_with_kit_id(table, SERIAL_KIT_ID);
}

/// Register serial natives under a specific kit ID.
///
/// This allows overriding the default kit ID when the scode image
/// assigns a different ID to the serial kit.
pub fn register_serial_with_kit_id(table: &mut NativeTable, kit_id: u8) {
    table.set_kit_name(kit_id, "serial");
    table.register(kit_id, 0, serial_do_open);
    table.register(kit_id, 1, serial_do_close);
    table.register(kit_id, 2, serial_do_read);
    table.register(kit_id, 3, serial_do_write);
    table.register(kit_id, 4, serial_do_read_byte);
    table.register(kit_id, 5, serial_do_write_byte);
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_table::{NativeContext, NativeEntry, NativeTable};

    fn test_ctx() -> (Vec<u8>, Vec<i32>) {
        (vec![0u8; 64], vec![])
    }

    // ── doOpen ───────────────────────────────────────────────────

    #[test]
    fn do_open_returns_success() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let params = [0, 9600, 8, 1, 0, 0]; // port, baud, dataBits, stopBits, parity, rtscts
        let result = serial_do_open(&mut ctx, &params).expect("doOpen failed");
        assert_eq!(result, 1, "doOpen should return 1 (success)");
    }

    #[test]
    fn do_open_with_no_params() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let result = serial_do_open(&mut ctx, &[]).expect("doOpen failed");
        assert_eq!(result, 1, "doOpen with no params should still succeed");
    }

    // ── doClose ──────────────────────────────────────────────────

    #[test]
    fn do_close_returns_zero() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let result = serial_do_close(&mut ctx, &[]).expect("doClose failed");
        assert_eq!(result, 0);
    }

    // ── doRead ───────────────────────────────────────────────────

    #[test]
    fn do_read_returns_zero_bytes() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let params = [0, 0, 128]; // buf, off, len
        let result = serial_do_read(&mut ctx, &params).expect("doRead failed");
        assert_eq!(result, 0, "doRead should return 0 (no data)");
    }

    // ── doWrite ──────────────────────────────────────────────────

    #[test]
    fn do_write_returns_len() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let params = [0, 0, 42]; // buf, off, len=42
        let result = serial_do_write(&mut ctx, &params).expect("doWrite failed");
        assert_eq!(result, 42, "doWrite should return len (42)");
    }

    #[test]
    fn do_write_with_no_params_returns_zero() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let result = serial_do_write(&mut ctx, &[]).expect("doWrite failed");
        assert_eq!(result, 0, "doWrite with no params returns 0");
    }

    // ── doReadByte ───────────────────────────────────────────────

    #[test]
    fn do_read_byte_returns_negative_one() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let result = serial_do_read_byte(&mut ctx, &[]).expect("doReadByte failed");
        assert_eq!(result, -1, "doReadByte should return -1 (no data)");
    }

    // ── doWriteByte ──────────────────────────────────────────────

    #[test]
    fn do_write_byte_returns_success() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let result = serial_do_write_byte(&mut ctx, &[0x42]).expect("doWriteByte failed");
        assert_eq!(result, 1, "doWriteByte should return 1 (success)");
    }

    #[test]
    fn do_write_byte_with_no_params() {
        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let result = serial_do_write_byte(&mut ctx, &[]).expect("doWriteByte failed");
        assert_eq!(result, 1, "doWriteByte with no params returns success");
    }

    // ── Registration ─────────────────────────────────────────────

    #[test]
    fn register_serial_populates_table() {
        let mut table = NativeTable::new();
        register_serial(&mut table);

        assert_eq!(table.kit_name(SERIAL_KIT_ID), Some("serial"));
        assert_eq!(table.method_count(SERIAL_KIT_ID), SERIAL_METHOD_COUNT as usize);

        for id in 0..SERIAL_METHOD_COUNT {
            assert!(
                table.is_implemented(SERIAL_KIT_ID, id),
                "serial method {id} should be implemented"
            );
            let entry = table.lookup(SERIAL_KIT_ID, id).unwrap();
            assert!(
                matches!(entry, NativeEntry::Normal(_)),
                "serial method {id} should be Normal, got {entry:?}"
            );
        }
    }

    #[test]
    fn register_serial_with_custom_kit_id() {
        let mut table = NativeTable::new();
        register_serial_with_kit_id(&mut table, 42);

        assert_eq!(table.kit_name(42), Some("serial"));
        assert_eq!(table.implemented_count(42), SERIAL_METHOD_COUNT as usize);
    }

    #[test]
    fn register_serial_methods_callable_via_dispatch() {
        let mut table = NativeTable::new();
        register_serial(&mut table);

        let (mut mem, _) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);

        // doOpen
        let r = table.call(SERIAL_KIT_ID, 0, &mut ctx, &[0, 9600, 8, 1, 0, 0]).unwrap();
        assert_eq!(r, 1);

        // doClose
        let r = table.call(SERIAL_KIT_ID, 1, &mut ctx, &[]).unwrap();
        assert_eq!(r, 0);

        // doRead
        let r = table.call(SERIAL_KIT_ID, 2, &mut ctx, &[0, 0, 64]).unwrap();
        assert_eq!(r, 0);

        // doWrite
        let r = table.call(SERIAL_KIT_ID, 3, &mut ctx, &[0, 0, 10]).unwrap();
        assert_eq!(r, 10);

        // doReadByte
        let r = table.call(SERIAL_KIT_ID, 4, &mut ctx, &[]).unwrap();
        assert_eq!(r, -1);

        // doWriteByte
        let r = table.call(SERIAL_KIT_ID, 5, &mut ctx, &[0x41]).unwrap();
        assert_eq!(r, 1);
    }
}
