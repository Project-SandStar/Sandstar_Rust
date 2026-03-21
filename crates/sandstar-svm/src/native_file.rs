//! Kit 0 FileStore file I/O native methods in pure Rust.
//!
//! Replaces the C implementation in `csrc/sys_FileStore_std.c`.
//! Maps to Kit 0 method slots 44–54 in the native dispatch table.
//!
//! # Handle Model
//!
//! The C code stores `FILE*` pointers in `Cell.aval`.  Since we cannot store
//! Rust `File` objects as raw pointers safely, we use an ID-based lookup via
//! a global `Mutex<FileStore>`.  `doOpen` returns an integer handle ID; all
//! other methods receive that ID and look up the `File` in the store.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

use crate::native_table::{NativeContext, NativeTable};
use crate::vm_error::VmResult;

// ────────────────────────────────────────────────────────────────
// Global file handle store
// ────────────────────────────────────────────────────────────────

/// Internal store mapping integer handle IDs to open `File` objects.
struct FileStore {
    files: HashMap<i32, File>,
    next_id: i32,
}

impl FileStore {
    fn new() -> Self {
        Self {
            files: HashMap::new(),
            // Start at 1 so that 0 can mean "invalid/null handle".
            next_id: 1,
        }
    }

    fn insert(&mut self, file: File) -> i32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        // Skip 0 on wrap-around (extremely unlikely).
        if self.next_id == 0 {
            self.next_id = 1;
        }
        self.files.insert(id, file);
        id
    }

    fn get(&mut self, id: i32) -> Option<&mut File> {
        self.files.get_mut(&id)
    }

    fn remove(&mut self, id: i32) -> Option<File> {
        self.files.remove(&id)
    }
}

static FILE_HANDLES: Mutex<Option<FileStore>> = Mutex::new(None);

/// Ensure the global store is initialized and run `f` with a mutable ref to it.
fn with_store<R>(f: impl FnOnce(&mut FileStore) -> R) -> R {
    let mut guard = FILE_HANDLES.lock().expect("FILE_HANDLES mutex poisoned");
    let store = guard.get_or_insert_with(FileStore::new);
    f(store)
}

// ────────────────────────────────────────────────────────────────
// Helper: read a null-terminated string from VM memory
// ────────────────────────────────────────────────────────────────

/// Read a null-terminated UTF-8 string from VM memory at the given byte offset.
/// Returns `None` if the offset is out of bounds or the bytes are not valid UTF-8.
fn read_string_from_memory(memory: &[u8], offset: usize) -> Option<String> {
    if offset >= memory.len() {
        return None;
    }
    let end = memory[offset..].iter().position(|&b| b == 0)?;
    String::from_utf8(memory[offset..offset + end].to_vec()).ok()
}

// ────────────────────────────────────────────────────────────────
// Native method implementations (Kit 0, slots 44–54)
// ────────────────────────────────────────────────────────────────

/// `int FileStore.doSize(Str name)` — Kit 0 slot 44
///
/// Returns the file size in bytes, or -1 if the file does not exist or is
/// not accessible.
pub fn filestore_do_size(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let name_offset = params.first().copied().unwrap_or(0) as usize;
    let path_str = match read_string_from_memory(ctx.memory, name_offset) {
        Some(s) => s,
        None => return Ok(-1),
    };

    match fs::metadata(&path_str) {
        Ok(m) => Ok(m.len() as i32),
        Err(_) => Ok(-1),
    }
}

/// `Obj FileStore.doOpen(Str name, Str mode)` — Kit 0 slot 45
///
/// Opens a file with the given mode character:
///   - `'r'` — read only (file must exist)
///   - `'w'` — write, create, truncate
///   - `'m'` — read+write, create if missing (modify)
///
/// Returns a handle ID (> 0) on success, or 0 on failure.
pub fn filestore_do_open(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let name_offset = params.first().copied().unwrap_or(0) as usize;
    let mode_offset = params.get(1).copied().unwrap_or(0) as usize;

    let path_str = match read_string_from_memory(ctx.memory, name_offset) {
        Some(s) => s,
        None => return Ok(0),
    };
    let mode_str = match read_string_from_memory(ctx.memory, mode_offset) {
        Some(s) => s,
        None => return Ok(0),
    };

    // Mode must be a single character.
    if mode_str.len() != 1 {
        return Ok(0);
    }
    let mode_char = mode_str.as_bytes()[0];

    let path = Path::new(&path_str);

    // For write/modify modes, create parent directories if they don't exist.
    if mode_char == b'w' || mode_char == b'm' {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }
    }

    let file = match mode_char {
        b'r' => File::open(path),
        b'w' => File::create(path),
        b'm' => {
            // Mimic C behaviour: first create with append to ensure file exists,
            // then reopen with read+write.
            if !path.exists() {
                // Create the file if it does not exist.
                if File::create(path).is_err() {
                    return Ok(0);
                }
            }
            OpenOptions::new().read(true).write(true).open(path)
        }
        _ => return Ok(0),
    };

    match file {
        Ok(f) => {
            let id = with_store(|store| store.insert(f));
            Ok(id)
        }
        Err(_) => Ok(0),
    }
}

/// `int FileStore.doRead(Obj handle)` — Kit 0 slot 46
///
/// Reads a single byte from the file.  Returns the byte value (0–255) or -1
/// on EOF/error.
pub fn filestore_do_read(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let handle = params.first().copied().unwrap_or(0);
    if handle == 0 {
        return Ok(-1);
    }

    with_store(|store| {
        let file = match store.get(handle) {
            Some(f) => f,
            None => return Ok(-1),
        };
        let mut buf = [0u8; 1];
        match file.read(&mut buf) {
            Ok(0) => Ok(-1), // EOF
            Ok(_) => Ok(buf[0] as i32),
            Err(_) => Ok(-1),
        }
    })
}

/// `int FileStore.doReadBytes(Obj handle, byte[] buf, int off, int len)` — Kit 0 slot 47
///
/// Reads up to `len` bytes from the file into `ctx.memory[buf + off ..]`.
/// Returns the number of bytes actually read, or -1 on EOF/error.
pub fn filestore_do_read_bytes(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let handle = params.first().copied().unwrap_or(0);
    let buf_offset = params.get(1).copied().unwrap_or(0) as usize;
    let off = params.get(2).copied().unwrap_or(0) as usize;
    let len = params.get(3).copied().unwrap_or(0) as usize;

    if handle == 0 {
        return Ok(-1);
    }

    let start = buf_offset + off;
    let end = start + len;
    if end > ctx.memory.len() {
        return Ok(-1);
    }

    // Read into a temp buffer, then copy to memory — avoids holding mutex
    // while also holding a mutable borrow on ctx.memory.
    let mut tmp = vec![0u8; len];
    let n = with_store(|store| {
        let file = match store.get(handle) {
            Some(f) => f,
            None => return Ok(-1i32),
        };
        match file.read(&mut tmp) {
            Ok(0) if len > 0 => Ok(-1), // EOF
            Ok(n) => Ok(n as i32),
            Err(_) => Ok(-1),
        }
    })?;

    if n > 0 {
        ctx.memory[start..start + n as usize].copy_from_slice(&tmp[..n as usize]);
    }
    Ok(n)
}

/// `bool FileStore.doWrite(Obj handle, int byte)` — Kit 0 slot 48
///
/// Writes a single byte.  Returns 1 (true) on success, 0 (false) on failure.
pub fn filestore_do_write(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let handle = params.first().copied().unwrap_or(0);
    let byte_val = params.get(1).copied().unwrap_or(0) as u8;

    if handle == 0 {
        return Ok(-1);
    }

    with_store(|store| {
        let file = match store.get(handle) {
            Some(f) => f,
            None => return Ok(-1),
        };
        match file.write_all(&[byte_val]) {
            Ok(()) => Ok(1), // true
            Err(_) => Ok(0), // false
        }
    })
}

/// `bool FileStore.doWriteBytes(Obj handle, byte[] buf, int off, int len)` — Kit 0 slot 49
///
/// Writes `len` bytes from `ctx.memory[buf + off ..]` to the file.
/// Returns 1 (true) on success, 0 (false) on failure.
pub fn filestore_do_write_bytes(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let handle = params.first().copied().unwrap_or(0);
    let buf_offset = params.get(1).copied().unwrap_or(0) as usize;
    let off = params.get(2).copied().unwrap_or(0) as usize;
    let len = params.get(3).copied().unwrap_or(0) as usize;

    if handle == 0 {
        return Ok(-1);
    }

    let start = buf_offset + off;
    let end = start + len;
    if end > ctx.memory.len() {
        return Ok(0);
    }

    // Copy data out of ctx.memory before taking the mutex.
    let data = ctx.memory[start..end].to_vec();

    with_store(|store| {
        let file = match store.get(handle) {
            Some(f) => f,
            None => return Ok(-1),
        };
        match file.write_all(&data) {
            Ok(()) => Ok(1), // true
            Err(_) => Ok(0), // false
        }
    })
}

/// `int FileStore.doTell(Obj handle)` — Kit 0 slot 50
///
/// Returns the current file position, or -1 on error.
pub fn filestore_do_tell(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let handle = params.first().copied().unwrap_or(0);
    if handle == 0 {
        return Ok(-1);
    }

    with_store(|store| {
        let file = match store.get(handle) {
            Some(f) => f,
            None => return Ok(-1),
        };
        match file.stream_position() {
            Ok(pos) => Ok(pos as i32),
            Err(_) => Ok(-1),
        }
    })
}

/// `bool FileStore.doSeek(Obj handle, int pos)` — Kit 0 slot 51
///
/// Seeks to an absolute position.  Returns 1 (true) on success, 0 (false) on
/// failure.
pub fn filestore_do_seek(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let handle = params.first().copied().unwrap_or(0);
    let pos = params.get(1).copied().unwrap_or(0) as u64;

    if handle == 0 {
        return Ok(-1);
    }

    with_store(|store| {
        let file = match store.get(handle) {
            Some(f) => f,
            None => return Ok(-1),
        };
        match file.seek(SeekFrom::Start(pos)) {
            Ok(_) => Ok(1),  // true
            Err(_) => Ok(0), // false
        }
    })
}

/// `void FileStore.doFlush(Obj handle)` — Kit 0 slot 52
///
/// Flushes buffered writes.  Returns 1 on success, 0 on failure (the C
/// version returned `nullCell` which maps to 0).
pub fn filestore_do_flush(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let handle = params.first().copied().unwrap_or(0);
    if handle == 0 {
        return Ok(-1);
    }

    with_store(|store| {
        let file = match store.get(handle) {
            Some(f) => f,
            None => return Ok(-1),
        };
        match file.flush() {
            Ok(()) => Ok(1),
            Err(_) => Ok(0),
        }
    })
}

/// `bool FileStore.doClose(Obj handle)` — Kit 0 slot 53
///
/// Closes the file and removes the handle from the store.
/// Returns 1 (true) on success, 0 (false) on failure.
pub fn filestore_do_close(_ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let handle = params.first().copied().unwrap_or(0);
    if handle == 0 {
        return Ok(-1);
    }

    with_store(|store| {
        match store.remove(handle) {
            Some(_file) => {
                // File is dropped here, which closes the OS handle.
                Ok(1) // true
            }
            None => Ok(0), // false — no such handle
        }
    })
}

/// `bool FileStore.rename(Str from, Str to)` — Kit 0 slot 54
///
/// Renames a file.  If the destination already exists it is removed first
/// (matching the C implementation behaviour).
/// Returns 1 (true) on success, 0 (false) on failure.
pub fn filestore_rename(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let from_offset = params.first().copied().unwrap_or(0) as usize;
    let to_offset = params.get(1).copied().unwrap_or(0) as usize;

    let from_str = match read_string_from_memory(ctx.memory, from_offset) {
        Some(s) => s,
        None => return Ok(0),
    };
    let to_str = match read_string_from_memory(ctx.memory, to_offset) {
        Some(s) => s,
        None => return Ok(0),
    };

    // Remove destination if it exists (match C behaviour).
    let to_path = Path::new(&to_str);
    if to_path.exists() {
        if fs::remove_file(to_path).is_err() {
            return Ok(0);
        }
    }

    match fs::rename(&from_str, &to_str) {
        Ok(()) => Ok(1),
        Err(_) => Ok(0),
    }
}

// ────────────────────────────────────────────────────────────────
// Registration
// ────────────────────────────────────────────────────────────────

/// Register all Kit 0 FileStore native methods (slots 44–54) in the dispatch
/// table, replacing the stubs that were registered by `NativeTable::with_defaults()`.
pub fn register_kit0_file(table: &mut NativeTable) {
    table.register(0, 44, filestore_do_size);        // FileStore.doSize
    table.register(0, 45, filestore_do_open);        // FileStore.doOpen
    table.register(0, 46, filestore_do_read);        // FileStore.doRead
    table.register(0, 47, filestore_do_read_bytes);  // FileStore.doReadBytes
    table.register(0, 48, filestore_do_write);       // FileStore.doWrite
    table.register(0, 49, filestore_do_write_bytes); // FileStore.doWriteBytes
    table.register(0, 50, filestore_do_tell);        // FileStore.doTell
    table.register(0, 51, filestore_do_seek);        // FileStore.doSeek
    table.register(0, 52, filestore_do_flush);       // FileStore.doFlush
    table.register(0, 53, filestore_do_close);       // FileStore.doClose
    table.register(0, 54, filestore_rename);         // FileStore.rename
}

/// Reset the global file store, closing all open handles.
/// NOTE: Not called in parallel tests because it can race with other tests
/// that have open handles via the same global store.
#[cfg(test)]
#[allow(dead_code)]
fn reset_file_store() {
    let mut guard = FILE_HANDLES.lock().expect("FILE_HANDLES mutex poisoned");
    *guard = None;
}

// ────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;


    /// Build a NativeContext with a memory buffer that has a null-terminated
    /// string written at position 0.
    fn ctx_with_string(s: &str) -> Vec<u8> {
        let mut mem = s.as_bytes().to_vec();
        mem.push(0); // null terminator
        mem
    }

    /// Build a memory buffer with two null-terminated strings back to back.
    fn ctx_with_two_strings(s1: &str, s2: &str) -> Vec<u8> {
        let mut mem = s1.as_bytes().to_vec();
        mem.push(0);
        mem.extend_from_slice(s2.as_bytes());
        mem.push(0);
        mem
    }

    /// Create a temp file path with a unique name.
    fn temp_path(name: &str) -> String {
        let dir = std::env::temp_dir();
        dir.join(format!("sandstar_test_{name}"))
            .to_string_lossy()
            .to_string()
    }

    /// Clean up a temp file, ignoring errors.
    fn cleanup(path: &str) {
        let _ = fs::remove_file(path);
    }

    // ── doSize ──────────────────────────────────────────────

    #[test]
    fn do_size_existing_file() {

        let path = temp_path("size_exist");
        fs::write(&path, b"hello").unwrap();

        let mut mem = ctx_with_string(&path);
        let mut ctx = NativeContext { memory: &mut mem };
        let result = filestore_do_size(&mut ctx, &[0]).unwrap();
        assert_eq!(result, 5);

        cleanup(&path);
    }

    #[test]
    fn do_size_nonexistent() {

        let path = temp_path("size_noexist_xyz");
        cleanup(&path); // ensure it doesn't exist

        let mut mem = ctx_with_string(&path);
        let mut ctx = NativeContext { memory: &mut mem };
        let result = filestore_do_size(&mut ctx, &[0]).unwrap();
        assert_eq!(result, -1);
    }

    // ── doOpen / doClose ────────────────────────────────────

    #[test]
    fn do_open_write_creates_file() {

        let path = temp_path("open_write");
        cleanup(&path);

        let mut mem = ctx_with_two_strings(&path, "w");
        let mode_offset = (path.len() + 1) as i32;
        let mut ctx = NativeContext { memory: &mut mem };
        let handle = filestore_do_open(&mut ctx, &[0, mode_offset]).unwrap();
        assert!(handle > 0, "expected valid handle, got {handle}");

        // Close it.
        let mut ctx = NativeContext { memory: &mut mem };
        let closed = filestore_do_close(&mut ctx, &[handle]).unwrap();
        assert_eq!(closed, 1);

        assert!(Path::new(&path).exists());
        cleanup(&path);
    }

    #[test]
    fn do_open_read_existing() {

        let path = temp_path("open_read");
        fs::write(&path, b"data").unwrap();

        let mut mem = ctx_with_two_strings(&path, "r");
        let mode_offset = (path.len() + 1) as i32;
        let mut ctx = NativeContext { memory: &mut mem };
        let handle = filestore_do_open(&mut ctx, &[0, mode_offset]).unwrap();
        assert!(handle > 0);

        let mut ctx = NativeContext { memory: &mut mem };
        filestore_do_close(&mut ctx, &[handle]).unwrap();
        cleanup(&path);
    }

    #[test]
    fn do_open_read_nonexistent_returns_zero() {

        let path = temp_path("open_read_nope");
        cleanup(&path);

        let mut mem = ctx_with_two_strings(&path, "r");
        let mode_offset = (path.len() + 1) as i32;
        let mut ctx = NativeContext { memory: &mut mem };
        let handle = filestore_do_open(&mut ctx, &[0, mode_offset]).unwrap();
        assert_eq!(handle, 0);
    }

    // ── doWrite + doRead roundtrip (single byte) ───────────

    #[test]
    fn write_read_single_byte_roundtrip() {

        let path = temp_path("wr_byte");
        cleanup(&path);

        // Open for write.
        let mut mem = ctx_with_two_strings(&path, "w");
        let mode_offset = (path.len() + 1) as i32;
        let mut ctx = NativeContext { memory: &mut mem };
        let wh = filestore_do_open(&mut ctx, &[0, mode_offset]).unwrap();
        assert!(wh > 0);

        // Write byte 0x42.
        let mut ctx = NativeContext { memory: &mut mem };
        let ok = filestore_do_write(&mut ctx, &[wh, 0x42]).unwrap();
        assert_eq!(ok, 1);

        // Close.
        let mut ctx = NativeContext { memory: &mut mem };
        filestore_do_close(&mut ctx, &[wh]).unwrap();

        // Reopen for read.
        let mut mem = ctx_with_two_strings(&path, "r");
        let mode_offset = (path.len() + 1) as i32;
        let mut ctx = NativeContext { memory: &mut mem };
        let rh = filestore_do_open(&mut ctx, &[0, mode_offset]).unwrap();
        assert!(rh > 0);

        // Read one byte.
        let mut ctx = NativeContext { memory: &mut mem };
        let byte = filestore_do_read(&mut ctx, &[rh]).unwrap();
        assert_eq!(byte, 0x42);

        // Next read should be EOF.
        let mut ctx = NativeContext { memory: &mut mem };
        let eof = filestore_do_read(&mut ctx, &[rh]).unwrap();
        assert_eq!(eof, -1);

        let mut ctx = NativeContext { memory: &mut mem };
        filestore_do_close(&mut ctx, &[rh]).unwrap();
        cleanup(&path);
    }

    // ── doWriteBytes + doReadBytes roundtrip ────────────────

    #[test]
    fn write_read_bytes_roundtrip() {

        let path = temp_path("wr_bytes");
        cleanup(&path);

        // Prepare memory: [path\0mode\0 ... buffer area]
        let data = b"HELLO";
        let mut mem = ctx_with_two_strings(&path, "w");
        let mode_offset = path.len() + 1;
        // Add buffer area at the end of memory.
        let buf_start = mem.len();
        mem.extend_from_slice(data);

        // Open for write.
        let mut ctx = NativeContext { memory: &mut mem };
        let wh = filestore_do_open(&mut ctx, &[0, mode_offset as i32]).unwrap();
        assert!(wh > 0);

        // Write 5 bytes from buffer.
        let mut ctx = NativeContext { memory: &mut mem };
        let ok = filestore_do_write_bytes(
            &mut ctx,
            &[wh, buf_start as i32, 0, data.len() as i32],
        )
        .unwrap();
        assert_eq!(ok, 1);

        // Close.
        let mut ctx = NativeContext { memory: &mut mem };
        filestore_do_close(&mut ctx, &[wh]).unwrap();

        // Reopen for read — rebuild memory with read buffer area.
        let mut mem = ctx_with_two_strings(&path, "r");
        let mode_offset = path.len() + 1;
        let buf_start = mem.len();
        mem.extend_from_slice(&[0u8; 10]); // read buffer (zeroed)

        let mut ctx = NativeContext { memory: &mut mem };
        let rh = filestore_do_open(&mut ctx, &[0, mode_offset as i32]).unwrap();
        assert!(rh > 0);

        let mut ctx = NativeContext { memory: &mut mem };
        let n = filestore_do_read_bytes(
            &mut ctx,
            &[rh, buf_start as i32, 0, 5],
        )
        .unwrap();
        assert_eq!(n, 5);
        assert_eq!(&mem[buf_start..buf_start + 5], b"HELLO");

        let mut ctx = NativeContext { memory: &mut mem };
        filestore_do_close(&mut ctx, &[rh]).unwrap();
        cleanup(&path);
    }

    // ── doSeek + doTell roundtrip ───────────────────────────

    #[test]
    fn seek_tell_roundtrip() {

        let path = temp_path("seek_tell");
        fs::write(&path, b"abcdefghij").unwrap();

        let mut mem = ctx_with_two_strings(&path, "r");
        let mode_offset = (path.len() + 1) as i32;
        let mut ctx = NativeContext { memory: &mut mem };
        let h = filestore_do_open(&mut ctx, &[0, mode_offset]).unwrap();
        assert!(h > 0);

        // Tell at start = 0.
        let mut ctx = NativeContext { memory: &mut mem };
        assert_eq!(filestore_do_tell(&mut ctx, &[h]).unwrap(), 0);

        // Seek to 5.
        let mut ctx = NativeContext { memory: &mut mem };
        assert_eq!(filestore_do_seek(&mut ctx, &[h, 5]).unwrap(), 1);

        // Tell = 5.
        let mut ctx = NativeContext { memory: &mut mem };
        assert_eq!(filestore_do_tell(&mut ctx, &[h]).unwrap(), 5);

        // Read byte at position 5 → 'f' = 0x66.
        let mut ctx = NativeContext { memory: &mut mem };
        assert_eq!(filestore_do_read(&mut ctx, &[h]).unwrap(), b'f' as i32);

        let mut ctx = NativeContext { memory: &mut mem };
        filestore_do_close(&mut ctx, &[h]).unwrap();
        cleanup(&path);
    }

    // ── doFlush ─────────────────────────────────────────────

    #[test]
    fn flush_does_not_error() {

        let path = temp_path("flush");
        cleanup(&path);

        let mut mem = ctx_with_two_strings(&path, "w");
        let mode_offset = (path.len() + 1) as i32;
        let mut ctx = NativeContext { memory: &mut mem };
        let h = filestore_do_open(&mut ctx, &[0, mode_offset]).unwrap();
        assert!(h > 0);

        let mut ctx = NativeContext { memory: &mut mem };
        let result = filestore_do_flush(&mut ctx, &[h]).unwrap();
        assert_eq!(result, 1);

        let mut ctx = NativeContext { memory: &mut mem };
        filestore_do_close(&mut ctx, &[h]).unwrap();
        cleanup(&path);
    }

    // ── doClose invalid handle ──────────────────────────────

    #[test]
    fn close_invalid_handle_returns_zero() {

        let mut mem = vec![0u8; 16];
        let mut ctx = NativeContext { memory: &mut mem };
        let result = filestore_do_close(&mut ctx, &[9999]).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn close_zero_handle_returns_neg_one() {

        let mut mem = vec![0u8; 16];
        let mut ctx = NativeContext { memory: &mut mem };
        let result = filestore_do_close(&mut ctx, &[0]).unwrap();
        assert_eq!(result, -1);
    }

    // ── rename ──────────────────────────────────────────────

    #[test]
    fn rename_success() {

        let from = temp_path("rename_from");
        let to = temp_path("rename_to");
        cleanup(&from);
        cleanup(&to);

        fs::write(&from, b"content").unwrap();

        let mut mem = ctx_with_two_strings(&from, &to);
        let to_offset = (from.len() + 1) as i32;
        let mut ctx = NativeContext { memory: &mut mem };
        let result = filestore_rename(&mut ctx, &[0, to_offset]).unwrap();
        assert_eq!(result, 1);

        assert!(!Path::new(&from).exists());
        assert!(Path::new(&to).exists());
        assert_eq!(fs::read_to_string(&to).unwrap(), "content");

        cleanup(&to);
    }

    #[test]
    fn rename_nonexistent_source_returns_zero() {

        let from = temp_path("rename_nosrc");
        let to = temp_path("rename_nodst");
        cleanup(&from);
        cleanup(&to);

        let mut mem = ctx_with_two_strings(&from, &to);
        let to_offset = (from.len() + 1) as i32;
        let mut ctx = NativeContext { memory: &mut mem };
        let result = filestore_rename(&mut ctx, &[0, to_offset]).unwrap();
        assert_eq!(result, 0);
    }

    // ── mode 'm' (modify) ──────────────────────────────────

    #[test]
    fn mode_m_creates_file_if_missing() {

        let path = temp_path("mode_m_create");
        cleanup(&path);

        let mut mem = ctx_with_two_strings(&path, "m");
        let mode_offset = (path.len() + 1) as i32;
        let mut ctx = NativeContext { memory: &mut mem };
        let h = filestore_do_open(&mut ctx, &[0, mode_offset]).unwrap();
        assert!(h > 0, "mode 'm' should create the file");
        assert!(Path::new(&path).exists());

        let mut ctx = NativeContext { memory: &mut mem };
        filestore_do_close(&mut ctx, &[h]).unwrap();
        cleanup(&path);
    }

    #[test]
    fn mode_m_opens_existing_for_rw() {

        let path = temp_path("mode_m_rw");
        fs::write(&path, b"ABCD").unwrap();

        let mut mem = ctx_with_two_strings(&path, "m");
        let mode_offset = (path.len() + 1) as i32;
        let mut ctx = NativeContext { memory: &mut mem };
        let h = filestore_do_open(&mut ctx, &[0, mode_offset]).unwrap();
        assert!(h > 0);

        // Read first byte → 'A'.
        let mut ctx = NativeContext { memory: &mut mem };
        assert_eq!(filestore_do_read(&mut ctx, &[h]).unwrap(), b'A' as i32);

        // Write a byte at current position (overwrite 'B' with 'X').
        let mut ctx = NativeContext { memory: &mut mem };
        assert_eq!(filestore_do_write(&mut ctx, &[h, b'X' as i32]).unwrap(), 1);

        let mut ctx = NativeContext { memory: &mut mem };
        filestore_do_close(&mut ctx, &[h]).unwrap();

        // Verify file content.
        let content = fs::read(&path).unwrap();
        assert_eq!(&content, b"AXCD");

        cleanup(&path);
    }

    // ── multiple files open simultaneously ──────────────────

    #[test]
    fn multiple_files_open() {

        let p1 = temp_path("multi1");
        let p2 = temp_path("multi2");
        cleanup(&p1);
        cleanup(&p2);

        // Open file 1 for write.
        let mut mem1 = ctx_with_two_strings(&p1, "w");
        let mo1 = (p1.len() + 1) as i32;
        let mut ctx = NativeContext { memory: &mut mem1 };
        let h1 = filestore_do_open(&mut ctx, &[0, mo1]).unwrap();
        assert!(h1 > 0);

        // Open file 2 for write.
        let mut mem2 = ctx_with_two_strings(&p2, "w");
        let mo2 = (p2.len() + 1) as i32;
        let mut ctx = NativeContext { memory: &mut mem2 };
        let h2 = filestore_do_open(&mut ctx, &[0, mo2]).unwrap();
        assert!(h2 > 0);

        // Handles should be distinct.
        assert_ne!(h1, h2);

        // Write to both.
        let mut ctx = NativeContext { memory: &mut mem1 };
        filestore_do_write(&mut ctx, &[h1, b'1' as i32]).unwrap();
        let mut ctx = NativeContext { memory: &mut mem2 };
        filestore_do_write(&mut ctx, &[h2, b'2' as i32]).unwrap();

        // Flush both to ensure data is on disk before close.
        let mut ctx = NativeContext { memory: &mut mem1 };
        filestore_do_flush(&mut ctx, &[h1]).unwrap();
        let mut ctx = NativeContext { memory: &mut mem2 };
        filestore_do_flush(&mut ctx, &[h2]).unwrap();

        // Close both.
        let mut ctx = NativeContext { memory: &mut mem1 };
        filestore_do_close(&mut ctx, &[h1]).unwrap();
        let mut ctx = NativeContext { memory: &mut mem2 };
        filestore_do_close(&mut ctx, &[h2]).unwrap();

        // Verify.
        assert_eq!(fs::read(&p1).unwrap(), b"1");
        assert_eq!(fs::read(&p2).unwrap(), b"2");

        cleanup(&p1);
        cleanup(&p2);
    }

    // ── read_string_from_memory edge cases ──────────────────

    #[test]
    fn read_string_empty() {
        let mem = vec![0u8]; // just a null terminator
        assert_eq!(read_string_from_memory(&mem, 0), Some(String::new()));
    }

    #[test]
    fn read_string_out_of_bounds() {
        let mem = vec![0u8; 4];
        assert_eq!(read_string_from_memory(&mem, 100), None);
    }

    // ── registration ────────────────────────────────────────

    #[test]
    fn register_replaces_stubs() {
        let mut table = NativeTable::with_defaults();

        // Before registration, slot 44 is a Stub.
        let entry = table.lookup(0, 44).unwrap();
        assert!(matches!(entry, crate::native_table::NativeEntry::Stub));

        // Register file methods.
        register_kit0_file(&mut table);

        // After registration, slot 44 should be Normal.
        let entry = table.lookup(0, 44).unwrap();
        assert!(
            matches!(entry, crate::native_table::NativeEntry::Normal(_)),
            "slot 44 should be Normal after registration"
        );
    }

    // ── doRead on invalid handle ────────────────────────────

    #[test]
    fn read_invalid_handle_returns_neg_one() {

        let mut mem = vec![0u8; 16];
        let mut ctx = NativeContext { memory: &mut mem };
        assert_eq!(filestore_do_read(&mut ctx, &[9999]).unwrap(), -1);
    }

    // ── auto-create directories on write ────────────────────

    #[test]
    fn open_write_creates_parent_dirs() {

        let dir = temp_path("subdir_test");
        let _ = fs::remove_dir_all(&dir);
        let file_path = format!("{dir}/nested/file.txt");

        let mut mem = ctx_with_two_strings(&file_path, "w");
        let mode_offset = (file_path.len() + 1) as i32;
        let mut ctx = NativeContext { memory: &mut mem };
        let h = filestore_do_open(&mut ctx, &[0, mode_offset]).unwrap();
        assert!(h > 0);

        let mut ctx = NativeContext { memory: &mut mem };
        filestore_do_close(&mut ctx, &[h]).unwrap();

        assert!(Path::new(&file_path).exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
