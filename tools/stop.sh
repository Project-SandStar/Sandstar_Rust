#!/usr/bin/env bash
# stop.sh — Gracefully stop the Sandstar Rust engine server
#
# Shutdown sequence:
#   1. CLI shutdown command via IPC (graceful, flushes state)
#   2. SIGTERM (if CLI fails after 5s)
#   3. Force kill (if SIGTERM fails after 5s)
#
# Usage:
#   ./stop.sh            # Normal graceful shutdown
#   ./stop.sh --force    # Skip CLI, go straight to kill

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PID_FILE="$ROOT_DIR/.sandstar.pid"

CLI_TIMEOUT=5
TERM_TIMEOUT=5
FORCE=false

# --- Helpers ---

# macOS doesn't have `timeout` by default; use perl fallback
if ! command -v timeout &>/dev/null; then
    timeout() {
        local secs=$1; shift
        perl -e "alarm $secs; exec @ARGV" -- "$@"
    }
fi

die()  { echo "ERROR: $*" >&2; exit 1; }
info() { echo ":: $*"; }

is_alive() { kill -0 "$1" 2>/dev/null; }

wait_for_exit() {
    local pid=$1 timeout=$2 elapsed=0
    while is_alive "$pid" && (( elapsed < timeout )); do
        sleep 1
        (( elapsed++ ))
    done
    ! is_alive "$pid"
}

# --- Parse arguments ---

for arg in "$@"; do
    case "$arg" in
        --force|-f) FORCE=true ;;
        -h|--help)
            echo "Usage: $0 [--force]"
            echo ""
            echo "  --force, -f    Skip graceful CLI shutdown, kill immediately"
            exit 0
            ;;
        *) die "Unknown argument: $arg" ;;
    esac
done

# --- Find the process ---

if [[ ! -f "$PID_FILE" ]]; then
    # Try to find it by process name as fallback
    FOUND_PID=$(pgrep -f "sandstar-engine-server" 2>/dev/null | head -1 || true)
    if [[ -z "$FOUND_PID" ]]; then
        info "Server is not running (no PID file, no matching process)."
        exit 0
    fi
    info "No PID file, but found process $FOUND_PID"
    PID="$FOUND_PID"
else
    PID=$(<"$PID_FILE")
    if ! is_alive "$PID"; then
        info "Server is not running (stale PID file for $PID). Cleaning up."
        rm -f "$PID_FILE"
        exit 0
    fi
fi

info "Stopping server (PID $PID)..."

# --- Step 1: Graceful shutdown via CLI ---

if ! $FORCE; then
    info "Sending shutdown command via CLI..."
    if (cd "$ROOT_DIR" && timeout "$CLI_TIMEOUT" cargo run -p sandstar-cli -- shutdown 2>/dev/null); then
        # CLI accepted the command — wait for process to exit
        if wait_for_exit "$PID" "$CLI_TIMEOUT"; then
            rm -f "$PID_FILE"
            info "Server stopped gracefully."
            exit 0
        fi
        info "CLI shutdown accepted but process still alive, escalating..."
    else
        info "CLI shutdown failed or timed out, escalating..."
    fi
fi

# --- Step 2: SIGTERM ---

info "Sending SIGTERM..."
kill "$PID" 2>/dev/null || true

if wait_for_exit "$PID" "$TERM_TIMEOUT"; then
    rm -f "$PID_FILE"
    info "Server stopped (SIGTERM)."
    exit 0
fi

# --- Step 3: Force kill ---

info "Process still alive after SIGTERM. Force killing..."

# Windows: taskkill /F, Unix: kill -9
if command -v taskkill &>/dev/null; then
    taskkill //F //PID "$PID" 2>/dev/null || true
else
    kill -9 "$PID" 2>/dev/null || true
fi

sleep 1
rm -f "$PID_FILE"

if is_alive "$PID"; then
    die "Failed to kill process $PID"
fi

info "Server killed (force)."
