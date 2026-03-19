#!/bin/bash
# Build Sandstar Rust — cross-compile for BeagleBone ARM or native dev builds
# Replaces the 303-line C build_and_extract_fixed.sh with a ~120-line Rust equivalent
#
# Usage:
#   ./tools/build-rust.sh              # ARM cross-compile (default)
#   BUILD_MODE=native ./tools/build-rust.sh   # Native build with mock-hal
#   BUILD_MODE=cross  ./tools/build-rust.sh   # Docker cross-compile via `cross`
#   ./tools/build-rust.sh --test       # Also run tests

set -euo pipefail

# ── Colors ────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'

info()  { echo -e "${CYAN}[INFO]${NC}  $*"; }
ok()    { echo -e "${GREEN}[OK]${NC}    $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
die()   { echo -e "${RED}[ERROR]${NC} $*" >&2; exit 1; }
step()  { echo -e "\n${BOLD}── $* ──${NC}"; }

# ── Configuration ─────────────────────────────────────────────
BUILD_MODE="${BUILD_MODE:-arm}"
RUN_TESTS=0

ARM_TARGET="armv7-unknown-linux-gnueabihf"
PACKAGES="-p sandstar-server -p sandstar-cli"
BINARIES=("sandstar-engine-server" "sandstar-cli")

# Parse flags
for arg in "$@"; do
    case "$arg" in
        --test) RUN_TESTS=1 ;;
        --help|-h)
            echo "Usage: BUILD_MODE=[arm|native|cross] $0 [--test]"
            echo ""
            echo "Build modes:"
            echo "  arm     Cross-compile for BeagleBone (default)"
            echo "  native  Build for host with mock-hal"
            echo "  cross   Use 'cross' tool (Docker) for ARM build"
            echo ""
            echo "Flags:"
            echo "  --test  Also run the test suite"
            exit 0
            ;;
        *) die "Unknown flag: $arg (use --help)" ;;
    esac
done

# ── Resolve workspace root (script lives in tools/) ──────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$WORKSPACE"

# ── Print banner ──────────────────────────────────────────────
echo "=========================================="
echo -e " ${BOLD}Sandstar Rust Build${NC}"
echo "  Mode:       $BUILD_MODE"
echo "  Tests:      $([ "$RUN_TESTS" -eq 1 ] && echo 'yes' || echo 'no')"
echo "  Workspace:  $WORKSPACE"
echo "=========================================="

# ── Validate toolchain ───────────────────────────────────────
command -v cargo >/dev/null 2>&1 || die "cargo not found. Install Rust: https://rustup.rs"

if [ "$BUILD_MODE" = "arm" ]; then
    rustup target list --installed 2>/dev/null | grep -q "$ARM_TARGET" \
        || die "ARM target not installed. Run: rustup target add $ARM_TARGET"
    command -v arm-linux-gnueabihf-gcc >/dev/null 2>&1 \
        || die "ARM cross-linker not found. Install: apt install gcc-arm-linux-gnueabihf"
fi

if [ "$BUILD_MODE" = "cross" ]; then
    command -v cross >/dev/null 2>&1 \
        || die "'cross' not found. Install: cargo install cross"
    command -v docker >/dev/null 2>&1 \
        || die "Docker not found. 'cross' requires Docker."
fi

# ── Set build flags per mode ─────────────────────────────────
case "$BUILD_MODE" in
    arm)
        BUILD_CMD="cargo"
        BUILD_ARGS="build --target $ARM_TARGET --release --no-default-features --features linux-hal $PACKAGES"
        CLIPPY_ARGS="--target $ARM_TARGET --no-default-features --features linux-hal $PACKAGES -- -W clippy::all"
        TARGET_DIR="target/$ARM_TARGET/release"
        ;;
    native)
        BUILD_CMD="cargo"
        BUILD_ARGS="build --release $PACKAGES"
        CLIPPY_ARGS="--all-targets -- -W clippy::all"
        TARGET_DIR="target/release"
        ;;
    cross)
        BUILD_CMD="cross"
        BUILD_ARGS="build --target $ARM_TARGET --release --no-default-features --features linux-hal $PACKAGES"
        CLIPPY_ARGS="--target $ARM_TARGET --no-default-features --features linux-hal $PACKAGES -- -W clippy::all"
        TARGET_DIR="target/$ARM_TARGET/release"
        ;;
    *)
        die "Unknown BUILD_MODE: $BUILD_MODE (expected: arm, native, cross)"
        ;;
esac

# ── Step 1: Clippy ────────────────────────────────────────────
step "Static Analysis (clippy)"
if [ "$BUILD_MODE" = "cross" ]; then
    warn "Skipping clippy in cross mode (runs inside Docker; use arm or native for lint)"
else
    cargo clippy $CLIPPY_ARGS
    ok "Clippy passed"
fi

# ── Step 2: Build ─────────────────────────────────────────────
step "Building ($BUILD_MODE)"
$BUILD_CMD $BUILD_ARGS
ok "Build complete"

# ── Step 3: Tests (optional) ─────────────────────────────────
if [ "$RUN_TESTS" -eq 1 ]; then
    step "Running Tests"
    if [ "$BUILD_MODE" = "arm" ]; then
        warn "Cannot run ARM tests on host — running native tests instead"
        cargo test --workspace
    elif [ "$BUILD_MODE" = "cross" ]; then
        warn "Cannot run tests in cross mode — running native tests instead"
        cargo test --workspace
    else
        cargo test --workspace
    fi
    ok "All tests passed"
fi

# ── Step 4: Package .deb (ARM builds only) ───────────────────
if [ "$BUILD_MODE" = "arm" ] || [ "$BUILD_MODE" = "cross" ]; then
    step "Packaging .deb"
    if command -v cargo-deb >/dev/null 2>&1; then
        cargo deb --target "$ARM_TARGET" --no-build -p sandstar-server --variant linux-hal
        DEB_FILE=$(ls -t target/"$ARM_TARGET"/debian/*.deb 2>/dev/null | head -1)
        if [ -n "$DEB_FILE" ]; then
            ok "Package: $DEB_FILE ($(du -h "$DEB_FILE" | cut -f1))"
        fi
    else
        warn "cargo-deb not installed — skipping .deb generation"
        warn "Install: cargo install cargo-deb"
    fi
fi

# ── Step 5: Binary sizes ─────────────────────────────────────
step "Binary Sizes"
for bin in "${BINARIES[@]}"; do
    BIN_PATH="$TARGET_DIR/$bin"
    if [ -f "$BIN_PATH" ]; then
        SIZE=$(du -h "$BIN_PATH" | cut -f1)
        echo -e "  ${GREEN}$bin${NC}  $SIZE"
    else
        warn "$bin not found at $BIN_PATH"
    fi
done

# ── Done ──────────────────────────────────────────────────────
echo ""
ok "Build finished successfully ($BUILD_MODE mode)"

if [ "$BUILD_MODE" = "arm" ] || [ "$BUILD_MODE" = "cross" ]; then
    echo ""
    info "Deploy: /home/parallels/code/ssCompile/tools/installSandstar.sh BahaHost2Device"
fi
