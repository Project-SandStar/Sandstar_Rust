# 06: Sedona VM FFI Strategy — C-to-Rust Foreign Function Interface

## Overview

The Sedona VM is a ~100K line C bytecode interpreter (`vm.c`: 49,670 lines, `scode.h`: 28,575 lines, `sedona.h`: 12,179 lines). It will **not** be rewritten. The VM stays as C and calls into Rust through a C-compatible FFI boundary.

The current FFI boundary lives in `shaystack.cpp` (740 lines), where the Sedona VM calls `extern "C"` functions that delegate to C++ haystack client code. After migration, these same `extern "C"` entry points will call Rust instead of C++.

**Source files analyzed:**
- `shaystack/sandstar/sandstar/EacIo/src/shaystack/native/shaystack.cpp` (740 lines)
- `shaystack/sandstar/sandstar/EacIo/src/vm/sedona.h` (12,179 lines) -- Cell, SedonaVM types
- `shaystack/sandstar/sandstar/EacIo/src/shaystack/native/haystack/http_client/include/client.hpp` (142 lines)
- `shaystack/sandstar/sandstar/EacIo/src/shaystack/native/haystack/auth/clientcontext.hpp` (103 lines)
- `shaystack/sandstar/sandstar/EacIo/src/shaystack/native/haystack/auth/scramscheme.hpp` (72 lines)
- `shaystack/sandstar/sandstar/EacIo/src/shaystack/native/haystack/auth/basicscheme.hpp` (19 lines)

---

## 1. How Rust Exposes extern "C" Functions

### The Core Mechanism

Rust can export functions with C-compatible ABI using two attributes:

```rust
#[no_mangle]          // Prevents Rust's name mangling (symbol = exact function name)
pub extern "C" fn     // Uses C calling convention (cdecl on x86, AAPCS on ARM7)
```

### Why Both Attributes Are Required

`#[no_mangle]` prevents the Rust compiler from turning `create_client` into something like `_ZN9sandstar14create_client17h8a3b5c7d9e1f2g3hE`. Without it, the C linker cannot find the symbol.

`extern "C"` changes the calling convention. Rust's default calling convention is unspecified and can change between compiler versions. The C calling convention is stable, documented, and what the Sedona VM expects.

### Practical Example

```rust
// In src/ffi/haystack_device.rs

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_float, c_void};

/// Opaque handle returned to C. C code sees this as void*.
pub struct HaystackClient {
    inner: crate::client::Client,
}

#[no_mangle]
pub extern "C" fn shaystack_HaystackDevice_create_client(
    vm: *mut c_void,       // SedonaVM* -- opaque, Rust doesn't inspect it
    params: *mut CellFfi,  // Cell* params array
) -> CellFfi {
    // Safety: caller guarantees params[0..2] are valid
    let params = unsafe { std::slice::from_raw_parts(params, 3) };

    let uri = match unsafe { ptr_to_str(params[0].aval) } {
        Some(s) => s,
        None => return CELL_NULL,
    };
    let username = match unsafe { ptr_to_str(params[1].aval) } {
        Some(s) => s,
        None => return CELL_NULL,
    };
    let password = match unsafe { ptr_to_str(params[2].aval) } {
        Some(s) => s,
        None => return CELL_NULL,
    };

    match crate::client::Client::new(uri, username, password) {
        Ok(client) => {
            let boxed = Box::new(HaystackClient { inner: client });
            let ptr = Box::into_raw(boxed);
            CellFfi { aval: ptr as *mut c_void }
        }
        Err(e) => {
            eprintln!("shaystack: ERROR: create_client: {}", e);
            CELL_NULL
        }
    }
}
```

### Function Naming Convention

The current C++ functions follow a precise naming scheme that the Sedona native method table uses for lookup:

```
shaystack_HaystackDevice_<method_name>
```

The Rust replacements **must** use identical names. The Sedona VM resolves native methods by name at startup -- any mismatch causes a runtime crash.

---

## 2. How cbindgen Generates C Headers from Rust Code

### What cbindgen Does

[cbindgen](https://github.com/mozilla/cbindgen) reads Rust source code and generates C/C++ header files that declare the `extern "C"` functions. This replaces the manual header maintenance that the current C++ approach requires.

### Setup

Add to `Cargo.toml`:

```toml
[package.metadata.capi.header]
name = "sandstar_haystack"
generation = true

[build-dependencies]
cbindgen = "0.27"
```

Create `build.rs`:

```rust
fn main() {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();

    let config = cbindgen::Config {
        language: cbindgen::Language::C,
        include_guard: Some("SANDSTAR_HAYSTACK_H".to_string()),
        no_includes: true,
        includes: vec!["sedona.h".to_string()],
        ..Default::default()
    };

    cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_config(config)
        .generate()
        .expect("Unable to generate C bindings")
        .write_to_file("include/sandstar_haystack.h");
}
```

### Generated Output Example

Running `cargo build` produces `include/sandstar_haystack.h`:

```c
#ifndef SANDSTAR_HAYSTACK_H
#define SANDSTAR_HAYSTACK_H

#include "sedona.h"

/* Opaque handle to Rust haystack client */
typedef struct HaystackClient HaystackClient;

Cell shaystack_HaystackDevice_create_client(SedonaVM *vm, Cell *params);
Cell shaystack_HaystackDevice_delete_client(SedonaVM *vm, Cell *params);
Cell shaystack_HaystackDevice_is_authenticated(SedonaVM *vm, Cell *params);
Cell shaystack_HaystackDevice_read_message(SedonaVM *vm, Cell *params);
Cell shaystack_HaystackDevice_eval_message(SedonaVM *vm, Cell *params);
Cell shaystack_HaystackDevice_watch_sub_message(SedonaVM *vm, Cell *params);
Cell shaystack_HaystackDevice_watch_unsub_message(SedonaVM *vm, Cell *params);
Cell shaystack_HaystackDevice_write_float_point_message(SedonaVM *vm, Cell *params);
Cell shaystack_HaystackDevice_write_bool_point_message(SedonaVM *vm, Cell *params);
/* ... */

#endif /* SANDSTAR_HAYSTACK_H */
```

### cbindgen Configuration for This Project

Key configuration decisions:

```toml
# cbindgen.toml
language = "C"
include_guard = "SANDSTAR_HAYSTACK_H"
autogen_warning = "/* Auto-generated by cbindgen. Do not edit. */"
includes = ["sedona.h"]

[export]
# Only export functions starting with shaystack_
include = ["shaystack_.*"]

[export.rename]
# Map Rust types to existing C types
"CellFfi" = "Cell"
```

**Important:** cbindgen cannot see into `sedona.h` -- it does not parse C headers. The `Cell` type and `SedonaVM` struct must either be:
1. Represented as opaque pointers (`*mut c_void`) in Rust, or
2. Re-declared as `#[repr(C)]` Rust structs that cbindgen can emit

For this project, option 1 is strongly recommended. The Sedona VM types are complex and we only pass pointers through.

---

## 3. Data Type Mapping Across the FFI Boundary

### The Cell Union

The most critical type crossing the boundary is `Cell`, the Sedona VM's universal value type:

```c
// From sedona.h:288-294
typedef union {
    int32_t ival;    // 32-bit signed int
    float   fval;    // 32-bit float
    void*   aval;    // address pointer
} Cell;
```

Rust equivalent:

```rust
/// FFI-compatible Cell union matching sedona.h
/// CRITICAL: Must be #[repr(C)] to match C memory layout
#[repr(C)]
#[derive(Copy, Clone)]
pub union CellFfi {
    pub ival: i32,
    pub fval: f32,
    pub aval: *mut c_void,
}

/// Cell constants matching sedona.h:297-302
pub const CELL_ZERO: CellFfi = CellFfi { ival: 0 };
pub const CELL_NULL: CellFfi = CellFfi { ival: 0 };   // nullCell = zeroCell
pub const CELL_FALSE: CellFfi = CellFfi { ival: 0 };   // falseCell = zeroCell
pub const CELL_TRUE: CellFfi = CellFfi { ival: 1 };    // trueCell = oneCell
```

### Pointer Size Warning (ARM7)

On the BeagleBone (ARM7, 32-bit), `void*` is 4 bytes. On a 64-bit development host, `void*` is 8 bytes. The `Cell` union is therefore:
- **ARM7 (target):** 4 bytes (all variants are 4 bytes)
- **x86_64 (dev host):** 8 bytes (`void*` is 8 bytes, dominates)

This means **testing on 64-bit host with the same binary is not safe** unless the Cell union is conditionally padded. The cross-compilation toolchain ensures correct sizes for the target.

### Complete Type Mapping Table

| C Type | Sedona Usage | Rust Type | Notes |
|--------|-------------|-----------|-------|
| `Cell` (union) | Return values, parameters | `CellFfi` (#[repr(C)] union) | Must match exact layout |
| `SedonaVM*` | First param of all FFI fns | `*mut c_void` | Opaque -- Rust never dereferences |
| `Cell*` | params array | `*mut CellFfi` | Rust reads via `slice::from_raw_parts` |
| `int32_t` (via Cell.ival) | Boolean, integer results | `i32` | `0` = false/null, `1` = true |
| `float` (via Cell.fval) | Sensor readings | `f32` | Single precision -- Sedona limitation |
| `void*` (via Cell.aval) | Client handle, string ptr | `*mut c_void` | Cast to/from `Box<T>` or `CStr` |
| `const char*` | String parameters | `*const c_char` | Read via `CStr::from_ptr()` |
| `char*` | Output buffer | `*mut c_char` | Write via `std::ptr::copy_nonoverlapping` |
| `int` | Buffer length | `c_int` (i32) | Used as buf_len parameter |
| `haystack::Client*` | Opaque client handle | `*mut HaystackClient` | Stored in Cell.aval |

### String Handling Across the Boundary

The current C++ code passes strings two ways:

**Input strings (C to Rust):** C passes `const char*` through `Cell.aval`. Rust reads them:

```rust
/// Safely extract a &str from a Cell's void pointer
/// Returns None if the pointer is null or the string is not valid UTF-8
unsafe fn ptr_to_str<'a>(ptr: *mut c_void) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    let c_str = CStr::from_ptr(ptr as *const c_char);
    c_str.to_str().ok()
}
```

**Output strings (Rust to C):** C passes a `char* buf` and `int buf_len`. Rust writes into the buffer:

```rust
/// Write a Rust string into a C buffer
/// Returns true if successful, false if buffer too small
unsafe fn write_to_c_buffer(s: &str, buf: *mut c_char, buf_len: c_int) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() >= buf_len as usize {
        eprintln!("shaystack: ERROR: buffer too small ({} >= {})", bytes.len(), buf_len);
        return false;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, bytes.len());
    *buf.add(bytes.len()) = 0; // null terminator
    true
}
```

**Warning:** The current C++ code has a buffer overflow vulnerability in several places:

```cpp
// shaystack.cpp:130-131 -- read_message
const std::string message = client->get_read_all_message(filter, 1);
message.copy(buf, buf_len);        // No length check!
buf[message.length()] = '\0';      // Writes past buf_len if message > buf_len
```

The Rust implementation must fix this by checking `bytes.len() < buf_len` before writing.

---

## 4. Memory Management: Who Allocates, Who Frees

### The Ownership Contract

The golden rule of FFI memory: **whoever allocates must free**. The Sedona VM allocates its own stack and Cell arrays. Rust allocates its own heap objects. They must never `free()` each other's memory.

### Pattern 1: Rust-Owned Client Handle (Box::into_raw / Box::from_raw)

**Allocation (create_client):**

```rust
#[no_mangle]
pub extern "C" fn shaystack_HaystackDevice_create_client(
    vm: *mut c_void,
    params: *mut CellFfi,
) -> CellFfi {
    // ... parse params ...

    match Client::new(uri, username, password) {
        Ok(client) => {
            let handle = Box::new(HaystackClient { inner: client });

            // Box::into_raw transfers ownership to C
            // The pointer is valid until Box::from_raw reclaims it
            let raw_ptr = Box::into_raw(handle);

            CellFfi { aval: raw_ptr as *mut c_void }
        }
        Err(e) => {
            eprintln!("shaystack: ERROR: create_client: {}", e);
            CELL_NULL
        }
    }
}
```

**Deallocation (delete_client):**

```rust
#[no_mangle]
pub extern "C" fn shaystack_HaystackDevice_delete_client(
    vm: *mut c_void,
    params: *mut CellFfi,
) -> CellFfi {
    let params = unsafe { std::slice::from_raw_parts(params, 1) };
    let ptr = unsafe { params[0].aval };

    if ptr.is_null() {
        return CELL_NULL; // Silently ignore null deletion (matches C++ behavior)
    }

    // SAFETY: ptr was created by Box::into_raw in create_client
    // After this, the pointer is invalid -- C must not use it
    unsafe {
        let _ = Box::from_raw(ptr as *mut HaystackClient);
        // Box is dropped here, calling HaystackClient's destructor
    }

    CELL_NULL
}
```

**Lifecycle diagram:**

```
Sedona VM                          Rust
─────────                          ────
call create_client(uri,user,pass)
    ──────────────────────────────►  Box::new(Client)
                                     Box::into_raw(handle) → ptr
    ◄──────────────────────────────  return Cell { aval: ptr }
store ptr in Cell.aval

... use ptr in read, eval, etc ...

call delete_client(ptr)
    ──────────────────────────────►  Box::from_raw(ptr)
                                     drop(client)  // destructor runs
    ◄──────────────────────────────  return nullCell
set stored ptr = NULL
```

### Pattern 2: Borrowing the Client Handle (Mid-Call)

Functions like `read_message` receive the client handle but do NOT take ownership:

```rust
#[no_mangle]
pub extern "C" fn shaystack_HaystackDevice_read_message(
    vm: *mut c_void,
    params: *mut CellFfi,
) -> CellFfi {
    let params = unsafe { std::slice::from_raw_parts(params, 4) };

    let client_ptr = unsafe { params[0].aval };
    if client_ptr.is_null() {
        return CELL_FALSE;
    }

    // SAFETY: Borrow (not own) the client for the duration of this call.
    // We use &* to get a reference without taking ownership.
    let client: &HaystackClient = unsafe { &*(client_ptr as *const HaystackClient) };

    // ... use client.inner to build message ...
    // client is NOT dropped here -- the borrow expires, that's all

    CELL_TRUE
}
```

**Critical:** Never call `Box::from_raw` in a borrow context. That would free the client while the Sedona VM still holds the pointer.

### Pattern 3: C-Owned Buffers (String Output)

The Sedona VM allocates output buffers on its stack. Rust writes into them:

```rust
// C owns this buffer -- Rust writes into it but does not free it
let buf = unsafe { params[2].aval } as *mut c_char;
let buf_len = unsafe { params[3].ival } as usize;

// Write message into C's buffer
let message = client.inner.get_read_all_message(filter, 1);
unsafe {
    write_to_c_buffer(&message, buf, buf_len as c_int);
}
// buf still belongs to C -- Rust does not free it
```

### Memory Safety Checklist

| Operation | C++ Current | Rust Replacement | Who Owns Memory? |
|-----------|-------------|-----------------|------------------|
| Create client | `new haystack::Client(...)` → `Cell.aval` | `Box::into_raw(Box::new(...))` → `Cell.aval` | Rust (via Box) |
| Delete client | `delete client` | `Box::from_raw(ptr)` → drop | Rust (reclaims) |
| Use client | `static_cast<Client*>(aval)` | `&*(ptr as *const HaystackClient)` | Rust (borrowed) |
| Read string param | `static_cast<const char*>(aval)` | `CStr::from_ptr(aval)` | C (owns buffer) |
| Write string result | `message.copy(buf, buf_len)` | `ptr::copy_nonoverlapping(...)` | C (owns buffer) |
| Parse response | `new haystack::Grid()` via `parse_response` | Local Rust Grid, dropped at end | Rust (temporary) |

---

## 5. Error Handling Across FFI

### The Problem

Rust's `Result<T, E>` and `?` operator cannot cross the FFI boundary. C has no concept of Rust's panic unwinding. A panic in Rust that unwinds across an `extern "C"` boundary is **undefined behavior** -- it will likely crash the Sedona VM.

### Strategy 1: Return Codes via Cell (Current Pattern)

The existing C++ code returns `trueCell`/`falseCell` for success/failure. The Rust code matches this:

```rust
#[no_mangle]
pub extern "C" fn shaystack_HaystackDevice_read_message(
    vm: *mut c_void,
    params: *mut CellFfi,
) -> CellFfi {
    // catch_unwind prevents panics from crossing FFI boundary
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read_message_impl(params)
    }));

    match result {
        Ok(Ok(cell)) => cell,                     // Success
        Ok(Err(e)) => {                           // Application error
            eprintln!("shaystack: ERROR: read_message: {}", e);
            CELL_FALSE
        }
        Err(_panic) => {                          // Panic caught
            eprintln!("shaystack: PANIC: read_message: internal error");
            CELL_FALSE
        }
    }
}

/// Internal implementation that can use ? and Result
fn read_message_impl(params: *mut CellFfi) -> Result<CellFfi, Box<dyn std::error::Error>> {
    let params = unsafe { std::slice::from_raw_parts(params, 4) };

    let client_ptr = unsafe { params[0].aval };
    if client_ptr.is_null() {
        return Err("null client pointer".into());
    }

    let client = unsafe { &*(client_ptr as *const HaystackClient) };
    let filter = unsafe { ptr_to_str(params[1].aval) }
        .ok_or("null or invalid filter string")?;
    let buf = unsafe { params[2].aval } as *mut c_char;
    let buf_len = unsafe { params[3].ival };

    if buf.is_null() {
        return Err("null output buffer".into());
    }

    let message = client.inner.get_read_all_message(filter, 1)?;

    unsafe {
        if write_to_c_buffer(&message, buf, buf_len) {
            Ok(CELL_TRUE)
        } else {
            Err("buffer too small".into())
        }
    }
}
```

### Strategy 2: catch_unwind Wrapper Macro

To avoid repeating the panic-catching boilerplate for every FFI function:

```rust
/// Macro that wraps an FFI function with panic catching
macro_rules! ffi_fn {
    ($name:ident, $params_count:expr, $body:expr) => {
        #[no_mangle]
        pub extern "C" fn $name(
            vm: *mut c_void,
            params: *mut CellFfi,
        ) -> CellFfi {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let params = unsafe {
                    std::slice::from_raw_parts(params, $params_count)
                };
                $body(params)
            }));

            match result {
                Ok(cell) => cell,
                Err(_) => {
                    eprintln!("shaystack: PANIC in {}", stringify!($name));
                    CELL_FALSE
                }
            }
        }
    };
}

// Usage:
ffi_fn!(shaystack_HaystackDevice_is_authenticated, 1, |params: &[CellFfi]| {
    let ptr = unsafe { params[0].aval };
    if ptr.is_null() {
        return CELL_FALSE;
    }
    let client = unsafe { &*(ptr as *const HaystackClient) };
    if client.inner.is_authenticated() { CELL_TRUE } else { CELL_FALSE }
});
```

### Error Return Value Convention

| Current C++ Return | Meaning | Rust Equivalent |
|-------------------|---------|-----------------|
| `trueCell` | Operation succeeded | `CELL_TRUE` |
| `falseCell` | Operation failed | `CELL_FALSE` |
| `nullCell` | Null/not found/error | `CELL_NULL` (same as CELL_FALSE) |
| `Cell { fval: ... }` | Float value | `CellFfi { fval: value }` |
| `Cell { ival: ... }` | Integer/bool value | `CellFfi { ival: value }` |
| `Cell { aval: ptr }` | Pointer (client handle) | `CellFfi { aval: ptr }` |

---

## 6. Thread Safety: Sedona VM Thread vs Rust Tokio Runtime

### Current Threading Model

The system runs with multiple threads:

```
┌─────────────────────────────────────────────────────┐
│ Process: svm (Sedona Virtual Machine)               │
│                                                     │
│  Thread 1: Sedona VM main loop                      │
│    └─ vm.c: vmRun() -- bytecode interpreter         │
│    └─ Calls shaystack_* FFI functions               │
│    └─ Single-threaded: one call at a time           │
│                                                     │
│  Thread 2: engineio message receiver                │
│    └─ engineio.c: engineio_main()                   │
│    └─ Blocks on msgrcv() for engine messages        │
│                                                     │
│  Thread 3: engineio flush thread                    │
│    └─ engineio.c: message_flush_thread()            │
│    └─ Flushes buffered IPC messages every 10ms      │
│                                                     │
│  (Future) Thread N: Tokio runtime threads           │
│    └─ Rust async tasks for HTTP, Haystack server    │
└─────────────────────────────────────────────────────┘
```

### The Critical Safety Question

The Sedona VM (Thread 1) calls FFI functions **synchronously** -- it blocks until the function returns. The Rust haystack implementation will use `tokio` for async HTTP. How do they interact?

### Solution: Dedicated Tokio Runtime in Rust

```rust
use once_cell::sync::Lazy;
use tokio::runtime::Runtime;

/// Global tokio runtime. Created once, lives for the process lifetime.
/// The Sedona VM's thread uses block_on() to run async code synchronously.
static RUNTIME: Lazy<Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)          // Small: BeagleBone has 1 core
        .enable_all()
        .thread_name("sandstar-rt")
        .build()
        .expect("Failed to create tokio runtime")
});

/// Called from FFI: runs an async operation synchronously on the tokio runtime
fn block_on_async<F: std::future::Future>(f: F) -> F::Output {
    RUNTIME.block_on(f)
}
```

### How FFI Functions Use Async

```rust
#[no_mangle]
pub extern "C" fn shaystack_HaystackDevice_read_message(
    vm: *mut c_void,
    params: *mut CellFfi,
) -> CellFfi {
    // ... extract params ...

    // This blocks the Sedona VM thread until the async HTTP call completes
    let result = block_on_async(async {
        client.inner.read_all(filter, 1).await
    });

    match result {
        Ok(message) => {
            unsafe { write_to_c_buffer(&message, buf, buf_len) };
            CELL_TRUE
        }
        Err(e) => {
            eprintln!("shaystack: ERROR: read_message: {}", e);
            CELL_FALSE
        }
    }
}
```

### Thread Safety for the Client Handle

The `HaystackClient` handle is created on Thread 1 (Sedona VM) and only ever used from Thread 1. This is safe because Sedona is single-threaded. However, the async code inside tokio may spawn tasks on worker threads. The client's internal state must be `Send + Sync`:

```rust
pub struct HaystackClient {
    // inner must be Send + Sync if used across tokio tasks
    inner: Arc<Client>,
}

// If Client contains !Send types (like raw pointers from POCO),
// use a Mutex to protect them:
pub struct HaystackClient {
    inner: Arc<Mutex<Client>>,
}
```

### Thread Safety Matrix

| Component | Thread | Rust Safety Requirement |
|-----------|--------|----------------------|
| SedonaVM* pointer | Thread 1 only | None -- opaque, never dereferenced |
| Cell* params | Thread 1 only | None -- used synchronously then forgotten |
| HaystackClient handle | Created Thread 1, used Thread 1 | `Send` (for tokio spawn) |
| HTTP connections | Tokio worker threads | `Send + Sync` (via Arc<Mutex<_>>) |
| Output buffer (char*) | Thread 1 stack | None -- written before return |

---

## 7. libhaystack's c-api Feature

### What Is libhaystack?

[libhaystack](https://crates.io/crates/libhaystack) is a Rust crate implementing Project Haystack data types, Zinc format parsing/encoding, and filter expressions. It already has a `c-api` feature that exposes C-compatible functions via cbindgen.

### Enabling the c-api Feature

```toml
# Cargo.toml
[dependencies]
libhaystack = { version = "1", features = ["c-api"] }
```

### What the c-api Feature Provides

The c-api feature exposes:

1. **Grid operations** -- Create, read, write Haystack grids as C-compatible structs
2. **Zinc parsing** -- Parse Zinc format strings into Grid objects
3. **Zinc encoding** -- Serialize Grid objects to Zinc format strings
4. **Filter parsing** -- Parse Haystack filter expressions
5. **Value types** -- Num, Bool, Str, Ref, DateTime as C-compatible types

### How This Replaces the C++ Haystack Library

Current C++ dependency chain:

```
shaystack.cpp
  └── haystack/grid.hpp           → libhaystack Grid
  └── haystack/zincreader.hpp     → libhaystack ZincReader
  └── haystack/zincwriter.hpp     → libhaystack ZincWriter
  └── haystack/num.hpp            → libhaystack Num
  └── haystack/bool.hpp           → libhaystack Bool
  └── http_client/client.hpp      → Rust HTTP client (hyper/reqwest)
  └── haystack/auth/*             → Rust auth implementation
```

### Using libhaystack in FFI Functions

```rust
use libhaystack::val::*;
use libhaystack::grid::Grid;
use libhaystack::zinc::decode::from_str as zinc_decode;
use libhaystack::zinc::encode::to_zinc_string;

/// Parse an HTTP response body into a Haystack Grid
fn parse_zinc_response(response: &str) -> Result<Grid, String> {
    // Extract body from HTTP response (skip headers)
    let body = extract_body(response)?;
    zinc_decode(&body).map_err(|e| format!("Zinc parse error: {}", e))
}

/// FFI wrapper for has_float
#[no_mangle]
pub extern "C" fn shaystack_HaystackDevice_has_float(
    vm: *mut c_void,
    params: *mut CellFfi,
) -> CellFfi {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let params = unsafe { std::slice::from_raw_parts(params, 3) };
        let response = unsafe { ptr_to_str(params[0].aval) }.unwrap_or("");
        let rownum = unsafe { params[1].ival } as usize;
        let col_name = unsafe { ptr_to_str(params[2].aval) }.unwrap_or("");

        match parse_zinc_response(response) {
            Ok(grid) => {
                if grid.is_empty() || grid.is_err() || grid.rows().len() <= rownum {
                    return CELL_FALSE;
                }
                match grid.rows()[rownum].get(col_name) {
                    Some(Value::Num(_)) => CELL_TRUE,
                    _ => CELL_FALSE,
                }
            }
            Err(_) => CELL_FALSE,
        }
    }));

    result.unwrap_or(CELL_FALSE)
}
```

### cbindgen Compatibility

libhaystack's c-api feature is designed for cbindgen. Its exported types use `#[repr(C)]` and its functions use `#[no_mangle] pub extern "C"`. This means the sandstar Rust crate can re-export libhaystack's C types in its own generated header, creating a single unified header for the Sedona VM.

---

## 8. Build Integration: Linking Rust Static Library into C/Sedona Binary

### Build Strategy

The Rust code compiles to a static library (`.a` file). The existing CMake build system links it into the SVM binary alongside the Sedona VM C code.

### Step 1: Cargo Configuration for Static Library

```toml
# sandstar_rust/Cargo.toml
[lib]
name = "sandstar_haystack"
crate-type = ["staticlib"]  # Produces libsandstar_haystack.a
```

### Step 2: Cross-Compilation for ARM7

```bash
# Add ARM target
rustup target add armv7-unknown-linux-gnueabihf

# Build for ARM
cargo build --release --target armv7-unknown-linux-gnueabihf

# Output: target/armv7-unknown-linux-gnueabihf/release/libsandstar_haystack.a
```

### Step 3: CMake Integration

```cmake
# In shaystack/sandstar/sandstar/CMakeLists.txt

# Path to Rust static library
set(RUST_LIB_DIR "${CMAKE_SOURCE_DIR}/../sandstar_rust/target/${RUST_TARGET}/release")
set(RUST_LIB "${RUST_LIB_DIR}/libsandstar_haystack.a")

# Path to generated C header
set(RUST_INCLUDE_DIR "${CMAKE_SOURCE_DIR}/../sandstar_rust/include")

# Build Rust library before C code
add_custom_command(
    OUTPUT ${RUST_LIB}
    COMMAND cargo build --release --target ${RUST_TARGET}
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}/../sandstar_rust
    COMMENT "Building Rust haystack library"
)

add_custom_target(rust_haystack DEPENDS ${RUST_LIB})

# Add Rust header to include path
include_directories(${RUST_INCLUDE_DIR})

# Link Rust static library into SVM binary
target_link_libraries(svm
    ${RUST_LIB}
    pthread     # Required by Rust's std
    dl          # Required by Rust's std
    m           # Math library
)

# Ensure Rust builds first
add_dependencies(svm rust_haystack)
```

### Step 4: Docker Cross-Compilation Integration

The existing Docker build environment needs Rust toolchain:

```dockerfile
# Add to shaystack/Dockerfile
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    --default-toolchain stable \
    --target armv7-unknown-linux-gnueabihf

# Add ARM linker for Rust
ENV CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc

# Copy Rust source
COPY sandstar_rust/ /build/sandstar_rust/

# Build Rust before CMake
RUN cd /build/sandstar_rust && \
    cargo build --release --target armv7-unknown-linux-gnueabihf
```

### Linking Order

When linking a Rust static library into a C binary, the Rust standard library dependencies must be listed **after** the Rust library:

```
arm-linux-gnueabihf-gcc -o svm \
    vm.o engineio.o ... \
    -L/path/to/rust/lib -lsandstar_haystack \
    -lpthread -ldl -lm -lgcc_s
```

### Migration Path: Parallel C++ and Rust

During migration, both the C++ `shaystack.cpp` and the Rust library can coexist. Use a compile-time flag:

```cmake
option(USE_RUST_HAYSTACK "Use Rust haystack implementation" OFF)

if(USE_RUST_HAYSTACK)
    # Link Rust static library
    target_link_libraries(svm ${RUST_LIB} pthread dl m)
    target_compile_definitions(svm PRIVATE USE_RUST_HAYSTACK=1)
else()
    # Link existing C++ code
    target_sources(svm PRIVATE shaystack.cpp)
    target_link_libraries(svm haystack_cpp poco_net poco_crypto)
endif()
```

---

## 9. Complete FFI Function Reference

### 9.1 create_client

**Purpose:** Create a new Haystack HTTP client connection.

**C side (current -- shaystack.cpp:17-36):**

```c
extern "C" Cell shaystack_HaystackDevice_create_client(SedonaVM* vm, Cell* params) {
    // params[0].aval = const char* uri
    // params[1].aval = const char* username
    // params[2].aval = const char* password
    const char* uri = static_cast<const char*>(params[0].aval);
    const char* username = static_cast<const char*>(params[1].aval);
    const char* password = static_cast<const char*>(params[2].aval);

    haystack::Client* client = new haystack::Client(uri, username, password);
    Cell result;
    result.aval = client;
    return result;
    // On error: return nullCell
}
```

**Rust side (replacement):**

```rust
#[no_mangle]
pub extern "C" fn shaystack_HaystackDevice_create_client(
    _vm: *mut c_void,
    params: *mut CellFfi,
) -> CellFfi {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let params = unsafe { std::slice::from_raw_parts(params, 3) };

        let uri = match unsafe { ptr_to_str(params[0].aval) } {
            Some(s) => s,
            None => {
                eprintln!("shaystack: ERROR: null parameter in create_client");
                return CELL_NULL;
            }
        };
        let username = match unsafe { ptr_to_str(params[1].aval) } {
            Some(s) => s,
            None => {
                eprintln!("shaystack: ERROR: null parameter in create_client");
                return CELL_NULL;
            }
        };
        let password = match unsafe { ptr_to_str(params[2].aval) } {
            Some(s) => s,
            None => {
                eprintln!("shaystack: ERROR: null parameter in create_client");
                return CELL_NULL;
            }
        };

        match Client::new(uri, username, password) {
            Ok(client) => {
                let handle = Box::new(HaystackClient {
                    inner: Arc::new(client),
                });
                CellFfi { aval: Box::into_raw(handle) as *mut c_void }
            }
            Err(e) => {
                eprintln!("shaystack: EXCEPTION: create_client: {}", e);
                CELL_NULL
            }
        }
    }));

    result.unwrap_or(CELL_NULL)
}
```

### 9.2 delete_client

**Purpose:** Destroy a client handle and free its resources.

**C side (current -- shaystack.cpp:38-51):**

```c
extern "C" Cell shaystack_HaystackDevice_delete_client(SedonaVM* vm, Cell* params) {
    // params[0].aval = haystack::Client*
    haystack::Client* client = static_cast<haystack::Client*>(params[0].aval);
    delete client;
    return nullCell;
}
```

**Rust side:**

```rust
#[no_mangle]
pub extern "C" fn shaystack_HaystackDevice_delete_client(
    _vm: *mut c_void,
    params: *mut CellFfi,
) -> CellFfi {
    let params = unsafe { std::slice::from_raw_parts(params, 1) };
    let ptr = unsafe { params[0].aval };

    if !ptr.is_null() {
        // Reclaim ownership and drop
        unsafe { let _ = Box::from_raw(ptr as *mut HaystackClient); }
    }

    CELL_NULL
}
```

### 9.3 is_authenticated

**Purpose:** Check if client has completed authentication.

**C side (current -- shaystack.cpp:53-64):**

```c
extern "C" Cell shaystack_HaystackDevice_is_authenticated(SedonaVM* vm, Cell* params) {
    haystack::Client* client = static_cast<haystack::Client*>(params[0].aval);
    return client->is_authenticated() ? trueCell : falseCell;
}
```

**Rust side:**

```rust
#[no_mangle]
pub extern "C" fn shaystack_HaystackDevice_is_authenticated(
    _vm: *mut c_void,
    params: *mut CellFfi,
) -> CellFfi {
    let params = unsafe { std::slice::from_raw_parts(params, 1) };
    let ptr = unsafe { params[0].aval };

    if ptr.is_null() {
        return CELL_FALSE;
    }

    let client = unsafe { &*(ptr as *const HaystackClient) };
    if client.inner.is_authenticated() { CELL_TRUE } else { CELL_FALSE }
}
```

### 9.4 read_message

**Purpose:** Build HTTP request string for "read" op with filter.

**C side (current -- shaystack.cpp:119-137):**

```c
extern "C" Cell shaystack_HaystackDevice_read_message(SedonaVM* vm, Cell* params) {
    // params[0].aval = Client*
    // params[1].aval = const char* filter
    // params[2].aval = char* output buffer
    // params[3].ival = int buffer length
    haystack::Client* client = static_cast<haystack::Client*>(params[0].aval);
    const char* filter = static_cast<const char*>(params[1].aval);
    char* buf = static_cast<char*>(params[2].aval);
    int buf_len = params[3].ival;

    const std::string message = client->get_read_all_message(filter, 1);
    message.copy(buf, buf_len);
    buf[message.length()] = '\0';
    return trueCell;
}
```

**Rust side:**

```rust
#[no_mangle]
pub extern "C" fn shaystack_HaystackDevice_read_message(
    _vm: *mut c_void,
    params: *mut CellFfi,
) -> CellFfi {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let params = unsafe { std::slice::from_raw_parts(params, 4) };

        let client_ptr = unsafe { params[0].aval };
        let buf = unsafe { params[2].aval } as *mut c_char;

        if client_ptr.is_null() || buf.is_null() {
            eprintln!("shaystack: ERROR: null parameter in read_message");
            return CELL_FALSE;
        }

        let client = unsafe { &*(client_ptr as *const HaystackClient) };
        let filter = unsafe { ptr_to_str(params[1].aval) }.unwrap_or("");
        let buf_len = unsafe { params[3].ival };

        match client.inner.get_read_all_message(filter, 1) {
            Ok(message) => {
                unsafe {
                    if write_to_c_buffer(&message, buf, buf_len) {
                        CELL_TRUE
                    } else {
                        CELL_FALSE
                    }
                }
            }
            Err(e) => {
                eprintln!("shaystack: EXCEPTION: read_message: {}", e);
                CELL_FALSE
            }
        }
    }));

    result.unwrap_or(CELL_FALSE)
}
```

### 9.5 eval_message

**Purpose:** Build HTTP request string for "eval" op (vendor-specific expressions like Axon).

**C side (current -- shaystack.cpp:139-157):**

```c
extern "C" Cell shaystack_HaystackDevice_eval_message(SedonaVM* vm, Cell* params) {
    // params[0].aval = Client*, params[1].aval = filter, params[2].aval = buf, params[3].ival = len
    haystack::Client* client = static_cast<haystack::Client*>(params[0].aval);
    const char* filter = static_cast<const char*>(params[1].aval);
    char* buf = static_cast<char*>(params[2].aval);
    int buf_len = params[3].ival;

    const std::string message = client->get_eval_message(filter);
    message.copy(buf, buf_len);
    buf[message.length()] = '\0';
    return trueCell;
}
```

**Rust side:**

```rust
#[no_mangle]
pub extern "C" fn shaystack_HaystackDevice_eval_message(
    _vm: *mut c_void,
    params: *mut CellFfi,
) -> CellFfi {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let params = unsafe { std::slice::from_raw_parts(params, 4) };

        let client_ptr = unsafe { params[0].aval };
        let buf = unsafe { params[2].aval } as *mut c_char;

        if client_ptr.is_null() || buf.is_null() {
            eprintln!("shaystack: ERROR: null parameter in eval_message");
            return CELL_FALSE;
        }

        let client = unsafe { &*(client_ptr as *const HaystackClient) };
        let filter = unsafe { ptr_to_str(params[1].aval) }.unwrap_or("");
        let buf_len = unsafe { params[3].ival };

        match client.inner.get_eval_message(filter) {
            Ok(message) => unsafe {
                if write_to_c_buffer(&message, buf, buf_len) { CELL_TRUE }
                else { CELL_FALSE }
            },
            Err(e) => {
                eprintln!("shaystack: EXCEPTION: eval_message: {}", e);
                CELL_FALSE
            }
        }
    }));

    result.unwrap_or(CELL_FALSE)
}
```

### 9.6 watch_sub_message

**Purpose:** Build HTTP request string to subscribe a point to a watch.

**C side (current -- shaystack.cpp:159-181):**

```c
extern "C" Cell shaystack_HaystackDevice_watch_sub_message(SedonaVM* vm, Cell* params) {
    // params[0].aval = Client*
    // params[1].aval = const char* dis (display name)
    // params[2].aval = const char* watch_id
    // params[3].aval = const char* ref
    // params[4].aval = char* buf
    // params[5].ival = int buf_len
    haystack::Client* client = static_cast<haystack::Client*>(params[0].aval);
    const char* dis = static_cast<const char*>(params[1].aval);
    const char* watch_id = static_cast<const char*>(params[2].aval);
    const char* ref = static_cast<const char*>(params[3].aval);
    char* buf = static_cast<char*>(params[4].aval);
    int buf_len = params[5].ival;

    const std::string message = client->get_watch_sub(dis, watch_id, { ref });
    message.copy(buf, buf_len);
    buf[message.length()] = '\0';
    return trueCell;
}
```

**Rust side:**

```rust
#[no_mangle]
pub extern "C" fn shaystack_HaystackDevice_watch_sub_message(
    _vm: *mut c_void,
    params: *mut CellFfi,
) -> CellFfi {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let params = unsafe { std::slice::from_raw_parts(params, 6) };

        let client_ptr = unsafe { params[0].aval };
        let buf = unsafe { params[4].aval } as *mut c_char;

        if client_ptr.is_null() || buf.is_null() {
            eprintln!("shaystack: ERROR: null parameter in watch_sub_message");
            return CELL_FALSE;
        }

        let client = unsafe { &*(client_ptr as *const HaystackClient) };
        let dis = unsafe { ptr_to_str(params[1].aval) }.unwrap_or("");
        let watch_id = unsafe { ptr_to_str(params[2].aval) }.unwrap_or("");
        let ref_id = unsafe { ptr_to_str(params[3].aval) }.unwrap_or("");
        let buf_len = unsafe { params[5].ival };

        match client.inner.get_watch_sub(dis, watch_id, &[ref_id]) {
            Ok(message) => unsafe {
                if write_to_c_buffer(&message, buf, buf_len) { CELL_TRUE }
                else { CELL_FALSE }
            },
            Err(e) => {
                eprintln!("shaystack: EXCEPTION: watch_sub_message: {}", e);
                CELL_FALSE
            }
        }
    }));

    result.unwrap_or(CELL_FALSE)
}
```

### 9.7 watch_unsub_message

**Purpose:** Build HTTP request string to unsubscribe a point from a watch.

**C side (current -- shaystack.cpp:183-204):**

```c
extern "C" Cell shaystack_HaystackDevice_watch_unsub_message(SedonaVM* vm, Cell* params) {
    // params[0].aval = Client*
    // params[1].aval = const char* watch_id
    // params[2].aval = const char* ref
    // params[3].aval = char* buf
    // params[4].ival = int buf_len
    // ... builds unsub message, writes to buf ...
}
```

**Rust side:** Follows the identical pattern as watch_sub_message with 5 params.

### 9.8 write_float_point_message

**Purpose:** Build HTTP request string to write a float value to a point.

**C side (current -- shaystack.cpp:398-421):**

```c
extern "C" Cell shaystack_HaystackDevice_write_float_point_message(SedonaVM* vm, Cell* params) {
    // params[0].aval = Client*
    // params[1].aval = const char* point_id
    // params[2].ival = int level
    // params[3].fval = float val
    // params[4].ival = int duration
    // params[5].aval = char* buf
    // params[6].ival = int buf_len
    haystack::Client* client = static_cast<haystack::Client*>(params[0].aval);
    const char* point_id = static_cast<const char*>(params[1].aval);
    int level = params[2].ival;
    float val = params[3].fval;
    int duration = params[4].ival;
    char* buf = static_cast<char*>(params[5].aval);
    int buf_len = params[6].ival;

    std::string message = client->get_point_write_float(point_id, level, val, duration);
    message.copy(buf, buf_len);
    buf[message.length()] = '\0';
    return trueCell;
}
```

**Rust side:**

```rust
#[no_mangle]
pub extern "C" fn shaystack_HaystackDevice_write_float_point_message(
    _vm: *mut c_void,
    params: *mut CellFfi,
) -> CellFfi {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let params = unsafe { std::slice::from_raw_parts(params, 7) };

        let client_ptr = unsafe { params[0].aval };
        let buf = unsafe { params[5].aval } as *mut c_char;

        if client_ptr.is_null() || buf.is_null() {
            eprintln!("shaystack: ERROR: null parameter in write_float_point_message");
            return CELL_FALSE;
        }

        let client = unsafe { &*(client_ptr as *const HaystackClient) };
        let point_id = unsafe { ptr_to_str(params[1].aval) }.unwrap_or("");
        let level = unsafe { params[2].ival };
        let val = unsafe { params[3].fval };
        let duration = unsafe { params[4].ival };
        let buf_len = unsafe { params[6].ival };

        match client.inner.get_point_write_float(point_id, level, val, duration) {
            Ok(message) => unsafe {
                if write_to_c_buffer(&message, buf, buf_len) { CELL_TRUE }
                else { CELL_FALSE }
            },
            Err(e) => {
                eprintln!("shaystack: EXCEPTION: write_float_point_message: {}", e);
                CELL_FALSE
            }
        }
    }));

    result.unwrap_or(CELL_FALSE)
}
```

### 9.9 parse_float_response / parse_bool_response / parse_str_response

**Purpose:** Parse HTTP response Zinc grid and extract typed values.

These functions receive an HTTP response string (not a client handle), parse it as Zinc, and extract values from specific grid rows/columns.

**Rust side (parse_float_response):**

```rust
#[no_mangle]
pub extern "C" fn shaystack_HaystackDevice_parse_float_response(
    _vm: *mut c_void,
    params: *mut CellFfi,
) -> CellFfi {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let params = unsafe { std::slice::from_raw_parts(params, 4) };

        let response = unsafe { ptr_to_str(params[0].aval) }.unwrap_or("");
        let rownum = unsafe { params[1].ival } as usize;
        let col_name = unsafe { ptr_to_str(params[2].aval) }.unwrap_or("");
        let client_ptr = unsafe { params[3].aval };

        match parse_zinc_response(response) {
            Ok(grid) => {
                if grid.is_empty() || grid.is_err() || grid.rows().len() <= rownum {
                    return CELL_NULL;
                }
                match grid.rows()[rownum].get_num(col_name) {
                    Some(val) => CellFfi { fval: val as f32 },
                    None => {
                        // Clear auth on parse failure (matches C++ behavior)
                        if !client_ptr.is_null() {
                            let client = unsafe { &*(client_ptr as *const HaystackClient) };
                            client.inner.clear_auth();
                        }
                        CELL_NULL
                    }
                }
            }
            Err(e) => {
                eprintln!("shaystack: EXCEPTION in parse_float_response: {}", e);
                if !client_ptr.is_null() {
                    let client = unsafe { &*(client_ptr as *const HaystackClient) };
                    client.inner.clear_auth();
                }
                CELL_NULL
            }
        }
    }));

    result.unwrap_or(CELL_NULL)
}
```

---

## 10. Authentication Flow: SCRAM-SHA256 and Basic Auth

### Current C++ Implementation

The authentication system lives in:
- `auth/clientcontext.hpp` -- Manages auth state machine
- `auth/scramscheme.hpp` -- SCRAM-SHA-256 implementation (RFC 5802)
- `auth/basicscheme.hpp` -- HTTP Basic authentication fallback
- `auth/scheme.hpp` -- Abstract base class with registry
- `auth/authmsg.hpp` -- RFC 7235 message encoding/decoding

### Authentication State Machine

```
                                  ┌─────────┐
                                  │ Initial │
                                  └────┬────┘
                                       │
                              getHelloMsg(req)
                                       │
                                       ▼
                              ┌────────────────┐
                              │ Hello Sent     │
                              │ GET /about     │
                              └───────┬────────┘
                                      │
                            401 with WWW-Authenticate
                                      │
                                      ▼
                         ┌─────────────────────────┐
                         │ Parse schemes from       │
                         │ WWW-Authenticate header  │
                         └────────────┬─────────────┘
                                      │
                        ┌─────────────┼──────────────┐
                        ▼             ▼              ▼
                   ┌─────────┐  ┌──────────┐  ┌───────────┐
                   │ SCRAM   │  │ Basic    │  │ Other     │
                   └────┬────┘  └────┬─────┘  └───────────┘
                        │            │
         ┌──────────────┘            │
         ▼                           ▼
  ┌──────────────┐          ┌──────────────────┐
  │ SCRAM First  │          │ Basic: send      │
  │ client-first │          │ base64(user:pass)│
  │ message      │          └────────┬─────────┘
  └──────┬───────┘                   │
         │                           │
  Server responds with               │
  salt, iteration count              │
         │                           │
         ▼                           │
  ┌──────────────┐                   │
  │ SCRAM Final  │                   │
  │ client-proof │                   │
  └──────┬───────┘                   │
         │                           │
         ▼                           ▼
  ┌──────────────────────────────────────┐
  │ Server: 200 OK with auth cookie     │
  │ → m_authenticated = true            │
  │ → Store auth headers for future use │
  └──────────────────────────────────────┘
```

### Async Authentication (processAuthMessage)

The Sedona VM cannot do synchronous HTTP round-trips for auth. Instead, it uses an async step-by-step approach:

```c
// From shaystack.cpp:88-117
extern "C" Cell shaystack_HaystackDevice_get_auth_message(SedonaVM* vm, Cell* params) {
    // params[0].aval = Client*
    // params[1].aval = const char* last_response (from previous HTTP response)
    // params[2].aval = char* buf (for next HTTP request)
    // params[3].ival = int buf_len

    haystack::Client* client = static_cast<haystack::Client*>(params[0].aval);
    const char* last_response = static_cast<const char*>(params[1].aval);
    char* buf = static_cast<char*>(params[2].aval);
    int buf_len = params[3].ival;

    HTTPRequest req;
    if (client->processAuthMessage(req, last_response)) {
        // More auth steps needed -- write next request to buf
        std::stringstream ss;
        req.write(ss);
        ss.str().copy(buf, buf_len);
        return trueCell;
    } else {
        return falseCell; // Auth complete (check is_authenticated)
    }
}
```

### Rust Auth Implementation

```rust
// In src/auth/mod.rs

pub mod scram;
pub mod basic;

use std::collections::HashMap;

/// Authentication message per RFC 7235
pub struct AuthMsg {
    pub scheme: String,
    pub params: HashMap<String, String>,
}

impl AuthMsg {
    /// Parse WWW-Authenticate header value
    pub fn parse(header: &str) -> Result<AuthMsg, AuthError> {
        let (scheme, rest) = header.split_once(' ')
            .ok_or(AuthError::InvalidHeader)?;

        let params = parse_auth_params(rest)?;

        Ok(AuthMsg {
            scheme: scheme.to_lowercase(),
            params,
        })
    }

    /// Encode as Authorization header value
    pub fn encode(&self) -> String {
        let params: Vec<String> = self.params.iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        format!("{} {}", self.scheme, params.join(", "))
    }
}

/// Authentication state machine
pub struct AuthContext {
    uri: String,
    user: String,
    pass: String,
    authenticated: bool,
    headers: HashMap<String, String>,
    stash: HashMap<String, String>,
    scheme: Option<Box<dyn AuthScheme>>,
}

pub trait AuthScheme: Send {
    fn name(&self) -> &str;
    fn on_client(&self, ctx: &mut AuthContext, msg: &AuthMsg) -> Result<AuthMsg, AuthError>;
    fn on_success(&self, ctx: &mut AuthContext, msg: &AuthMsg);
}
```

### SCRAM-SHA-256 in Rust

```rust
// In src/auth/scram.rs

use sha2::{Sha256, Digest};
use hmac::{Hmac, Mac};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rand::Rng;

type HmacSha256 = Hmac<Sha256>;

pub struct ScramScheme;

impl ScramScheme {
    /// Generate client-first message
    fn first_msg(&self, ctx: &mut AuthContext) -> Result<AuthMsg, AuthError> {
        // Generate random client nonce (16 bytes)
        let nonce_bytes: [u8; 16] = rand::thread_rng().gen();
        let client_nonce = BASE64.encode(nonce_bytes);

        // Store for later verification
        ctx.stash.insert("c_nonce".into(), client_nonce.clone());

        // Build client-first-message-bare
        let bare = format!("n={},r={}", ctx.user, client_nonce);
        ctx.stash.insert("c1_bare".into(), bare.clone());

        // GS2 header + bare message
        let full = format!("n,,{}", bare);
        let data = BASE64.encode(full.as_bytes());

        let mut params = HashMap::new();
        params.insert("data".into(), data);

        // Include handshakeToken if present
        if let Some(token) = ctx.stash.get("handshakeToken") {
            params.insert("handshakeToken".into(), token.clone());
        }

        Ok(AuthMsg { scheme: "scram".into(), params })
    }

    /// Generate client-final message with proof
    fn final_msg(&self, ctx: &mut AuthContext, server_msg: &AuthMsg) -> Result<AuthMsg, AuthError> {
        let data_b64 = server_msg.params.get("data")
            .ok_or(AuthError::MissingParam("data"))?;
        let data = String::from_utf8(BASE64.decode(data_b64)?)?;

        // Parse server-first-message: r=...,s=...,i=...
        let server_nonce = parse_field(&data, "r")?;
        let salt_b64 = parse_field(&data, "s")?;
        let iterations: u32 = parse_field(&data, "i")?.parse()?;

        let salt = BASE64.decode(&salt_b64)?;
        let c1_bare = ctx.stash.get("c1_bare")
            .ok_or(AuthError::MissingStash("c1_bare"))?;

        // Compute salted password via PBKDF2
        let salted_password = pbkdf2_sha256(ctx.pass.as_bytes(), &salt, iterations);

        // client-final-message-without-proof
        let c2_no_proof = format!("c=biws,r={}", server_nonce);

        // auth message = c1_bare + "," + server_first + "," + c2_no_proof
        let auth_msg = format!("{},{},{}", c1_bare, data, c2_no_proof);

        // Compute client proof
        let client_key = hmac_sha256(&salted_password, b"Client Key");
        let stored_key = sha256(&client_key);
        let client_sig = hmac_sha256(&stored_key, auth_msg.as_bytes());

        let mut client_proof = client_key.clone();
        for (i, b) in client_sig.iter().enumerate() {
            client_proof[i] ^= b;
        }

        let proof_b64 = BASE64.encode(&client_proof);
        let full = format!("{},p={}", c2_no_proof, proof_b64);
        let data_out = BASE64.encode(full.as_bytes());

        // Store server key for verification
        let server_key = hmac_sha256(&salted_password, b"Server Key");
        let server_sig = hmac_sha256(&server_key, auth_msg.as_bytes());
        ctx.stash.insert("server_sig".into(), BASE64.encode(&server_sig));

        let mut params = HashMap::new();
        params.insert("data".into(), data_out);

        if let Some(token) = server_msg.params.get("handshakeToken") {
            params.insert("handshakeToken".into(), token.clone());
        }

        Ok(AuthMsg { scheme: "scram".into(), params })
    }
}
```

### Rust Crate Dependencies for Auth

```toml
[dependencies]
sha2 = "0.10"           # SHA-256 digest
hmac = "0.12"            # HMAC-SHA-256
pbkdf2 = "0.12"          # PBKDF2 key derivation
base64 = "0.22"          # Base64 encoding/decoding
rand = "0.8"             # Random nonce generation
```

These are pure Rust crates with no C dependencies, replacing the POCO Crypto dependency. This eliminates the OpenSSL cross-compilation requirement for ARM7.

---

## 11. Complete FFI Function Inventory

All functions that must be implemented in Rust, with their parameter signatures:

| # | Function | Params | Returns | Category |
|---|----------|--------|---------|----------|
| 1 | `create_client` | uri, user, pass | Client* | Lifecycle |
| 2 | `delete_client` | Client* | null | Lifecycle |
| 3 | `is_authenticated` | Client* | bool | Auth |
| 4 | `isSessionConnected` | Client* | bool | Auth |
| 5 | `get_auth_message` | Client*, response, buf, len | bool | Auth |
| 6 | `open` | uri, user, pass | Client* | Auth |
| 7 | `read_message` | Client*, filter, buf, len | bool | Haystack Ops |
| 8 | `eval_message` | Client*, filter, buf, len | bool | Haystack Ops |
| 9 | `watch_sub_message` | Client*, dis, id, ref, buf, len | bool | Watch |
| 10 | `watch_unsub_message` | Client*, id, ref, buf, len | bool | Watch |
| 11 | `watch_poll_message` | Client*, id, buf, len | bool | Watch |
| 12 | `has_float` | response, row, col | bool | Parse |
| 13 | `has_bool` | response, row, col | bool | Parse |
| 14 | `parse_float_response` | response, row, col, Client* | float | Parse |
| 15 | `parse_bool_response` | response, row, col | bool | Parse |
| 16 | `parse_str_response` | response, row, col, checked, buf, len | bool | Parse |
| 17 | `is_empty` | response | bool | Parse |
| 18 | `is_err` | response | bool | Parse |
| 19 | `write_float_point_message` | Client*, id, level, val, dur, buf, len | bool | Write |
| 20 | `write_bool_point_message` | Client*, id, level, val, dur, buf, len | bool | Write |
| 21 | `reset_point_message` | Client*, id, level, buf, len | bool | Write |
| 22 | `write_read_point_message` | Client*, id, buf, len | bool | Write |
| 23 | `call` | Client*, op | null | Debug |
| 24 | `read_by_id` | Client* | (stub) | (Future) |
| 25 | `read_float` | Client*, filter | float | Direct Read |
| 26 | `read_bool` | Client*, filter | bool | Direct Read |
| 27 | `eval_bool` | Client*, expr, row, col | bool | Direct Eval |
| 28 | `eval_float` | Client*, expr, row, col | float | Direct Eval |
| 29 | `is_filter_valid` | filter | bool | Validation |

**Total: 29 FFI functions to implement in Rust.**

---

## 12. Migration Order

### Phase 1: Parse Functions (No Network, No State)

Implement functions 12-18, 29 first. These are pure functions that parse Zinc strings -- no client handle, no network, no state. They are the easiest to test and the safest starting point.

### Phase 2: Message Building Functions (State, No Network)

Implement functions 7-11, 19-22. These use the client handle to build HTTP request strings but do not perform actual HTTP calls. The Sedona VM handles the actual HTTP transport.

### Phase 3: Authentication (State + Crypto)

Implement functions 3-6. These require the SCRAM-SHA-256 and Basic auth implementations.

### Phase 4: Direct Operations (Full Network)

Implement functions 1-2, 23-28. These perform actual HTTP calls and manage the client lifecycle. Function 24 (`read_by_id`) is currently a stub and can remain so.

### Phase 5: Remove C++ Code

Once all 29 functions are implemented and tested, remove `shaystack.cpp` and the C++ haystack library from the build.
