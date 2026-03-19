#!/usr/bin/env bash
# start.sh — Start the Sandstar Rust engine server
#
# Usage:
#   ./start.sh                     # Demo mode (mock HAL, 5 channels)
#   ./start.sh --config            # Real EacIo config (140 channels)
#   ./start.sh --sedona            # Sedona VM mode
#   ./start.sh -- --http-port 9090 # Pass extra args to server
#
# Environment variables:
#   SANDSTAR_BUILD=1               # Build before starting (default: 0)
#   SANDSTAR_LOG_LEVEL=debug       # Override log level (default: info)
#   SANDSTAR_HTTP_PORT=8085        # Override HTTP port
#   SANDSTAR_HTTP_BIND=0.0.0.0    # Override bind address

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PID_FILE="$ROOT_DIR/.sandstar.pid"
LOG_DIR="$ROOT_DIR/logs"
LOG_FILE="$LOG_DIR/sandstar-$(date +%Y%m%d_%H%M%S).log"
EACIO_CONFIG="$ROOT_DIR/../shaystack/sandstar/sandstar/EacIo"

# --- Helpers ---

die()  { echo "ERROR: $*" >&2; exit 1; }
info() { echo ":: $*"; }

is_running() {
    if [[ -f "$PID_FILE" ]]; then
        local pid
        pid=$(<"$PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            return 0
        fi
        # Stale PID file — clean up
        rm -f "$PID_FILE"
    fi
    return 1
}

# --- Pre-flight ---

if is_running; then
    pid=$(<"$PID_FILE")
    die "Already running (PID $pid). Use stop.sh first, or restart.sh."
fi

mkdir -p "$LOG_DIR"

# --- Parse arguments ---

MODE="demo"
EXTRA_ARGS=()
PASSTHROUGH=false

for arg in "$@"; do
    if $PASSTHROUGH; then
        EXTRA_ARGS+=("$arg")
        continue
    fi
    case "$arg" in
        --config)  MODE="config" ;;
        --sedona)  MODE="sedona" ;;
        --)        PASSTHROUGH=true ;;
        -h|--help)
            echo "Usage: $0 [--config|--sedona] [-- <extra server args>]"
            echo ""
            echo "Modes:"
            echo "  (default)   Demo mode — mock HAL, 5 demo channels"
            echo "  --config    EacIo config — real config from $EACIO_CONFIG"
            echo "  --sedona    Sedona VM — requires scode-path"
            echo ""
            echo "Environment:"
            echo "  SANDSTAR_BUILD=1          Build before starting"
            echo "  SANDSTAR_LOG_LEVEL=debug  Override log level"
            echo "  SANDSTAR_HTTP_PORT=8085   Override HTTP port"
            echo "  SANDSTAR_HTTP_BIND=0.0.0.0  Bind to all interfaces"
            exit 0
            ;;
        *)         die "Unknown argument: $arg (use -- to pass args to server)" ;;
    esac
done

# --- Optional build ---

if [[ "${SANDSTAR_BUILD:-0}" == "1" ]]; then
    info "Building sandstar-server..."
    (cd "$ROOT_DIR" && cargo build -p sandstar-server 2>&1) || die "Build failed"
    info "Build complete."
fi

# --- Assemble server arguments ---

SERVER_ARGS=()

case "$MODE" in
    demo)
        info "Starting in DEMO mode (mock HAL)"
        ;;
    config)
        if [[ ! -d "$EACIO_CONFIG" ]]; then
            die "Config directory not found: $EACIO_CONFIG"
        fi
        SERVER_ARGS+=(--config-dir "$EACIO_CONFIG")
        info "Starting with EacIo config: $EACIO_CONFIG"
        ;;
    sedona)
        if [[ ! -d "$EACIO_CONFIG" ]]; then
            die "Config directory not found: $EACIO_CONFIG"
        fi
        SERVER_ARGS+=(--config-dir "$EACIO_CONFIG" --sedona)
        if [[ -n "${SANDSTAR_SCODE_PATH:-}" ]]; then
            SERVER_ARGS+=(--scode-path "$SANDSTAR_SCODE_PATH")
        fi
        info "Starting with Sedona VM"
        ;;
esac

# Apply environment overrides
if [[ -n "${SANDSTAR_LOG_LEVEL:-}" ]]; then
    SERVER_ARGS+=(--log-level "$SANDSTAR_LOG_LEVEL")
fi
if [[ -n "${SANDSTAR_HTTP_PORT:-}" ]]; then
    SERVER_ARGS+=(--http-port "$SANDSTAR_HTTP_PORT")
fi
if [[ -n "${SANDSTAR_HTTP_BIND:-}" ]]; then
    SERVER_ARGS+=(--http-bind "$SANDSTAR_HTTP_BIND")
fi

# Log to file
SERVER_ARGS+=(--log-file "$LOG_FILE")

# Append any passthrough args
SERVER_ARGS+=("${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}")

# --- Start server ---

info "Log file: $LOG_FILE"

cd "$ROOT_DIR"
cargo run -p sandstar-server -- "${SERVER_ARGS[@]}" >> "$LOG_FILE" 2>&1 &
SERVER_PID=$!

echo "$SERVER_PID" > "$PID_FILE"
info "Server started (PID $SERVER_PID)"

# Wait briefly and verify it's still alive
sleep 2
if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    rm -f "$PID_FILE"
    echo ""
    echo "--- Last 20 lines of log ---"
    tail -20 "$LOG_FILE" 2>/dev/null || true
    die "Server exited immediately. Check log: $LOG_FILE"
fi

# Show first few lines of startup
info "Startup log (tail -f $LOG_FILE to follow):"
echo "---"
head -20 "$LOG_FILE" 2>/dev/null || true
echo "---"
info "Server is running. Use stop.sh to shut down."
