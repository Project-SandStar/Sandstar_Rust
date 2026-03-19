#!/bin/bash
#
# build-sedona-sox.sh
#
# Builds Sedona SVM inside Docker with engine DISABLED in svm.properties,
# so only SOX (port 1876) is active for testing SOX communication.
#
# Output: Compiled kits, manifests, scode → SedonaRepo/<datetime>/
#
# Usage:
#   ./tools/build-sedona-sox.sh [--clean]
#
#   --clean   Remove Docker image and rebuild from scratch
#

set -euo pipefail

###############################################################################
# Paths
###############################################################################
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RUST_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
EACIO_DIR="$(cd "$RUST_ROOT/../shaystack/sandstar/sandstar/EacIo" && pwd)"
SEDONA_REPO="$RUST_ROOT/SedonaRepo"

# Create timestamped output directory
TIMESTAMP="$(date '+%Y-%m-%d_%H-%M-%S')"
OUTPUT_DIR="$SEDONA_REPO/$TIMESTAMP"

DOCKER_IMAGE="sedona-sox-builder"
DOCKER_TAG="latest"
CONTAINER_NAME="sedona-sox-build-$$"

###############################################################################
# Parse args
###############################################################################
CLEAN=0
for arg in "$@"; do
    case "$arg" in
        --clean) CLEAN=1 ;;
        *) echo "Unknown arg: $arg"; exit 1 ;;
    esac
done

###############################################################################
# Colors
###############################################################################
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

log()  { echo -e "${GREEN}[sedona-sox]${NC} $*"; }
warn() { echo -e "${YELLOW}[sedona-sox]${NC} $*"; }
err()  { echo -e "${RED}[sedona-sox]${NC} $*" >&2; }

###############################################################################
# Preflight checks
###############################################################################
if ! command -v docker &>/dev/null; then
    err "Docker is not installed or not in PATH"
    exit 1
fi

if [ ! -d "$EACIO_DIR" ]; then
    err "EacIo directory not found: $EACIO_DIR"
    exit 1
fi

if [ ! -f "$EACIO_DIR/bin/sedonac.sh" ]; then
    err "sedonac.sh not found in $EACIO_DIR/bin/"
    exit 1
fi

log "EacIo source:  $EACIO_DIR"
log "Output dir:    $OUTPUT_DIR"
log "Timestamp:     $TIMESTAMP"

###############################################################################
# Clean if requested
###############################################################################
if [ "$CLEAN" -eq 1 ]; then
    warn "Removing Docker image $DOCKER_IMAGE:$DOCKER_TAG"
    docker rmi "$DOCKER_IMAGE:$DOCKER_TAG" 2>/dev/null || true
fi

###############################################################################
# Create Dockerfile (in-memory via heredoc)
###############################################################################
DOCKER_BUILD_DIR="$(mktemp -d)"
trap 'rm -rf "$DOCKER_BUILD_DIR"; rm -rf "${WORK_DIR:-}"' EXIT

cat > "$DOCKER_BUILD_DIR/Dockerfile" <<'DOCKERFILE'
FROM eclipse-temurin:17-jdk-jammy

RUN apt-get update && apt-get install -y --no-install-recommends \
    bash \
    coreutils \
    sed \
    && rm -rf /var/lib/apt/lists/*

# Sedona home will be mounted at /sedona
WORKDIR /sedona

# Build script that:
# 1. Patches svm.properties to disable engine
# 2. Compiles all kits via sedonac
# 3. Compiles kits.scode
# 4. Compiles app.sab
# 5. Copies output to /output
COPY build-inside.sh /build-inside.sh
RUN chmod +x /build-inside.sh

ENTRYPOINT ["/build-inside.sh"]
DOCKERFILE

###############################################################################
# Create the in-container build script
###############################################################################
cat > "$DOCKER_BUILD_DIR/build-inside.sh" <<'BUILD_SCRIPT'
#!/bin/bash
set -euo pipefail

SEDONA_HOME="/sedona"
OUTPUT="/output"
export SEDONA_HOME

echo "=== Sedona SOX Builder ==="
echo "SEDONA_HOME=$SEDONA_HOME"
echo ""

#---------------------------------------------------------------------------
# 1. Patch svm.properties: disable engine, keep sedona + haystack
#---------------------------------------------------------------------------
SVM_PROPS="$SEDONA_HOME/svm.properties"
if [ -f "$SVM_PROPS" ]; then
    echo "[1/5] Patching svm.properties (engine=false for SOX-only mode)"
    sed -i 's/^engine=true/engine=false/' "$SVM_PROPS"
    echo "  engine=$(grep '^engine=' "$SVM_PROPS" | cut -d= -f2)"
    echo "  sedona=$(grep '^sedona=' "$SVM_PROPS" | cut -d= -f2)"
    echo "  haystack=$(grep '^haystack=' "$SVM_PROPS" | cut -d= -f2)"
else
    echo "[1/5] WARNING: svm.properties not found, creating minimal config"
    cat > "$SVM_PROPS" <<EOF
engine=false
sedona=true
haystack=true
listen=0.0.0.0
port=8085
level=DEBUG
filter=ALL
EOF
fi

#---------------------------------------------------------------------------
# 2. Compile each kit from source (if src/ exists)
#---------------------------------------------------------------------------
echo ""
echo "[2/5] Compiling Sedona kits..."

SEDONAC="$SEDONA_HOME/bin/sedonac.sh"
if [ ! -f "$SEDONAC" ]; then
    echo "ERROR: sedonac.sh not found at $SEDONAC"
    exit 1
fi
chmod +x "$SEDONAC"

# Compile kits in topological dependency order.
# - sys must be first (root dependency for everything)
# - inet before sox, web (they depend on inet)
# - sox before web, shaystack (they depend on sox)
# - EacIo before shaystack (shaystack depends on EacIo)
# - platUnix/platWin32/AnkaLabs excluded: precompiled only, source at non-standard paths
KIT_ORDER=(
    sys
    inet
    sox
    soxcert
    datetime
    datetimeStd
    driver
    types
    timing
    func
    math
    logic
    serial
    pstore
    logManager
    control
    pricomp
    hvac
    basicSchedule
    web
    EacIo
    shaystack
)

# Critical kits — abort build if these fail
CRITICAL_KITS="sys inet sox"

COMPILED=0
SKIPPED=0
FAILED=0

for kit in "${KIT_ORDER[@]}"; do
    KIT_XML="$SEDONA_HOME/src/$kit/kit.xml"
    if [ -f "$KIT_XML" ]; then
        echo -n "  Compiling $kit... "
        if bash "$SEDONAC" "$KIT_XML" > "/tmp/sedonac_${kit}.log" 2>&1; then
            echo "OK"
            COMPILED=$((COMPILED + 1))
        else
            echo "FAILED"
            cat "/tmp/sedonac_${kit}.log" | tail -5
            FAILED=$((FAILED + 1))
            # Abort on critical kit failure
            if echo "$CRITICAL_KITS" | grep -qw "$kit"; then
                echo "  FATAL: critical kit '$kit' failed to compile — aborting"
                exit 1
            fi
        fi
    else
        echo "  Skipping $kit (no source at src/$kit/kit.xml)"
        SKIPPED=$((SKIPPED + 1))
    fi
done

echo "  Compiled: $COMPILED, Skipped: $SKIPPED, Failed: $FAILED"

#---------------------------------------------------------------------------
# 3. Compile kits.scode (the merged scode image)
#---------------------------------------------------------------------------
echo ""
echo "[3/5] Compiling kits.scode..."

KITS_XML="$SEDONA_HOME/EacIoApp/kits.xml"
if [ -f "$KITS_XML" ]; then
    if bash "$SEDONAC" "$KITS_XML" > /tmp/sedonac_scode.log 2>&1; then
        echo "  kits.scode compiled successfully"
    else
        echo "  FATAL: kits.scode compilation failed"
        cat /tmp/sedonac_scode.log | tail -10
        exit 1
    fi
else
    echo "  FATAL: kits.xml not found at $KITS_XML"
    exit 1
fi

#---------------------------------------------------------------------------
# 4. Compile app.sab
#---------------------------------------------------------------------------
echo ""
echo "[4/5] Compiling app.sab..."

APP_SAX="$SEDONA_HOME/EacIoApp/app.sax"
if [ -f "$APP_SAX" ]; then
    if bash "$SEDONAC" "$APP_SAX" > /tmp/sedonac_app.log 2>&1; then
        echo "  app.sab compiled successfully"
    else
        echo "  WARNING: app.sab compilation failed"
        cat /tmp/sedonac_app.log | tail -10
    fi
else
    echo "  WARNING: app.sax not found"
fi

#---------------------------------------------------------------------------
# 5. Collect output
#---------------------------------------------------------------------------
echo ""
echo "[5/5] Collecting build artifacts to /output..."

mkdir -p "$OUTPUT/kits"
mkdir -p "$OUTPUT/manifests"
mkdir -p "$OUTPUT/scode"
mkdir -p "$OUTPUT/app"
mkdir -p "$OUTPUT/config"
mkdir -p "$OUTPUT/lib"
mkdir -p "$OUTPUT/bin"

# Copy compiled kits
if [ -d "$SEDONA_HOME/kits" ]; then
    cp -r "$SEDONA_HOME/kits/"* "$OUTPUT/kits/" 2>/dev/null || true
    echo "  Kits: $(ls "$OUTPUT/kits/" 2>/dev/null | wc -l) directories"
fi

# Copy manifests
if [ -d "$SEDONA_HOME/manifests" ]; then
    cp -r "$SEDONA_HOME/manifests/"* "$OUTPUT/manifests/" 2>/dev/null || true
    echo "  Manifests: $(ls "$OUTPUT/manifests/" 2>/dev/null | wc -l) directories"
fi

# Copy scode files
if [ -d "$SEDONA_HOME/scode" ]; then
    cp -r "$SEDONA_HOME/scode/"* "$OUTPUT/scode/" 2>/dev/null || true
fi

# Copy compiled app
if [ -f "$SEDONA_HOME/EacIoApp/kits.scode" ]; then
    cp "$SEDONA_HOME/EacIoApp/kits.scode" "$OUTPUT/app/"
    echo "  kits.scode: $(wc -c < "$SEDONA_HOME/EacIoApp/kits.scode") bytes"
fi
if [ -f "$SEDONA_HOME/EacIoApp/app.sab" ]; then
    cp "$SEDONA_HOME/EacIoApp/app.sab" "$OUTPUT/app/"
    echo "  app.sab: $(wc -c < "$SEDONA_HOME/EacIoApp/app.sab") bytes"
fi
if [ -f "$SEDONA_HOME/EacIoApp/kits.xml" ]; then
    cp "$SEDONA_HOME/EacIoApp/kits.xml" "$OUTPUT/app/"
fi
if [ -f "$SEDONA_HOME/EacIoApp/app.sax" ]; then
    cp "$SEDONA_HOME/EacIoApp/app.sax" "$OUTPUT/app/"
fi

# Copy patched svm.properties
cp "$SEDONA_HOME/svm.properties" "$OUTPUT/config/svm.properties"

# Copy sedonac tools (for reference)
cp -r "$SEDONA_HOME/lib/"* "$OUTPUT/lib/" 2>/dev/null || true
cp "$SEDONA_HOME/bin/sedonac.sh" "$OUTPUT/bin/" 2>/dev/null || true

# Collect build logs (before container exits)
mkdir -p "$OUTPUT/logs"
cp /tmp/sedonac_*.log "$OUTPUT/logs/" 2>/dev/null || true
echo "  Build logs: $(ls "$OUTPUT/logs/" 2>/dev/null | wc -l) files"

# Build info
cat > "$OUTPUT/build-info.txt" <<EOF
Sedona SOX Build
================
Date: $(date -u '+%Y-%m-%dT%H:%M:%SZ')
Purpose: SOX communication testing (engine=false)
SOX Port: 1876 (default)
Haystack Port: 8085
Engine: DISABLED
Sedona VM: ENABLED
Kits Compiled: $COMPILED
Kits Skipped: $SKIPPED
Kits Failed: $FAILED
EOF

echo ""
echo "=== Build Complete ==="
echo "SOX port: 1876 (SoxService in app.sax)"
echo "Engine: DISABLED (svm.properties engine=false)"
echo "Sedona VM: ENABLED"
echo "Output: /output"
BUILD_SCRIPT

###############################################################################
# Build Docker image
###############################################################################
log "Building Docker image: $DOCKER_IMAGE:$DOCKER_TAG"
docker build -t "$DOCKER_IMAGE:$DOCKER_TAG" "$DOCKER_BUILD_DIR" 2>&1

###############################################################################
# Create output directory
###############################################################################
mkdir -p "$OUTPUT_DIR"

###############################################################################
# Run the build container
###############################################################################
log "Running Sedona build in Docker..."
log "  Mounting EacIo as /sedona (read-write copy)"
log "  Output → $OUTPUT_DIR"

# We mount a COPY of EacIo to avoid modifying the original svm.properties
WORK_DIR="$(mktemp -d)"
cp -r "$EACIO_DIR/"* "$WORK_DIR/"

# Convert MSYS/Git Bash paths to Windows paths for Docker volume mounts
to_win_path() {
    local p="$1"
    # Convert /c/... to C:/... or /tmp/... to full Windows path
    if command -v cygpath &>/dev/null; then
        cygpath -w "$p"
    else
        echo "$p" | sed 's|^/\([a-zA-Z]\)/|\1:/|'
    fi
}

WORK_DIR_WIN="$(to_win_path "$WORK_DIR")"
OUTPUT_DIR_WIN="$(to_win_path "$OUTPUT_DIR")"

log "  WORK_DIR (Docker mount): $WORK_DIR_WIN"
log "  OUTPUT_DIR (Docker mount): $OUTPUT_DIR_WIN"

# MSYS_NO_PATHCONV prevents Git Bash from mangling the container paths
MSYS_NO_PATHCONV=1 docker run --rm \
    --name "$CONTAINER_NAME" \
    -v "$WORK_DIR_WIN:/sedona" \
    -v "$OUTPUT_DIR_WIN:/output" \
    "$DOCKER_IMAGE:$DOCKER_TAG"

BUILD_EXIT=$?

# Cleanup work dir
rm -rf "$WORK_DIR"

if [ $BUILD_EXIT -ne 0 ]; then
    err "Docker build failed with exit code $BUILD_EXIT"
    exit $BUILD_EXIT
fi

###############################################################################
# Summary
###############################################################################
echo ""
log "=========================================="
log " Sedona SOX Build Complete"
log "=========================================="
log ""
log "Output directory: $OUTPUT_DIR"
log ""
log "Contents:"
ls -la "$OUTPUT_DIR/" 2>/dev/null || true
echo ""

if [ -f "$OUTPUT_DIR/build-info.txt" ]; then
    cat "$OUTPUT_DIR/build-info.txt"
fi

echo ""
log "To test SOX communication:"
log "  1. Deploy kits.scode to device:"
log "     scp $OUTPUT_DIR/app/kits.scode eacio@172.28.211.135:/home/eacio/sandstar/data/"
log ""
log "  2. Deploy patched svm.properties:"
log "     scp $OUTPUT_DIR/config/svm.properties eacio@172.28.211.135:/home/eacio/sandstar/data/"
log ""
log "  3. Restart sandstar on device:"
log "     ssh -p 1919 eacio@172.28.211.135 'sudo systemctl restart sandstar'"
log ""
log "  4. Test SOX connection (port 1876):"
log "     # From Java client:"
log "     java -cp 'sedona.jar:sedonac.jar' sedona.sox.Main -u admin -p <pass> <device-ip>"
log ""
log "  5. Or run with Sandstar Rust (locally):"
log "     cargo run -p sandstar-server -- --sedona --scode-path $OUTPUT_DIR/app/kits.scode"
log ""
