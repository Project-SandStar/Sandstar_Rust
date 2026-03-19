# 10 - Build System & Cross-Compilation Migration

## Overview

This document covers the complete migration of the Sandstar build system from CMake + Docker + GCC cross-compilation to Cargo + Rust cross-compilation. It addresses every aspect of the current build pipeline and shows the Rust equivalent.

### Current Build System Summary

| Component | Current | Rust Replacement |
|---|---|---|
| Build tool | CMake 3.27 + Make/Ninja | Cargo |
| Cross-compiler | `arm-linux-gnueabihf-g++-10` (GCC 10) | `rustc` with `armv7-unknown-linux-gnueabihf` target |
| Docker image | Debian 11 Bullseye, ~2GB | `cross` tool or `rustup target add` |
| Static analysis | cppcheck | `cargo clippy` |
| Package format | `.deb` via CPack | `.deb` via `cargo-deb` |
| Build script | `build_and_extract_fixed.sh` (303 lines) | `cargo build --target armv7-unknown-linux-gnueabihf` |
| Toolchain file | `armv7l.cmake` (100 lines) | `.cargo/config.toml` (~10 lines) |

---

## 1. Rust Target: `armv7-unknown-linux-gnueabihf`

The BeagleBone uses an ARM Cortex-A8 processor (ARMv7-A architecture) with hard-float ABI. The exact Rust target is:

```
armv7-unknown-linux-gnueabihf
```

**Target details:**
- Architecture: ARMv7-A (32-bit ARM)
- ABI: GNU EABI, hard-float (`gnueabihf`)
- Support tier: **Tier 2 with host tools** (pre-built `std`, cross-compilation fully supported)
- OS: Linux (glibc)
- Equivalent to current: `-march=armv7-a -mfpu=neon -mfloat-abi=hard`

**Verify available targets:**
```bash
rustup target list | grep armv7
# Output:
# armv7-unknown-linux-gnueabihf
# armv7-unknown-linux-musleabihf
# armv7-unknown-linux-gnueabi
```

---

## 2. Cross-Compilation Method 1: `cross` Tool (Docker-based)

The `cross` tool provides the closest equivalent to the current Docker-based build approach. It automatically manages Docker containers with the correct cross-compilation toolchain.

### Installation

```bash
cargo install cross
```

### Usage

```bash
# Build for ARM (equivalent to current Docker build)
cross build --target armv7-unknown-linux-gnueabihf --release

# Run tests on ARM via QEMU (inside Docker)
cross test --target armv7-unknown-linux-gnueabihf

# Build .deb package
cross build --target armv7-unknown-linux-gnueabihf --release
# Then use cargo-deb (see section 7)
```

### How `cross` Works

1. Downloads a pre-built Docker image with ARM toolchain and QEMU
2. Mounts your project directory into the container
3. Runs `cargo build` inside the container with proper environment
4. Outputs ARM binary to `target/armv7-unknown-linux-gnueabihf/release/`

### Comparison with Current Docker Flow

| Step | Current (Docker + GCC) | Rust (`cross`) |
|---|---|---|
| Docker image | Custom Dockerfile, ~2GB | Pre-built `ghcr.io/cross-rs` image, ~500MB |
| Image maintenance | Manual: Debian packages, CMake install | Automatic: managed by cross team |
| Build command | Complex bash script (303 lines) | `cross build --target armv7-unknown-linux-gnueabihf --release` |
| Source mounting | Manual `docker cp` + flatten layers | Automatic volume mount |
| Code update | `FORCE_CODEUPDATE=1` + container export/import | Automatic (mounts host directory) |
| Output extraction | `docker cp` + `find ... -name "*.deb"` | Binary in `target/` directory |

### Custom `Cross.toml` (if needed)

If the Sedona VM C code requires special system libraries:

```toml
# Cross.toml (project root)
[target.armv7-unknown-linux-gnueabihf]
# Use custom Docker image if needed for Sedona VM dependencies
# image = "sandstar-cross:latest"

# Or add packages to the default image
pre-build = [
    "dpkg --add-architecture armhf",
    "apt-get update && apt-get install -y libc6-dev:armhf"
]

[build.env]
passthrough = ["SANDSTAR_CONFIG_PATH"]
```

---

## 3. Cross-Compilation Method 2: Native Toolchain

For faster iteration without Docker overhead, install the ARM GCC linker natively and use `rustup` to add the target.

### Setup

```bash
# Install ARM cross-linker (on Ubuntu/Debian host)
sudo apt-get install gcc-arm-linux-gnueabihf

# Add Rust target
rustup target add armv7-unknown-linux-gnueabihf
```

### Configure Linker

Create `.cargo/config.toml` in the project root:

```toml
# .cargo/config.toml

[target.armv7-unknown-linux-gnueabihf]
linker = "arm-linux-gnueabihf-gcc"
rustflags = [
    "-C", "target-feature=+neon",
    "-C", "link-arg=-static-libgcc",
]

# Runner for testing via QEMU (optional)
runner = "qemu-arm -L /usr/arm-linux-gnueabihf"
```

### Build

```bash
# Debug build
cargo build --target armv7-unknown-linux-gnueabihf

# Release build
cargo build --target armv7-unknown-linux-gnueabihf --release

# Output location
ls -la target/armv7-unknown-linux-gnueabihf/release/sandstar
```

### Comparison: Docker vs Native Toolchain

| Factor | `cross` (Docker) | Native Toolchain |
|---|---|---|
| Setup complexity | `cargo install cross` | Install GCC + rustup target + config.toml |
| Build speed | ~10-30% slower (Docker overhead) | Fastest |
| Reproducibility | Excellent (containerized) | Depends on host packages |
| CI/CD compatibility | Excellent (works on any Docker host) | Requires specific packages |
| System library linking | Automatic (in container) | Manual sysroot setup |
| Disk usage | ~500MB Docker image | ~100MB toolchain |

**Recommendation:** Use native toolchain for development, `cross` for CI/CD and release builds.

---

## 4. `.cargo/config.toml` Configuration

Full configuration for the Sandstar project:

```toml
# .cargo/config.toml

# ── ARM Cross-Compilation ────────────────────────────────────────

[target.armv7-unknown-linux-gnueabihf]
linker = "arm-linux-gnueabihf-gcc"
rustflags = [
    # Enable NEON SIMD (matches current -mfpu=neon)
    "-C", "target-feature=+neon",
    # Static link to libgcc (matches current -static-libstdc++)
    "-C", "link-arg=-static-libgcc",
    # Equivalent of current -Wl,--as-needed
    "-C", "link-arg=-Wl,--as-needed",
]

# QEMU runner for testing
runner = "qemu-arm -L /usr/arm-linux-gnueabihf"

# ── Build Defaults ───────────────────────────────────────────────

[build]
# Uncomment to default to ARM target (so you don't need --target every time)
# target = "armv7-unknown-linux-gnueabihf"

# Number of parallel codegen units in debug mode
# BeagleBone has 1 core; host has many
jobs = 8

# ── Alias Commands ───────────────────────────────────────────────

[alias]
arm-build = "build --target armv7-unknown-linux-gnueabihf --release"
arm-debug = "build --target armv7-unknown-linux-gnueabihf"
arm-test = "test --target armv7-unknown-linux-gnueabihf"
lint = "clippy --all-targets --all-features -- -W clippy::all -W clippy::pedantic"
```

With these aliases:

```bash
cargo arm-build    # Release build for ARM
cargo arm-debug    # Debug build for ARM
cargo lint         # Run clippy static analysis
```

---

## 5. Linking Sedona VM (C) into Rust: `build.rs` with `cc` Crate

The Sedona VM remains in C and is linked into the Rust binary via FFI. The `build.rs` build script compiles the C code using the `cc` crate.

### `build.rs`

```rust
// build.rs
fn main() {
    // Tell Cargo to re-run build.rs if any C source changes
    println!("cargo:rerun-if-changed=sedona/");

    // Compile Sedona VM C code
    cc::Build::new()
        // Core VM sources
        .file("sedona/src/svm/main.c")
        .file("sedona/src/svm/vm.c")
        .file("sedona/src/svm/scode.c")
        .file("sedona/src/svm/sys.c")
        .file("sedona/src/svm/errorcodes.c")
        // Platform-specific
        .file("sedona/src/svm/platform/linux/nativetable_linux.c")
        .file("sedona/src/svm/platform/linux/sys_linux.c")
        // Include paths
        .include("sedona/src/svm")
        .include("sedona/src/svm/platform/linux")
        // Compiler flags matching current CMake setup
        .flag("-std=c11")
        .flag("-Wall")
        .flag("-Wextra")
        .warnings(true)
        // Optimization
        .opt_level_str("2")
        // Name the output library
        .compile("sedona_vm");

    // Link against system libraries needed by Sedona VM
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-lib=m");
}
```

### FFI Bindings to Sedona VM

```rust
// src/sedona/ffi.rs

extern "C" {
    /// Initialize the Sedona Virtual Machine
    pub fn svm_init(plat_file: *const std::ffi::c_char) -> std::ffi::c_int;

    /// Run one cycle of the Sedona VM
    pub fn svm_step() -> std::ffi::c_int;

    /// Shutdown the Sedona VM
    pub fn svm_shutdown();

    /// Get a component property value
    pub fn svm_get_prop(
        comp_id: std::ffi::c_int,
        slot_id: std::ffi::c_int,
    ) -> f64;

    /// Set a component property value
    pub fn svm_set_prop(
        comp_id: std::ffi::c_int,
        slot_id: std::ffi::c_int,
        value: f64,
    );
}

// Safe Rust wrapper
pub struct SedonaVm;

impl SedonaVm {
    pub fn init(plat_file: &str) -> Result<Self, String> {
        let c_path = std::ffi::CString::new(plat_file)
            .map_err(|e| format!("Invalid path: {}", e))?;
        let result = unsafe { svm_init(c_path.as_ptr()) };
        if result == 0 {
            Ok(SedonaVm)
        } else {
            Err(format!("SVM init failed with code {}", result))
        }
    }

    pub fn step(&self) -> Result<(), String> {
        let result = unsafe { svm_step() };
        if result == 0 {
            Ok(())
        } else {
            Err(format!("SVM step failed with code {}", result))
        }
    }

    pub fn get_prop(&self, comp_id: i32, slot_id: i32) -> f64 {
        unsafe { svm_get_prop(comp_id, slot_id) }
    }

    pub fn set_prop(&self, comp_id: i32, slot_id: i32, value: f64) {
        unsafe { svm_set_prop(comp_id, slot_id, value) }
    }
}

impl Drop for SedonaVm {
    fn drop(&mut self) {
        unsafe { svm_shutdown() }
    }
}
```

### Cross-Compilation with `cc` Crate

The `cc` crate automatically uses the correct cross-compiler when building for ARM:
- When `--target armv7-unknown-linux-gnueabihf` is set, `cc` uses `arm-linux-gnueabihf-gcc`
- No additional configuration needed in `build.rs`
- The `.cargo/config.toml` linker setting is automatically picked up

---

## 6. Static vs Dynamic Linking

### Current Approach

The current build uses:
- Static linking for `libstdc++` (`-static-libstdc++`)
- Dynamic linking for `libc` (system glibc)
- Static linking for `engine_logger` (custom C++ library)
- Dynamic linking for POCO shared libraries

### Rust Approach

```toml
# For fully static binary (musl libc):
# Target: armv7-unknown-linux-musleabihf
# Pro: Single binary, no runtime dependencies
# Con: No glibc-specific features, slightly larger

# For dynamic glibc linking (recommended, matches current):
# Target: armv7-unknown-linux-gnueabihf
# Pro: Smaller binary, system glibc compatibility
# Con: Requires glibc >= 2.28 on target
```

**Recommendation:** Use `armv7-unknown-linux-gnueabihf` (dynamic glibc) for consistency with the current Debian 9 target. The BeagleBone already has glibc installed.

### Linking Strategy

| Component | Current | Rust |
|---|---|---|
| C standard library | Dynamic (glibc) | Dynamic (glibc) |
| C++ standard library | Static (`-static-libstdc++`) | N/A (no C++ runtime needed) |
| POCO | Dynamic (.so) | N/A (replaced by Rust crates) |
| Boost | Header-only (static) | N/A (replaced by Rust crates) |
| Sedona VM | Linked via CMake | Static via `cc` crate in build.rs |
| pthread | Dynamic | Dynamic (`-lpthread`) |
| math | Dynamic | Dynamic (`-lm`) |

**Key advantage:** Rust eliminates the need for `libstdc++` entirely. The only C runtime dependency is `libc` (glibc), which is already on the target system.

---

## 7. `.deb` Package Generation: `cargo-deb`

### Installation

```bash
cargo install cargo-deb
```

### Configuration in `Cargo.toml`

```toml
[package.metadata.deb]
name = "sandstar"
maintainer = "Sandstar Team"
copyright = "2024, Sandstar"
license-file = ["LICENSE", "0"]
depends = "$auto"
section = "embedded"
priority = "optional"
assets = [
    # Main binaries
    ["target/armv7-unknown-linux-gnueabihf/release/sandstar", "/home/eacio/sandstar/bin/engine", "755"],
    ["target/armv7-unknown-linux-gnueabihf/release/sandstar-read", "/home/eacio/sandstar/bin/read", "755"],
    ["target/armv7-unknown-linux-gnueabihf/release/sandstar-write", "/home/eacio/sandstar/bin/write", "755"],
    ["target/armv7-unknown-linux-gnueabihf/release/sandstar-watch", "/home/eacio/sandstar/bin/watch", "755"],
    ["target/armv7-unknown-linux-gnueabihf/release/sandstar-notify", "/home/eacio/sandstar/bin/notify", "755"],
    ["target/armv7-unknown-linux-gnueabihf/release/sandstar-channels", "/home/eacio/sandstar/bin/channels", "755"],
    ["target/armv7-unknown-linux-gnueabihf/release/sandstar-polls", "/home/eacio/sandstar/bin/polls", "755"],
    ["target/armv7-unknown-linux-gnueabihf/release/sandstar-status", "/home/eacio/sandstar/bin/status", "755"],
    ["target/armv7-unknown-linux-gnueabihf/release/sandstar-tables", "/home/eacio/sandstar/bin/tables", "755"],
    ["target/armv7-unknown-linux-gnueabihf/release/sandstar-convert", "/home/eacio/sandstar/bin/convert", "755"],
    # Configuration files
    ["etc/config/*", "/home/eacio/sandstar/etc/config/", "644"],
    # Systemd service
    ["etc/sandstar.service", "/etc/systemd/system/sandstar.service", "644"],
]
# Architecture for BeagleBone
conf-files = ["/home/eacio/sandstar/etc/config/"]

[package.metadata.deb.systemd-units]
unit-name = "sandstar"
enable = true
start = false
```

### Build `.deb`

```bash
# Build and package in one step
cargo deb --target armv7-unknown-linux-gnueabihf

# Output: target/armv7-unknown-linux-gnueabihf/debian/sandstar_0.1.0_armhf.deb
```

### Install on Device

```bash
# Same as current workflow
scp sandstar_0.1.0_armhf.deb root@beaglebone:/tmp/
ssh root@beaglebone dpkg -i /tmp/sandstar_0.1.0_armhf.deb
systemctl restart sandstar
```

---

## 8. CI/CD: Replacing `build_and_extract_fixed.sh`

### Current Script Analysis

The `build_and_extract_fixed.sh` (303 lines) handles:
1. Build mode selection (native vs ARM)
2. Docker image creation/update (`FORCE_REBUILD`, `FORCE_CODEUPDATE`)
3. Docker container lifecycle (run, monitor, extract artifacts)
4. Static analysis (cppcheck)
5. `.deb` extraction from Docker container
6. Log extraction and summary

### Rust Replacement: Simple Shell Script

```bash
#!/bin/bash
# build.sh - Sandstar Rust build script
set -e

BUILD_MODE=${BUILD_MODE:-arm}
BUILD_TYPE=${BUILD_TYPE:-release}
RUN_CLIPPY=${RUN_CLIPPY:-1}
TARGET="armv7-unknown-linux-gnueabihf"

echo "=========================================="
echo "Sandstar Rust Build"
echo "  Mode: $BUILD_MODE"
echo "  Type: $BUILD_TYPE"
echo "  Clippy: $RUN_CLIPPY"
echo "=========================================="

if [ "$BUILD_MODE" = "native" ]; then
    # Native build for local testing
    if [ "$BUILD_TYPE" = "release" ]; then
        cargo build --release
    else
        cargo build
    fi
elif [ "$BUILD_MODE" = "arm" ]; then
    # ARM cross-compilation
    if [ "$BUILD_TYPE" = "release" ]; then
        cross build --target $TARGET --release
    else
        cross build --target $TARGET
    fi
else
    echo "Error: Invalid BUILD_MODE=$BUILD_MODE"
    exit 1
fi

# Static analysis
if [ "$RUN_CLIPPY" = "1" ]; then
    echo ""
    echo "=== Running Static Analysis (clippy) ==="
    cargo clippy --all-targets --all-features -- \
        -W clippy::all \
        -W clippy::pedantic \
        -W clippy::nursery \
        -A clippy::module_name_repetitions \
        2>&1 | tee clippy.log

    ERROR_COUNT=$(grep -c "^error" clippy.log 2>/dev/null || echo "0")
    WARNING_COUNT=$(grep -c "^warning" clippy.log 2>/dev/null || echo "0")
    echo "Errors: $ERROR_COUNT, Warnings: $WARNING_COUNT"
fi

# Package
if [ "$BUILD_MODE" = "arm" ] && [ "$BUILD_TYPE" = "release" ]; then
    echo ""
    echo "=== Building .deb Package ==="
    cargo deb --target $TARGET --no-build
    DEB=$(find target/$TARGET/debian -name "*.deb" | head -1)
    cp "$DEB" .
    echo "Package: $(basename $DEB)"
fi

echo ""
echo "Build complete!"
```

**Lines reduced: 303 (current) -> ~55 (Rust version) = 82% reduction**

### GitHub Actions CI

```yaml
# .github/workflows/build.yml
name: Build Sandstar

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - run: cargo fmt --check
      - run: cargo clippy --all-targets --all-features -- -D warnings

  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --all-features

  build-arm:
    runs-on: ubuntu-latest
    needs: [lint, test]
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: armv7-unknown-linux-gnueabihf
      - name: Install cross
        run: cargo install cross
      - name: Build ARM binary
        run: cross build --target armv7-unknown-linux-gnueabihf --release
      - name: Build .deb package
        run: |
          cargo install cargo-deb
          cargo deb --target armv7-unknown-linux-gnueabihf --no-build
      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: sandstar-deb
          path: target/armv7-unknown-linux-gnueabihf/debian/*.deb
```

---

## 9. Static Analysis: `cargo clippy` Replaces `cppcheck`

### Current: cppcheck

```bash
# Current: Integrated into CMake build
make cppcheck
# Produces cppcheck.log with error/warning reports
```

Known issues found by cppcheck:
- `engineio.c:141` - Missing return statement
- `grid.cpp:361` - Uninitialized variables `t_r`, `o_r`
- `points.cpp:232` - Null pointer dereference of `m_ops`

### Rust: clippy

Clippy is Rust's official linter, integrated into the compiler. It catches issues at compile time that cppcheck can only detect through static analysis of already-compiled code.

```bash
# Basic clippy run
cargo clippy

# Strict mode (recommended for CI)
cargo clippy --all-targets --all-features -- \
    -W clippy::all \
    -W clippy::pedantic \
    -W clippy::nursery \
    -D warnings           # Treat warnings as errors in CI

# With specific lints
cargo clippy -- \
    -W clippy::unwrap_used \
    -W clippy::expect_used \
    -W clippy::panic \
    -W clippy::unimplemented
```

### Comparison: What Each Tool Catches

| Issue Category | cppcheck | Rust Compiler | clippy |
|---|---|---|---|
| Null pointer dereference | Yes (heuristic) | **Impossible** (`Option<T>`) | N/A |
| Uninitialized variables | Yes (heuristic) | **Compile error** | N/A |
| Missing return statements | Yes | **Compile error** | N/A |
| Buffer overflow | Sometimes | **Compile error** (bounds check) | N/A |
| Memory leaks | Sometimes | **Impossible** (ownership) | N/A |
| Use-after-free | Rarely | **Compile error** (borrow checker) | N/A |
| Dead code | Yes | Yes (warning) | Yes (more thorough) |
| Unused variables | Yes | Yes (warning) | Yes |
| Code style | No | No | **Yes** (500+ lint rules) |
| Performance anti-patterns | No | No | **Yes** (needless clones, etc.) |
| Error handling quality | No | No | **Yes** (unwrap detection) |
| Idiomatic usage | No | No | **Yes** (Rust-specific patterns) |

**Key insight:** The three critical cppcheck issues in the current codebase (`engineio.c:141`, `grid.cpp:361`, `points.cpp:232`) are **impossible to write in Rust** because:
1. Missing return statement: Rust requires all code paths to return a value
2. Uninitialized variables: Rust requires initialization before use
3. Null pointer dereference: Rust has no null; uses `Option<T>` instead

### `clippy.toml` Configuration

```toml
# clippy.toml (project root)

# Maximum cognitive complexity allowed (default 25)
cognitive-complexity-threshold = 15

# Minimum number of items before triggering "too many arguments" lint
too-many-arguments-threshold = 8

# Types that are allowed to be used with `.unwrap()`
# (Only in test code)
allow-unwrap-in-tests = true

# Disallow print statements in non-test code
disallowed-methods = ["std::io::stdout", "std::io::stderr"]
```

---

## 10. Debug Builds: `cargo build` vs `cargo build --release`

### Build Profiles

```bash
# Debug build (fast compilation, full debug symbols, bounds checks, overflow checks)
cargo build
# Output: target/debug/sandstar (or target/armv7.../debug/sandstar)
# ~10-50MB binary, unoptimized, 1-2 second compile (incremental)

# Release build (slow compilation, optimized, stripped)
cargo build --release
# Output: target/release/sandstar (or target/armv7.../release/sandstar)
# ~2-5MB binary, fully optimized, 30-120 second compile
```

### Debugging with GDB

```bash
# Build with debug symbols (default in debug profile)
cargo build --target armv7-unknown-linux-gnueabihf

# Copy to device and run with GDB
scp target/armv7-unknown-linux-gnueabihf/debug/sandstar root@beaglebone:/tmp/
ssh root@beaglebone gdb /tmp/sandstar

# Or use gdbserver for remote debugging
ssh root@beaglebone gdbserver :1234 /tmp/sandstar
gdb-multiarch target/armv7-unknown-linux-gnueabihf/debug/sandstar
(gdb) target remote beaglebone:1234
```

### Debugging with Valgrind

```bash
# Native debug build
cargo build

# Run with valgrind
valgrind --leak-check=full --track-origins=yes target/debug/sandstar

# On ARM (via QEMU)
cross run --target armv7-unknown-linux-gnueabihf
# Note: valgrind is less useful in Rust because the borrow checker
# prevents most memory issues at compile time
```

### Custom Debug Profile

```toml
# Cargo.toml - for faster debug builds with some optimization

[profile.dev]
opt-level = 1          # Some optimization (faster than 0 at runtime)
debug = true           # Full debug symbols
overflow-checks = true # Catch integer overflow
lto = false            # No LTO (faster compilation)

[profile.dev.package."*"]
opt-level = 2          # Optimize dependencies (compile once, run fast)
```

---

## 11. Binary Size Optimization

The BeagleBone has limited storage (~4GB eMMC). Binary size matters.

### Release Profile Optimization

```toml
[profile.release]
opt-level = "z"        # Optimize for size (smallest binary)
lto = true             # Link-Time Optimization (eliminates dead code across crates)
strip = true           # Strip debug symbols from binary
codegen-units = 1      # Single codegen unit (maximum optimization opportunities)
panic = "abort"        # No unwinding machinery (saves ~100KB)
```

### Size Comparison Estimates

| Configuration | Estimated Binary Size | Notes |
|---|---|---|
| Debug (default) | 20-50 MB | Debug symbols, no optimization |
| Release (default) | 5-10 MB | Standard optimization |
| Release + strip | 3-6 MB | Symbols removed |
| Release + LTO + strip | 2-4 MB | Dead code eliminated |
| Release + `opt-level=z` + LTO + strip + `panic=abort` | 1-3 MB | Maximum size reduction |

### Additional Size Reduction Techniques

```bash
# Check what contributes to binary size
cargo install cargo-bloat
cargo bloat --release --target armv7-unknown-linux-gnueabihf -n 20

# Find duplicate code
cargo install cargo-bloat
cargo bloat --release --crates

# UPX compression (optional, for extreme size reduction)
upx --best target/armv7-unknown-linux-gnueabihf/release/sandstar
# Can reduce by 50-70% more, but slower startup
```

### Comparison with Current Binary Sizes

| Current Binary | Size | Rust Equivalent (est.) |
|---|---|---|
| `engine` (C, statically linked) | ~500 KB | ~1-2 MB (includes HTTP server) |
| `svm` (C/C++, with POCO) | ~8 MB | ~1-2 MB (Sedona VM via FFI) |
| POCO .so libraries | ~15 MB total | 0 (eliminated) |
| **Total installed** | **~25 MB** | **~3-5 MB** |

The Rust binary is larger than the C `engine` alone because it includes the HTTP server and Haystack stack. However, it eliminates the POCO shared libraries entirely, resulting in a net reduction.

---

## 12. Side-by-Side: Current vs Rust Build Flow

### Current Docker Build Flow

```
Developer Machine                          Docker Container
─────────────────                          ─────────────────
1. ./build_and_extract_fixed.sh
   ├── Check FORCE_REBUILD/FORCE_CODEUPDATE
   ├── If FORCE_REBUILD:
   │   └── docker build (Dockerfile)    ──>  Install Debian 11
   │                                          Install GCC 10 ARM
   │                                          Install Boost ARM
   │                                          Install CMake 3.27
   │                                          Copy source code
   │                                          (~10 minutes)
   │
   ├── If FORCE_CODEUPDATE:
   │   ├── docker run + docker exec      ──>  Delete old source
   │   ├── docker cp source              ──>  Copy new source
   │   ├── docker export | docker import      Flatten layers
   │   └── docker image prune                 (~2 minutes)
   │
   ├── docker run BUILD_CMD              ──>  cd arm-build
   │                                          cmake -DCMAKE_TOOLCHAIN_FILE=armv7l.cmake
   │                                          make -j7 package
   │                                          make cppcheck (optional)
   │                                          (~5-10 minutes)
   │
   ├── Wait for container exit
   ├── docker cp /workspace/arm-build    ──>  Extract build dir
   ├── find ... -name "*.deb"                 Find .deb file
   ├── docker cp build.log                    Extract logs
   ├── docker cp cppcheck.log                 Extract analysis
   ├── docker rm container                    Cleanup
   └── Output: sandstar-*.deb
```

### Rust Build Flow

```
Developer Machine
─────────────────
1. cargo build --target armv7-unknown-linux-gnueabihf --release
   │
   ├── Cargo resolves dependencies (cached after first build)
   ├── cc crate compiles Sedona VM C code
   ├── rustc compiles Rust code
   ├── Linker produces ARM binary
   └── Output: target/armv7-unknown-linux-gnueabihf/release/sandstar
       (~1-3 minutes, <30 seconds incremental)

2. cargo clippy --all-targets
   └── Static analysis output (instant)

3. cargo deb --target armv7-unknown-linux-gnueabihf --no-build
   └── Output: target/.../debian/sandstar_0.1.0_armhf.deb
       (~5 seconds)

4. Total: ~2-4 minutes (first build), <30 seconds (incremental)
```

### Build Time Comparison

| Operation | Current (Docker + CMake) | Rust (Cargo) | Speedup |
|---|---|---|---|
| First build (cold) | ~15 minutes | ~3-5 minutes | 3-5x |
| Code change (incremental) | ~5-10 minutes | ~10-30 seconds | 20-60x |
| Docker image rebuild | ~10 minutes | N/A | Eliminated |
| Code update (`FORCE_CODEUPDATE`) | ~2 minutes | N/A | Eliminated |
| Static analysis | ~2-5 minutes (cppcheck) | ~5-10 seconds (clippy) | 20-30x |
| Package (.deb) | Included in build | ~5 seconds | Minimal |

### Key Advantages of Rust Build

1. **No Docker required for cross-compilation** (just `rustup target add`)
2. **Incremental compilation**: Only recompiles changed files (~seconds vs minutes)
3. **No source copy step**: Cargo builds from the project directory directly
4. **No layer management**: No Docker image flattening or `FORCE_CODEUPDATE` workaround
5. **Deterministic builds**: `Cargo.lock` pins exact dependency versions
6. **Offline builds**: `cargo vendor` downloads all dependencies for offline use
7. **Static analysis is instant**: Clippy runs in seconds, not minutes
8. **Single binary output**: No shared libraries to manage or extract

### Migration Command Cheat Sheet

```bash
# ─── Equivalents ─────────────────────────────────────────────
# Old: FORCE_CODEUPDATE=1 RUN_STATIC_ANALYSIS=0 ./build_and_extract_fixed.sh
# New:
cargo build --target armv7-unknown-linux-gnueabihf --release

# Old: FORCE_CODEUPDATE=1 RUN_STATIC_ANALYSIS=1 ./build_and_extract_fixed.sh
# New:
cargo build --target armv7-unknown-linux-gnueabihf --release && cargo clippy

# Old: FORCE_REBUILD=1 ./build_and_extract_fixed.sh
# New:
cargo clean && cargo build --target armv7-unknown-linux-gnueabihf --release

# Old: BUILD_MODE=native ./build_and_extract_fixed.sh
# New:
cargo build

# Old: grep "error:" cppcheck.log
# New:
cargo clippy 2>&1 | grep "^error"

# Old: /home/parallels/code/ssCompile/tools/installSandstar.sh BahaHost2Device
# New: (same script, just different .deb file)
/home/parallels/code/ssCompile/tools/installSandstar.sh BahaHost2Device
```
