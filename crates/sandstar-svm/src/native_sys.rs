//! Kit 0 (sys) native methods — pure Rust implementations.
//!
//! Replaces the C implementations in `sys_Sys.c`, `sys_Sys_std.c`,
//! `sys_Sys_unix.c`, `sys_StdOutStream_std.c`, `sys_Str.c`,
//! `sys_PlatformService_unix.c`, and `sys_Test.c`.
//!
//! # Address model
//!
//! The original C VM on 32-bit ARM passed raw pointers as i32 Cell values.
//! On 64-bit hosts, pointers don't fit in i32. This module uses a handle-based
//! approach for malloc/free and stores formatted strings in a thread-local
//! buffer whose address is returned as an opaque handle via a lookup table.
//! Byte operations (copy, compareBytes, etc.) work on raw pointer values
//! which on 32-bit targets are identity and on 64-bit targets go through
//! a handle lookup.
//!
//! # Method ID mapping (from nativetable.c)
//!
//! | ID | Method                          | Width  |
//! |----|---------------------------------|--------|
//! |  0 | Sys.platformType()              | normal |
//! |  1 | Sys.copy(...)                   | normal |
//! |  2 | Sys.malloc(int)                 | normal |
//! |  3 | Sys.free(Obj)                   | normal |
//! |  4 | Sys.intStr(int)                 | normal |
//! |  5 | Sys.hexStr(int)                 | normal |
//! |  6 | Sys.longStr(long)               | normal |
//! |  7 | Sys.longHexStr(long)            | normal |
//! |  8 | Sys.floatStr(float)             | normal |
//! |  9 | Sys.doubleStr(double)           | normal |
//! | 10 | Sys.floatToBits(float)          | normal |
//! | 11 | Sys.doubleToBits(double)        | wide   |
//! | 12 | Sys.bitsToFloat(int)            | normal |
//! | 13 | Sys.bitsToDouble(long)          | wide   |
//! | 14 | Sys.ticks()                     | wide   |
//! | 15 | Sys.sleep(long)                 | normal |
//! | 16 | Sys.compareBytes(...)           | normal |
//! | 17 | Sys.setBytes(...)               | normal |
//! | 18 | Sys.andBytes(...)               | normal |
//! | 19 | Sys.orBytes(...)                | normal |
//! | 20 | Sys.scodeAddr()                 | normal |
//! | 21 | Sys.rand()                      | normal |
//! | 41 | StdOutStream.doWrite(int)       | normal |
//! | 42 | StdOutStream.doWriteBytes(...)  | normal |
//! | 43 | StdOutStream.doFlush()          | normal |
//! | 56 | Str.fromBytes(byte[],int)       | normal |
//! | 57 | PlatformService.doPlatformId()  | normal |
//! | 58 | PlatformService.getPlatVersion()| normal |
//! | 59 | PlatformService.getNativeMemAvailable() | wide |

use std::alloc::{self, Layout};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;
use std::time::Instant;

use crate::native_table::{NativeContext, NativeTable};
use crate::vm_error::{VmError, VmResult};

// ════════════════════════════════════════════════════════════════
// Pointer handle table (64-bit safe)
// ════════════════════════════════════════════════════════════════
//
// On 64-bit hosts, raw pointers don't fit in i32. We maintain a
// bidirectional mapping between i32 "handles" and real pointers.
// On 32-bit targets this is a no-op identity mapping.

static HANDLE_TABLE: Mutex<Option<HandleTable>> = Mutex::new(None);

struct HandleTable {
    /// Map from i32 handle → real pointer
    handle_to_ptr: HashMap<i32, *mut u8>,
    /// Map from real pointer → i32 handle
    ptr_to_handle: HashMap<usize, i32>,
    /// Next handle to assign (start at a value unlikely to collide)
    next_handle: i32,
    /// Layout tracking for deallocation
    layouts: HashMap<i32, Layout>,
}

// SAFETY: HandleTable contains raw pointers that are only used for allocation
// tracking. The pointers are obtained from alloc::alloc_zeroed and only
// deallocated via alloc::dealloc through the matching handle.
unsafe impl Send for HandleTable {}

impl HandleTable {
    fn new() -> Self {
        Self {
            handle_to_ptr: HashMap::new(),
            ptr_to_handle: HashMap::new(),
            // Start handles at 0x1000 to avoid confusion with NULL/small ints
            next_handle: 0x1000,
            layouts: HashMap::new(),
        }
    }

    fn alloc(&mut self, size: usize) -> Option<i32> {
        let layout = Layout::from_size_align(size, 8).ok()?;
        let ptr = unsafe { alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            return None;
        }
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1);
        self.handle_to_ptr.insert(handle, ptr);
        self.ptr_to_handle.insert(ptr as usize, handle);
        self.layouts.insert(handle, layout);
        Some(handle)
    }

    fn free(&mut self, handle: i32) -> bool {
        if let Some(ptr) = self.handle_to_ptr.remove(&handle) {
            self.ptr_to_handle.remove(&(ptr as usize));
            if let Some(layout) = self.layouts.remove(&handle) {
                unsafe { alloc::dealloc(ptr, layout); }
            }
            true
        } else {
            false
        }
    }

    fn get_ptr(&self, handle: i32) -> Option<*mut u8> {
        self.handle_to_ptr.get(&handle).copied()
    }

    /// Register a static/persistent pointer as a handle.
    fn register_static(&mut self, ptr: *const u8) -> i32 {
        let addr = ptr as usize;
        if let Some(&h) = self.ptr_to_handle.get(&addr) {
            return h;
        }
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1);
        self.handle_to_ptr.insert(handle, ptr as *mut u8);
        self.ptr_to_handle.insert(addr, handle);
        handle
    }
}

fn with_handle_table<F, R>(f: F) -> R
where
    F: FnOnce(&mut HandleTable) -> R,
{
    let mut guard = HANDLE_TABLE.lock().expect("handle table lock poisoned");
    let table = guard.get_or_insert_with(HandleTable::new);
    f(table)
}

/// Resolve an i32 handle to a real pointer. On 32-bit targets, handle == ptr.
/// Returns None if the handle is not found.
fn resolve_ptr(handle: i32) -> Option<*mut u8> {
    if handle == 0 {
        return None;
    }
    // On 32-bit, the handle IS the pointer
    if std::mem::size_of::<usize>() <= 4 {
        return Some(handle as usize as *mut u8);
    }
    // On 64-bit, look up in handle table
    with_handle_table(|t| t.get_ptr(handle))
}

// ════════════════════════════════════════════════════════════════
// String formatting buffer
// ════════════════════════════════════════════════════════════════

// Thread-local buffer matching C `static char strbuf[32]`.
// We use 64 bytes for safety.
thread_local! {
    static STR_BUF: RefCell<[u8; 64]> = const { RefCell::new([0u8; 64]) };
    /// Handle for the STR_BUF in the handle table
    static STR_BUF_HANDLE: RefCell<i32> = const { RefCell::new(0) };
}

/// Write a formatted string into the thread-local buffer and return
/// its handle as an i32.
fn format_to_strbuf(s: &str) -> i32 {
    STR_BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        let bytes = s.as_bytes();
        let len = bytes.len().min(63); // leave room for NUL
        buf[..len].copy_from_slice(&bytes[..len]);
        buf[len] = 0; // NUL terminate

        let ptr = buf.as_ptr();

        if std::mem::size_of::<usize>() <= 4 {
            // 32-bit: return raw pointer directly
            ptr as i32
        } else {
            // 64-bit: register in handle table and return handle
            let handle = with_handle_table(|t| t.register_static(ptr));
            STR_BUF_HANDLE.with(|h| *h.borrow_mut() = handle);
            handle
        }
    })
}

/// Read a NUL-terminated string from a handle. Returns None if handle is invalid.
fn read_cstr_from_handle(handle: i32) -> Option<String> {
    let ptr = resolve_ptr(handle)?;
    unsafe {
        let cstr = std::ffi::CStr::from_ptr(ptr as *const std::ffi::c_char);
        cstr.to_str().ok().map(|s| s.to_string())
    }
}

// ════════════════════════════════════════════════════════════════
// Monotonic clock base for ticks()
// ════════════════════════════════════════════════════════════════

thread_local! {
    static TICKS_BASE: Instant = Instant::now();
}

// ════════════════════════════════════════════════════════════════
// Static string constants
// ════════════════════════════════════════════════════════════════

static PLATFORM_TYPE: &[u8] = b"sys::PlatformService\0";
static PLATFORM_ID: &[u8] = b"sandstar-rust\0";
static PLAT_VERSION: &[u8] = b"1.0.0\0";

/// Return a handle for a static byte string.
fn static_str_handle(s: &'static [u8]) -> i32 {
    if std::mem::size_of::<usize>() <= 4 {
        s.as_ptr() as i32
    } else {
        with_handle_table(|t| t.register_static(s.as_ptr()))
    }
}

// ════════════════════════════════════════════════════════════════
// String formatting methods (IDs 4-9)
// ════════════════════════════════════════════════════════════════

/// 0::4 — Sys.intStr(int): format i32 as decimal string
pub fn sys_int_str(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let val = params.first().copied().unwrap_or(0);
    let s = format!("{val}");
    Ok(format_to_strbuf(&s))
}

/// 0::5 — Sys.hexStr(int): format i32 as lowercase hex string
pub fn sys_hex_str(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let val = params.first().copied().unwrap_or(0);
    // C uses %x which treats the value as unsigned for hex output
    let s = format!("{:x}", val as u32);
    Ok(format_to_strbuf(&s))
}

/// 0::6 — Sys.longStr(long): format i64 as decimal (wide param: lo, hi)
pub fn sys_long_str(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let lo = params.first().copied().unwrap_or(0) as u32;
    let hi = params.get(1).copied().unwrap_or(0) as u32;
    let val = ((hi as i64) << 32) | (lo as i64);
    let s = format!("{val}");
    Ok(format_to_strbuf(&s))
}

/// 0::7 — Sys.longHexStr(long): format i64 as lowercase hex
pub fn sys_long_hex_str(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let lo = params.first().copied().unwrap_or(0) as u32;
    let hi = params.get(1).copied().unwrap_or(0) as u32;
    let val = ((hi as u64) << 32) | (lo as u64);
    let s = format!("{val:x}");
    Ok(format_to_strbuf(&s))
}

/// 0::8 — Sys.floatStr(float): format f32 as string (C %f format)
pub fn sys_float_str(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let bits = params.first().copied().unwrap_or(0) as u32;
    let val = f32::from_bits(bits);
    // C sprintf %f gives 6 decimal places
    let s = format!("{val:.6}");
    Ok(format_to_strbuf(&s))
}

/// 0::9 — Sys.doubleStr(double): format f64 as string (wide param)
pub fn sys_double_str(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let lo = params.first().copied().unwrap_or(0) as u32;
    let hi = params.get(1).copied().unwrap_or(0) as u32;
    let bits = ((hi as u64) << 32) | (lo as u64);
    let val = f64::from_bits(bits);
    // C sprintf %lf gives 6 decimal places
    let s = format!("{val:.6}");
    Ok(format_to_strbuf(&s))
}

// ════════════════════════════════════════════════════════════════
// Memory/byte operations (IDs 1-3, 16-19)
// ════════════════════════════════════════════════════════════════

/// 0::2 — Sys.malloc(int): allocate zeroed memory, return handle as i32
pub fn sys_malloc(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let size = params.first().copied().unwrap_or(0);
    if size <= 0 {
        return Ok(0); // NULL
    }

    if std::mem::size_of::<usize>() <= 4 {
        // 32-bit: return raw pointer
        let layout = Layout::from_size_align(size as usize, 8).map_err(|e| {
            VmError::NativeError {
                kit: 0,
                method: 2,
                message: format!("invalid layout: {e}"),
            }
        })?;
        let ptr = unsafe { alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            return Ok(0);
        }
        Ok(ptr as i32)
    } else {
        // 64-bit: use handle table
        let handle = with_handle_table(|t| t.alloc(size as usize));
        Ok(handle.unwrap_or(0))
    }
}

/// 0::3 — Sys.free(Obj): free previously allocated memory
pub fn sys_free(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let handle = params.first().copied().unwrap_or(0);
    if handle == 0 {
        return Ok(0); // free(NULL) is a no-op
    }

    if std::mem::size_of::<usize>() <= 4 {
        // 32-bit: handle IS the pointer — but we don't track layouts on 32-bit,
        // so we can't safely dealloc. In production (BeagleBone), the C VM
        // uses real malloc/free. For now, skip dealloc.
        // TODO: track 32-bit allocations too
        Ok(0)
    } else {
        with_handle_table(|t| { t.free(handle); });
        Ok(0)
    }
}

/// 0::1 — Sys.copy(src, srcOff, dest, destOff, num): memmove
///
/// On the pure-Rust VM, src/dest are handles. We resolve them to real pointers.
pub fn sys_copy(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let src_handle = params.first().copied().unwrap_or(0);
    let src_off = params.get(1).copied().unwrap_or(0) as usize;
    let dest_handle = params.get(2).copied().unwrap_or(0);
    let dest_off = params.get(3).copied().unwrap_or(0) as usize;
    let num = params.get(4).copied().unwrap_or(0);

    if num <= 0 {
        return Ok(0);
    }
    let num = num as usize;

    let src_ptr = resolve_ptr(src_handle).ok_or_else(|| VmError::NativeError {
        kit: 0,
        method: 1,
        message: "copy: null source pointer".into(),
    })?;
    let dest_ptr = resolve_ptr(dest_handle).ok_or_else(|| VmError::NativeError {
        kit: 0,
        method: 1,
        message: "copy: null dest pointer".into(),
    })?;

    unsafe {
        std::ptr::copy(src_ptr.add(src_off), dest_ptr.add(dest_off), num);
    }
    Ok(0)
}

/// 0::16 — Sys.compareBytes(a, aOff, b, bOff, len): memcmp returning -1/0/1
pub fn sys_compare_bytes(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let a_handle = params.first().copied().unwrap_or(0);
    let a_off = params.get(1).copied().unwrap_or(0) as usize;
    let b_handle = params.get(2).copied().unwrap_or(0);
    let b_off = params.get(3).copied().unwrap_or(0) as usize;
    let len = params.get(4).copied().unwrap_or(0);

    if len <= 0 {
        return Ok(0);
    }
    let len = len as usize;

    let a_ptr = resolve_ptr(a_handle).ok_or_else(|| VmError::NativeError {
        kit: 0,
        method: 16,
        message: "compareBytes: null pointer a".into(),
    })?;
    let b_ptr = resolve_ptr(b_handle).ok_or_else(|| VmError::NativeError {
        kit: 0,
        method: 16,
        message: "compareBytes: null pointer b".into(),
    })?;

    unsafe {
        let a = a_ptr.add(a_off);
        let b = b_ptr.add(b_off);
        for i in 0..len {
            let ai = *a.add(i) as i32;
            let bi = *b.add(i) as i32;
            if ai != bi {
                return Ok(if ai < bi { -1 } else { 1 });
            }
        }
    }
    Ok(0)
}

/// 0::17 — Sys.setBytes(val, bytes, off, len): memset
pub fn sys_set_bytes(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let val = params.first().copied().unwrap_or(0) as u8;
    let bytes_handle = params.get(1).copied().unwrap_or(0);
    let off = params.get(2).copied().unwrap_or(0) as usize;
    let len = params.get(3).copied().unwrap_or(0);

    if len <= 0 || bytes_handle == 0 {
        return Ok(0);
    }
    let len = len as usize;

    let ptr = resolve_ptr(bytes_handle).ok_or_else(|| VmError::NativeError {
        kit: 0,
        method: 17,
        message: "setBytes: null pointer".into(),
    })?;

    unsafe {
        std::ptr::write_bytes(ptr.add(off), val, len);
    }
    Ok(0)
}

/// 0::18 — Sys.andBytes(mask, bytes, off, len): AND each byte with mask
pub fn sys_and_bytes(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let mask = params.first().copied().unwrap_or(0) as u8;
    let bytes_handle = params.get(1).copied().unwrap_or(0);
    let off = params.get(2).copied().unwrap_or(0) as usize;
    let len = params.get(3).copied().unwrap_or(0);

    if len <= 0 || bytes_handle == 0 {
        return Ok(0);
    }
    let len = len as usize;

    let ptr = resolve_ptr(bytes_handle).ok_or_else(|| VmError::NativeError {
        kit: 0,
        method: 18,
        message: "andBytes: null pointer".into(),
    })?;

    unsafe {
        let base = ptr.add(off);
        for i in 0..len {
            *base.add(i) &= mask;
        }
    }
    Ok(0)
}

/// 0::19 — Sys.orBytes(mask, bytes, off, len): OR each byte with mask
pub fn sys_or_bytes(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let mask = params.first().copied().unwrap_or(0) as u8;
    let bytes_handle = params.get(1).copied().unwrap_or(0);
    let off = params.get(2).copied().unwrap_or(0) as usize;
    let len = params.get(3).copied().unwrap_or(0);

    if len <= 0 || bytes_handle == 0 {
        return Ok(0);
    }
    let len = len as usize;

    let ptr = resolve_ptr(bytes_handle).ok_or_else(|| VmError::NativeError {
        kit: 0,
        method: 19,
        message: "orBytes: null pointer".into(),
    })?;

    unsafe {
        let base = ptr.add(off);
        for i in 0..len {
            *base.add(i) |= mask;
        }
    }
    Ok(0)
}

// ════════════════════════════════════════════════════════════════
// Bit conversion (IDs 10-13)
// ════════════════════════════════════════════════════════════════

/// 0::10 — Sys.floatToBits(float): identity — float bits stored as int in Cell
pub fn sys_float_to_bits(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    Ok(params.first().copied().unwrap_or(0))
}

/// 0::12 — Sys.bitsToFloat(int): identity — int bits reinterpreted as float
pub fn sys_bits_to_float(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    Ok(params.first().copied().unwrap_or(0))
}

/// 0::11 — Sys.doubleToBits(double): identity — double bits as i64 (wide)
pub fn sys_double_to_bits(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i64> {
    let lo = params.first().copied().unwrap_or(0) as u32;
    let hi = params.get(1).copied().unwrap_or(0) as u32;
    Ok(((hi as i64) << 32) | (lo as i64))
}

/// 0::13 — Sys.bitsToDouble(long): identity — i64 bits reinterpreted as double
pub fn sys_bits_to_double(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i64> {
    let lo = params.first().copied().unwrap_or(0) as u32;
    let hi = params.get(1).copied().unwrap_or(0) as u32;
    Ok(((hi as i64) << 32) | (lo as i64))
}

// ════════════════════════════════════════════════════════════════
// Platform/timing (IDs 0, 14, 15, 20, 21)
// ════════════════════════════════════════════════════════════════

/// 0::0 — Sys.platformType(): return "sys::PlatformService" string handle
pub fn sys_platform_type(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    Ok(static_str_handle(PLATFORM_TYPE))
}

/// 0::14 — Sys.ticks(): monotonic nanosecond counter (wide return)
pub fn sys_ticks(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i64> {
    let ns = TICKS_BASE.with(|base| base.elapsed().as_nanos() as i64);
    Ok(ns)
}

/// 0::15 — Sys.sleep(ns_lo, ns_hi): sleep for nanoseconds (wide param)
pub fn sys_sleep(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let lo = params.first().copied().unwrap_or(0) as u32;
    let hi = params.get(1).copied().unwrap_or(0) as u32;
    let ns = ((hi as i64) << 32) | (lo as i64);

    if ns <= 0 {
        return Ok(0);
    }
    std::thread::sleep(std::time::Duration::from_nanos(ns as u64));
    Ok(0)
}

/// 0::21 — Sys.rand(): return random integer
pub fn sys_rand(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    thread_local! {
        static STATE: RefCell<u32> = RefCell::new({
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u32;
            if now == 0 { 1 } else { now }
        });
    }
    STATE.with(|s| {
        let mut x = *s.borrow();
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *s.borrow_mut() = x;
        Ok(x as i32)
    })
}

/// 0::20 — Sys.scodeAddr(): return code base address
pub fn sys_scode_addr(ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    if std::mem::size_of::<usize>() <= 4 {
        Ok(ctx.memory.as_ptr() as i32)
    } else {
        // On 64-bit, register memory base as a handle
        let ptr = ctx.memory.as_ptr();
        Ok(with_handle_table(|t| t.register_static(ptr)))
    }
}

// ════════════════════════════════════════════════════════════════
// StdOut (IDs 41-43)
// ════════════════════════════════════════════════════════════════

/// 0::41 — StdOutStream.doWrite(int): write single byte to stdout
pub fn sys_stdout_write(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let b = params.first().copied().unwrap_or(0) as u8;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let _ = handle.write_all(&[b]);
    if b == b'\n' {
        let _ = handle.flush();
    }
    Ok(1) // true
}

/// 0::42 — StdOutStream.doWriteBytes(buf_handle, off, len): write bytes to stdout
pub fn sys_stdout_write_bytes(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let buf_handle = params.first().copied().unwrap_or(0);
    let off = params.get(1).copied().unwrap_or(0) as usize;
    let len = params.get(2).copied().unwrap_or(0);

    if len <= 0 || buf_handle == 0 {
        return Ok(1); // true
    }
    let len = len as usize;

    if let Some(ptr) = resolve_ptr(buf_handle) {
        unsafe {
            let slice = std::slice::from_raw_parts(ptr.add(off), len);
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            let _ = handle.write_all(slice);
        }
    }
    Ok(1) // true
}

/// 0::43 — StdOutStream.doFlush(): flush stdout
pub fn sys_stdout_flush(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    let _ = std::io::stdout().flush();
    Ok(0)
}

// ════════════════════════════════════════════════════════════════
// Str (ID 56)
// ════════════════════════════════════════════════════════════════

/// 0::56 — Str.fromBytes(buf_handle, off): return buf+off as string handle
pub fn sys_str_from_bytes(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let buf_handle = params.first().copied().unwrap_or(0);
    let off = params.get(1).copied().unwrap_or(0) as usize;

    if buf_handle == 0 {
        return Ok(0);
    }

    if std::mem::size_of::<usize>() <= 4 {
        // 32-bit: pointer arithmetic
        Ok((buf_handle as usize + off) as i32)
    } else {
        // 64-bit: resolve, offset, re-register
        if let Some(ptr) = resolve_ptr(buf_handle) {
            let new_ptr = unsafe { ptr.add(off) };
            Ok(with_handle_table(|t| t.register_static(new_ptr)))
        } else {
            Ok(0)
        }
    }
}

// ════════════════════════════════════════════════════════════════
// PlatformService (IDs 57-59)
// ════════════════════════════════════════════════════════════════

/// 0::57 — PlatformService.doPlatformId(): return platform ID string
pub fn sys_platform_service_id(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    Ok(static_str_handle(PLATFORM_ID))
}

/// 0::58 — PlatformService.getPlatVersion(): return platform version string
pub fn sys_platform_service_version(
    _ctx: &mut NativeContext<'_>,
    _params: &[i32],
) -> VmResult<i32> {
    Ok(static_str_handle(PLAT_VERSION))
}

/// 0::59 — PlatformService.getNativeMemAvailable(): return free memory (wide)
pub fn sys_platform_service_mem_available(
    _ctx: &mut NativeContext<'_>,
    _params: &[i32],
) -> VmResult<i64> {
    Ok(64 * 1024 * 1024) // 64 MB default
}

// ════════════════════════════════════════════════════════════════
// Registration
// ════════════════════════════════════════════════════════════════

/// Register all Kit 0 (sys) native methods that have pure-Rust implementations.
///
/// Methods not covered here (Component invoke/get/set, FileStore, Type.malloc,
/// Test.doMain) remain as stubs — they require VM call-back capabilities
/// or filesystem access that will be implemented in later phases.
pub fn register_kit0_sys(table: &mut NativeTable) {
    // Sys methods (0::0 through 0::21)
    table.register(0, 0, sys_platform_type);
    table.register(0, 1, sys_copy);
    table.register(0, 2, sys_malloc);
    table.register(0, 3, sys_free);
    table.register(0, 4, sys_int_str);
    table.register(0, 5, sys_hex_str);
    table.register(0, 6, sys_long_str);
    table.register(0, 7, sys_long_hex_str);
    table.register(0, 8, sys_float_str);
    table.register(0, 9, sys_double_str);
    table.register(0, 10, sys_float_to_bits);
    table.register_wide(0, 11, sys_double_to_bits);
    table.register(0, 12, sys_bits_to_float);
    table.register_wide(0, 13, sys_bits_to_double);
    table.register_wide(0, 14, sys_ticks);
    table.register(0, 15, sys_sleep);
    table.register(0, 16, sys_compare_bytes);
    table.register(0, 17, sys_set_bytes);
    table.register(0, 18, sys_and_bytes);
    table.register(0, 19, sys_or_bytes);
    table.register(0, 20, sys_scode_addr);
    table.register(0, 21, sys_rand);

    // StdOutStream (0::41 through 0::43)
    table.register(0, 41, sys_stdout_write);
    table.register(0, 42, sys_stdout_write_bytes);
    table.register(0, 43, sys_stdout_flush);

    // Str (0::56)
    table.register(0, 56, sys_str_from_bytes);

    // PlatformService (0::57 through 0::59)
    table.register(0, 57, sys_platform_service_id);
    table.register(0, 58, sys_platform_service_version);
    table.register_wide(0, 59, sys_platform_service_mem_available);
}

// ════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_table::NativeContext;

    fn test_ctx(mem: &mut Vec<u8>) -> NativeContext<'_> {
        NativeContext::new(mem)
    }

    // ── String formatting ────────────────────────────────────

    #[test]
    fn int_str_zero() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_int_str(&mut ctx, &[0]).unwrap();
        let s = read_cstr_from_handle(handle).unwrap();
        assert_eq!(s, "0");
    }

    #[test]
    fn int_str_negative() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_int_str(&mut ctx, &[-1]).unwrap();
        let s = read_cstr_from_handle(handle).unwrap();
        assert_eq!(s, "-1");
    }

    #[test]
    fn int_str_positive() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_int_str(&mut ctx, &[42]).unwrap();
        let s = read_cstr_from_handle(handle).unwrap();
        assert_eq!(s, "42");
    }

    #[test]
    fn int_str_max() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_int_str(&mut ctx, &[i32::MAX]).unwrap();
        let s = read_cstr_from_handle(handle).unwrap();
        assert_eq!(s, "2147483647");
    }

    #[test]
    fn hex_str_ff() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_hex_str(&mut ctx, &[0xff]).unwrap();
        let s = read_cstr_from_handle(handle).unwrap();
        assert_eq!(s, "ff");
    }

    #[test]
    fn hex_str_dead() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_hex_str(&mut ctx, &[0xdead_i32]).unwrap();
        let s = read_cstr_from_handle(handle).unwrap();
        assert_eq!(s, "dead");
    }

    #[test]
    fn hex_str_negative() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_hex_str(&mut ctx, &[-1]).unwrap();
        let s = read_cstr_from_handle(handle).unwrap();
        assert_eq!(s, "ffffffff");
    }

    #[test]
    fn float_str_basic() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let bits = 3.14_f32.to_bits() as i32;
        let handle = sys_float_str(&mut ctx, &[bits]).unwrap();
        let s = read_cstr_from_handle(handle).unwrap();
        let val: f64 = s.parse().unwrap();
        assert!((val - 3.14).abs() < 0.001);
    }

    #[test]
    fn float_str_zero() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let bits = 0.0_f32.to_bits() as i32;
        let handle = sys_float_str(&mut ctx, &[bits]).unwrap();
        let s = read_cstr_from_handle(handle).unwrap();
        assert_eq!(s, "0.000000");
    }

    #[test]
    fn long_str_basic() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let val: i64 = 1_000_000_000_000;
        let lo = val as i32;
        let hi = (val >> 32) as i32;
        let handle = sys_long_str(&mut ctx, &[lo, hi]).unwrap();
        let s = read_cstr_from_handle(handle).unwrap();
        assert_eq!(s, "1000000000000");
    }

    #[test]
    fn long_hex_str_basic() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let val: i64 = 0xDEAD_BEEF_CAFE;
        let lo = val as i32;
        let hi = (val >> 32) as i32;
        let handle = sys_long_hex_str(&mut ctx, &[lo, hi]).unwrap();
        let s = read_cstr_from_handle(handle).unwrap();
        assert_eq!(s, "deadbeefcafe");
    }

    #[test]
    fn double_str_basic() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let bits = 3.14159_f64.to_bits();
        let lo = bits as i32;
        let hi = (bits >> 32) as i32;
        let handle = sys_double_str(&mut ctx, &[lo, hi]).unwrap();
        let s = read_cstr_from_handle(handle).unwrap();
        let val: f64 = s.parse().unwrap();
        assert!((val - 3.14159).abs() < 0.0001);
    }

    // ── Float/bit conversion roundtrips ──────────────────────

    #[test]
    fn float_to_bits_roundtrip() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let original = 42.5_f32.to_bits() as i32;
        let bits = sys_float_to_bits(&mut ctx, &[original]).unwrap();
        let back = sys_bits_to_float(&mut ctx, &[bits]).unwrap();
        assert_eq!(original, back);
        assert_eq!(f32::from_bits(back as u32), 42.5);
    }

    #[test]
    fn float_to_bits_zero() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let bits = sys_float_to_bits(&mut ctx, &[0]).unwrap();
        assert_eq!(bits, 0);
        assert_eq!(f32::from_bits(bits as u32), 0.0);
    }

    #[test]
    fn float_to_bits_negative() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let original = (-1.0_f32).to_bits() as i32;
        let bits = sys_float_to_bits(&mut ctx, &[original]).unwrap();
        assert_eq!(f32::from_bits(bits as u32), -1.0);
    }

    #[test]
    fn double_to_bits_roundtrip() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let original = 123.456_f64.to_bits();
        let lo = original as i32;
        let hi = (original >> 32) as i32;
        let result = sys_double_to_bits(&mut ctx, &[lo, hi]).unwrap();
        let back = sys_bits_to_double(&mut ctx, &[result as i32, (result >> 32) as i32]).unwrap();
        assert_eq!(result, back);
        assert_eq!(f64::from_bits(back as u64), 123.456);
    }

    #[test]
    fn double_to_bits_zero() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let result = sys_double_to_bits(&mut ctx, &[0, 0]).unwrap();
        assert_eq!(f64::from_bits(result as u64), 0.0);
    }

    // ── Timing ───────────────────────────────────────────────

    #[test]
    fn ticks_returns_positive() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        std::thread::sleep(std::time::Duration::from_millis(1));
        let t = sys_ticks(&mut ctx, &[]).unwrap();
        assert!(t > 0, "ticks should be positive, got {t}");
    }

    #[test]
    fn ticks_monotonically_increasing() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let t1 = sys_ticks(&mut ctx, &[]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let t2 = sys_ticks(&mut ctx, &[]).unwrap();
        assert!(t2 > t1, "ticks should increase: t1={t1}, t2={t2}");
    }

    #[test]
    fn sleep_does_not_panic() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let ns: i64 = 1_000_000; // 1ms
        let lo = ns as i32;
        let hi = (ns >> 32) as i32;
        let result = sys_sleep(&mut ctx, &[lo, hi]);
        assert!(result.is_ok());
    }

    #[test]
    fn sleep_zero_is_noop() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(sys_sleep(&mut ctx, &[0, 0]).unwrap(), 0);
    }

    #[test]
    fn sleep_negative_is_noop() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(sys_sleep(&mut ctx, &[-1, -1]).unwrap(), 0);
    }

    // ── rand ─────────────────────────────────────────────────

    #[test]
    fn rand_returns_a_value() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let _ = sys_rand(&mut ctx, &[]).unwrap();
    }

    #[test]
    fn rand_produces_different_values() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let r1 = sys_rand(&mut ctx, &[]).unwrap();
        let r2 = sys_rand(&mut ctx, &[]).unwrap();
        assert_ne!(r1, r2, "two consecutive rand() should differ");
    }

    // ── Memory operations via handle table ───────────────────

    #[test]
    fn malloc_returns_non_zero_for_valid_size() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_malloc(&mut ctx, &[64]).unwrap();
        assert_ne!(handle, 0, "malloc(64) should return non-zero");
        sys_free(&mut ctx, &[handle]).unwrap();
    }

    #[test]
    fn malloc_returns_zero_for_zero_size() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_malloc(&mut ctx, &[0]).unwrap();
        assert_eq!(handle, 0);
    }

    #[test]
    fn malloc_returns_zero_for_negative_size() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_malloc(&mut ctx, &[-1]).unwrap();
        assert_eq!(handle, 0);
    }

    #[test]
    fn malloc_zeroes_memory() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_malloc(&mut ctx, &[16]).unwrap();
        assert_ne!(handle, 0);
        let ptr = resolve_ptr(handle).expect("should resolve");
        unsafe {
            let slice = std::slice::from_raw_parts(ptr, 16);
            assert!(slice.iter().all(|&b| b == 0), "malloc memory should be zeroed");
        }
        sys_free(&mut ctx, &[handle]).unwrap();
    }

    #[test]
    fn free_does_not_panic_on_valid_pointer() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_malloc(&mut ctx, &[32]).unwrap();
        assert_ne!(handle, 0);
        let result = sys_free(&mut ctx, &[handle]);
        assert!(result.is_ok());
    }

    #[test]
    fn free_null_is_noop() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let result = sys_free(&mut ctx, &[0]);
        assert!(result.is_ok());
    }

    // ── compareBytes via malloc'd buffers ─────────────────────

    #[test]
    fn compare_bytes_equal() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let a = sys_malloc(&mut ctx, &[4]).unwrap();
        let b = sys_malloc(&mut ctx, &[4]).unwrap();
        // Write same data to both
        let a_ptr = resolve_ptr(a).unwrap();
        let b_ptr = resolve_ptr(b).unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping([1u8, 2, 3, 4].as_ptr(), a_ptr, 4);
            std::ptr::copy_nonoverlapping([1u8, 2, 3, 4].as_ptr(), b_ptr, 4);
        }
        let result = sys_compare_bytes(&mut ctx, &[a, 0, b, 0, 4]).unwrap();
        assert_eq!(result, 0);
        sys_free(&mut ctx, &[a]).unwrap();
        sys_free(&mut ctx, &[b]).unwrap();
    }

    #[test]
    fn compare_bytes_less() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let a = sys_malloc(&mut ctx, &[4]).unwrap();
        let b = sys_malloc(&mut ctx, &[4]).unwrap();
        let a_ptr = resolve_ptr(a).unwrap();
        let b_ptr = resolve_ptr(b).unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping([1u8, 2, 3, 4].as_ptr(), a_ptr, 4);
            std::ptr::copy_nonoverlapping([1u8, 2, 4, 4].as_ptr(), b_ptr, 4);
        }
        let result = sys_compare_bytes(&mut ctx, &[a, 0, b, 0, 4]).unwrap();
        assert_eq!(result, -1);
        sys_free(&mut ctx, &[a]).unwrap();
        sys_free(&mut ctx, &[b]).unwrap();
    }

    #[test]
    fn compare_bytes_greater() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let a = sys_malloc(&mut ctx, &[4]).unwrap();
        let b = sys_malloc(&mut ctx, &[4]).unwrap();
        let a_ptr = resolve_ptr(a).unwrap();
        let b_ptr = resolve_ptr(b).unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping([1u8, 2, 5, 4].as_ptr(), a_ptr, 4);
            std::ptr::copy_nonoverlapping([1u8, 2, 3, 4].as_ptr(), b_ptr, 4);
        }
        let result = sys_compare_bytes(&mut ctx, &[a, 0, b, 0, 4]).unwrap();
        assert_eq!(result, 1);
        sys_free(&mut ctx, &[a]).unwrap();
        sys_free(&mut ctx, &[b]).unwrap();
    }

    #[test]
    fn compare_bytes_with_offset() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let a = sys_malloc(&mut ctx, &[4]).unwrap();
        let b = sys_malloc(&mut ctx, &[4]).unwrap();
        let a_ptr = resolve_ptr(a).unwrap();
        let b_ptr = resolve_ptr(b).unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping([0u8, 0, 1, 2].as_ptr(), a_ptr, 4);
            std::ptr::copy_nonoverlapping([1u8, 2, 0, 0].as_ptr(), b_ptr, 4);
        }
        let result = sys_compare_bytes(&mut ctx, &[a, 2, b, 0, 2]).unwrap();
        assert_eq!(result, 0);
        sys_free(&mut ctx, &[a]).unwrap();
        sys_free(&mut ctx, &[b]).unwrap();
    }

    #[test]
    fn compare_bytes_zero_length() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let result = sys_compare_bytes(&mut ctx, &[0, 0, 0, 0, 0]).unwrap();
        assert_eq!(result, 0);
    }

    // ── setBytes via malloc ──────────────────────────────────

    #[test]
    fn set_bytes_writes_correct_values() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_malloc(&mut ctx, &[8]).unwrap();
        sys_set_bytes(&mut ctx, &[0xAB_i32, handle, 2, 4]).unwrap();
        let ptr = resolve_ptr(handle).unwrap();
        unsafe {
            let slice = std::slice::from_raw_parts(ptr, 8);
            assert_eq!(slice, &[0, 0, 0xAB, 0xAB, 0xAB, 0xAB, 0, 0]);
        }
        sys_free(&mut ctx, &[handle]).unwrap();
    }

    #[test]
    fn set_bytes_zero_length_noop() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_malloc(&mut ctx, &[4]).unwrap();
        // Fill with 1s first
        let ptr = resolve_ptr(handle).unwrap();
        unsafe { std::ptr::write_bytes(ptr, 1, 4); }
        sys_set_bytes(&mut ctx, &[0, handle, 0, 0]).unwrap();
        unsafe {
            let slice = std::slice::from_raw_parts(ptr, 4);
            assert_eq!(slice, &[1, 1, 1, 1]); // unchanged
        }
        sys_free(&mut ctx, &[handle]).unwrap();
    }

    // ── andBytes / orBytes ───────────────────────────────────

    #[test]
    fn and_bytes_applies_mask() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_malloc(&mut ctx, &[4]).unwrap();
        let ptr = resolve_ptr(handle).unwrap();
        unsafe { std::ptr::write_bytes(ptr, 0xFF, 4); }
        sys_and_bytes(&mut ctx, &[0x0F_i32, handle, 0, 4]).unwrap();
        unsafe {
            let slice = std::slice::from_raw_parts(ptr, 4);
            assert_eq!(slice, &[0x0F, 0x0F, 0x0F, 0x0F]);
        }
        sys_free(&mut ctx, &[handle]).unwrap();
    }

    #[test]
    fn or_bytes_applies_mask() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_malloc(&mut ctx, &[4]).unwrap();
        let ptr = resolve_ptr(handle).unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping([0x00u8, 0x01, 0x02, 0x03].as_ptr(), ptr, 4);
        }
        sys_or_bytes(&mut ctx, &[0xF0_i32, handle, 0, 4]).unwrap();
        unsafe {
            let slice = std::slice::from_raw_parts(ptr, 4);
            assert_eq!(slice, &[0xF0, 0xF1, 0xF2, 0xF3]);
        }
        sys_free(&mut ctx, &[handle]).unwrap();
    }

    #[test]
    fn and_bytes_with_offset() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_malloc(&mut ctx, &[4]).unwrap();
        let ptr = resolve_ptr(handle).unwrap();
        unsafe { std::ptr::write_bytes(ptr, 0xFF, 4); }
        sys_and_bytes(&mut ctx, &[0x0F_i32, handle, 1, 2]).unwrap();
        unsafe {
            let slice = std::slice::from_raw_parts(ptr, 4);
            assert_eq!(slice, &[0xFF, 0x0F, 0x0F, 0xFF]);
        }
        sys_free(&mut ctx, &[handle]).unwrap();
    }

    #[test]
    fn or_bytes_with_offset() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_malloc(&mut ctx, &[4]).unwrap();
        // Already zeroed from malloc
        sys_or_bytes(&mut ctx, &[0xAA_i32, handle, 2, 2]).unwrap();
        let ptr = resolve_ptr(handle).unwrap();
        unsafe {
            let slice = std::slice::from_raw_parts(ptr, 4);
            assert_eq!(slice, &[0x00, 0x00, 0xAA, 0xAA]);
        }
        sys_free(&mut ctx, &[handle]).unwrap();
    }

    // ── copy ─────────────────────────────────────────────────

    #[test]
    fn copy_non_overlapping() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let src = sys_malloc(&mut ctx, &[4]).unwrap();
        let dest = sys_malloc(&mut ctx, &[4]).unwrap();
        let src_ptr = resolve_ptr(src).unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping([10u8, 20, 30, 40].as_ptr(), src_ptr, 4);
        }
        sys_copy(&mut ctx, &[src, 0, dest, 0, 4]).unwrap();
        let dest_ptr = resolve_ptr(dest).unwrap();
        unsafe {
            let slice = std::slice::from_raw_parts(dest_ptr, 4);
            assert_eq!(slice, &[10, 20, 30, 40]);
        }
        sys_free(&mut ctx, &[src]).unwrap();
        sys_free(&mut ctx, &[dest]).unwrap();
    }

    #[test]
    fn copy_with_offsets() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let src = sys_malloc(&mut ctx, &[4]).unwrap();
        let dest = sys_malloc(&mut ctx, &[4]).unwrap();
        let src_ptr = resolve_ptr(src).unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping([0u8, 0, 0xAA, 0xBB].as_ptr(), src_ptr, 4);
        }
        sys_copy(&mut ctx, &[src, 2, dest, 1, 2]).unwrap();
        let dest_ptr = resolve_ptr(dest).unwrap();
        unsafe {
            let slice = std::slice::from_raw_parts(dest_ptr, 4);
            assert_eq!(slice, &[0, 0xAA, 0xBB, 0]);
        }
        sys_free(&mut ctx, &[src]).unwrap();
        sys_free(&mut ctx, &[dest]).unwrap();
    }

    #[test]
    fn copy_zero_length_noop() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        assert_eq!(sys_copy(&mut ctx, &[0, 0, 0, 0, 0]).unwrap(), 0);
    }

    // ── platformType ─────────────────────────────────────────

    #[test]
    fn platform_type_returns_non_zero() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_platform_type(&mut ctx, &[]).unwrap();
        assert_ne!(handle, 0);
        let s = read_cstr_from_handle(handle).unwrap();
        assert_eq!(s, "sys::PlatformService");
    }

    // ── scodeAddr ────────────────────────────────────────────

    #[test]
    fn scode_addr_returns_non_zero() {
        let mut mem = vec![0u8; 64];
        let mut ctx = test_ctx(&mut mem);
        let addr = sys_scode_addr(&mut ctx, &[]).unwrap();
        assert_ne!(addr, 0);
    }

    // ── StdOut ───────────────────────────────────────────────

    #[test]
    fn stdout_write_returns_true() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let result = sys_stdout_write(&mut ctx, &[b'A' as i32]).unwrap();
        assert_eq!(result, 1);
    }

    #[test]
    fn stdout_flush_returns_zero() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let result = sys_stdout_flush(&mut ctx, &[]).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn stdout_write_bytes_returns_true() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let buf_handle = sys_malloc(&mut ctx, &[5]).unwrap();
        let ptr = resolve_ptr(buf_handle).unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping(b"hello".as_ptr(), ptr, 5);
        }
        let result = sys_stdout_write_bytes(&mut ctx, &[buf_handle, 0, 5]).unwrap();
        assert_eq!(result, 1);
        sys_free(&mut ctx, &[buf_handle]).unwrap();
    }

    // ── PlatformService ──────────────────────────────────────

    #[test]
    fn platform_service_id_returns_non_zero() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_platform_service_id(&mut ctx, &[]).unwrap();
        assert_ne!(handle, 0);
        let s = read_cstr_from_handle(handle).unwrap();
        assert_eq!(s, "sandstar-rust");
    }

    #[test]
    fn platform_service_version_returns_non_zero() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let handle = sys_platform_service_version(&mut ctx, &[]).unwrap();
        assert_ne!(handle, 0);
        let s = read_cstr_from_handle(handle).unwrap();
        assert_eq!(s, "1.0.0");
    }

    #[test]
    fn platform_service_mem_available_returns_positive() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let avail = sys_platform_service_mem_available(&mut ctx, &[]).unwrap();
        assert!(avail > 0);
    }

    // ── Str.fromBytes ────────────────────────────────────────

    #[test]
    fn str_from_bytes_null_returns_zero() {
        let mut mem = Vec::new();
        let mut ctx = test_ctx(&mut mem);
        let result = sys_str_from_bytes(&mut ctx, &[0, 0]).unwrap();
        assert_eq!(result, 0);
    }

    // ── Registration ─────────────────────────────────────────

    #[test]
    fn register_kit0_sys_populates_table() {
        let mut table = NativeTable::with_defaults();
        register_kit0_sys(&mut table);

        let implemented_ids: Vec<u16> = vec![
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
            16, 17, 18, 19, 20, 21, 41, 42, 43, 56, 57, 58, 59,
        ];
        for id in &implemented_ids {
            assert!(
                table.is_implemented(0, *id),
                "kit 0, method {id} should be implemented after registration"
            );
        }
    }

    #[test]
    fn register_kit0_sys_leaves_stubs_for_component_methods() {
        // Use a fresh table with only stubs (not with_defaults which registers all)
        let mut table = NativeTable::new();
        for id in 0..60u16 {
            table.register_stub(0, id);
        }
        register_kit0_sys(&mut table);

        for id in 22..=39u16 {
            assert!(
                !table.is_implemented(0, id),
                "kit 0, method {id} should remain a stub"
            );
        }
    }

    #[test]
    fn register_kit0_sys_leaves_stubs_for_filestore() {
        // Use a fresh table with only stubs (not with_defaults which registers all)
        let mut table = NativeTable::new();
        for id in 0..60u16 {
            table.register_stub(0, id);
        }
        register_kit0_sys(&mut table);

        for id in 44..=54u16 {
            assert!(
                !table.is_implemented(0, id),
                "kit 0, method {id} (FileStore) should remain a stub"
            );
        }
    }

    #[test]
    fn register_kit0_sys_implemented_count() {
        // Use a fresh table with only stubs to test sys registration in isolation
        let mut table = NativeTable::new();
        for id in 0..60u16 {
            table.register_stub(0, id);
        }
        register_kit0_sys(&mut table);
        // 29 methods: 0-21 (22) + 41-43 (3) + 56 (1) + 57-59 (3)
        assert_eq!(table.implemented_count(0), 29);
    }
}
