#!/usr/bin/env bash
# sim.sh — One-command simulation launcher for Sandstar + BASemulator
#
# Modes:
#   ./sim.sh                    # Demo: 5 channels, bridge to BASemulator
#   ./sim.sh --config           # EacIo: 140 channels + PID control loop
#   ./sim.sh --no-bridge        # Server only (no BASemulator bridge)
#   ./sim.sh --scenario         # Inject cooling scenario (no BASemulator)
#   ./sim.sh --test             # Quick smoke test: inject, read, verify
#
# Examples:
#   ./sim.sh                    # Start everything, Ctrl+C to stop
#   ./sim.sh --config           # Full PID loop with BASemulator
#   ./sim.sh --test             # Quick self-test (no BASemulator needed)
#   ./sim.sh --no-bridge -v     # Server only with verbose logging
#
# Requirements:
#   - Rust/Cargo installed
#   - Python 3 with 'requests' library (for bridge/scenario modes)
#   - BASemulator running on localhost:5001 (for bridge modes)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PID_FILE="$ROOT_DIR/.sandstar-sim.pid"
LOG_DIR="$ROOT_DIR/logs"
EACIO_CONFIG="$ROOT_DIR/../shaystack/sandstar/sandstar/EacIo"
CONTROL_TOML="$ROOT_DIR/examples/control_sim.toml"
MAPPING_FILE="$SCRIPT_DIR/basemulator-mapping.json"
SCENARIO_FILE="$ROOT_DIR/examples/scenario_cooling.json"

# Defaults
MODE="demo"
BRIDGE=true
SCENARIO=false
SELFTEST=false
BUILD=false
VERBOSE=false
ONCE=false
INTERVAL=""
HTTP_PORT="${SANDSTAR_HTTP_PORT:-8085}"
BAS_URL="${BAS_URL:-http://localhost:5001}"

# Colors (if terminal supports it)
if [[ -t 1 ]]; then
    GREEN='\033[0;32m'
    YELLOW='\033[0;33m'
    RED='\033[0;31m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    GREEN=''; YELLOW=''; RED=''; CYAN=''; BOLD=''; NC=''
fi

# ── Helpers ──────────────────────────────────────────────────

die()  { echo -e "${RED}ERROR:${NC} $*" >&2; exit 1; }
info() { echo -e "${GREEN}::${NC} $*"; }
warn() { echo -e "${YELLOW}!!${NC} $*"; }
step() { echo -e "${CYAN}▸${NC} ${BOLD}$*${NC}"; }

SELFTEST_DONE=false

cleanup() {
    set +e  # Don't exit on errors during cleanup
    echo ""
    step "Shutting down..."

    # Stop the bridge (if running)
    if [[ -n "${BRIDGE_PID:-}" ]] && kill -0 "$BRIDGE_PID" 2>/dev/null; then
        info "Stopping bridge (PID $BRIDGE_PID)..."
        kill "$BRIDGE_PID" 2>/dev/null
        wait "$BRIDGE_PID" 2>/dev/null
    fi

    # Stop the server
    if [[ -f "$PID_FILE" ]]; then
        local pid
        pid=$(<"$PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            info "Stopping server (PID $pid)..."
            kill "$pid" 2>/dev/null
            # Wait up to 5 seconds
            local elapsed=0
            while kill -0 "$pid" 2>/dev/null && (( elapsed < 5 )); do
                sleep 1
                elapsed=$((elapsed + 1))
            done
            # Force kill if still alive
            if kill -0 "$pid" 2>/dev/null; then
                if command -v taskkill &>/dev/null; then
                    taskkill //F //PID "$pid" 2>/dev/null
                else
                    kill -9 "$pid" 2>/dev/null
                fi
            fi
        fi
        rm -f "$PID_FILE"
    fi

    info "Simulation stopped."
    exit 0
}

wait_for_server() {
    local url="http://127.0.0.1:${HTTP_PORT}/health"
    local timeout=30
    local elapsed=0
    while (( elapsed < timeout )); do
        if curl -sf "$url" > /dev/null 2>&1; then
            return 0
        fi
        sleep 1
        (( elapsed++ ))
    done
    return 1
}

check_basemulator() {
    curl -sf -u admin:admin -m 3 \
        -X POST "$BAS_URL/cgi-bin/xml-cgi" \
        -H "Content-Type: application/xml" \
        -d '<rdom fcn="rpc" doc="rtd"><req>rd_unit</req><unit>0</unit></rdom>' \
        > /dev/null 2>&1
}

check_python() {
    local py
    py=$(get_python 2>/dev/null) || return 1
    # Check for requests library
    "$py" -c "import requests" 2>/dev/null || return 2
    return 0
}

get_python() {
    # Prefer python (no spaces in path on Windows) over python3
    if command -v python &>/dev/null; then
        command -v python
    elif command -v python3 &>/dev/null; then
        command -v python3
    else
        return 1
    fi
}

# ── Parse arguments ──────────────────────────────────────────

for arg in "$@"; do
    case "$arg" in
        --config)     MODE="config" ;;
        --no-bridge)  BRIDGE=false ;;
        --scenario)   SCENARIO=true; BRIDGE=false ;;
        --test)       SELFTEST=true; BRIDGE=false ;;
        --build)      BUILD=true ;;
        --once)       ONCE=true ;;
        --verbose|-v) VERBOSE=true ;;
        --interval=*) INTERVAL="${arg#*=}" ;;
        -h|--help)
            echo -e "${BOLD}sim.sh${NC} — Sandstar Simulation Launcher"
            echo ""
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Modes:"
            echo "  (default)      Demo mode (5 channels) + BASemulator bridge"
            echo "  --config       EacIo config (140 channels) + PID control + bridge"
            echo "  --no-bridge    Start server only (no BASemulator connection)"
            echo "  --scenario     Inject cooling scenario file (no BASemulator)"
            echo "  --test         Quick smoke test: inject → poll → verify"
            echo ""
            echo "Options:"
            echo "  --build        Build before starting (default: skip if binary exists)"
            echo "  --verbose, -v  Enable debug-level logging"
            echo "  --once         Run a single bridge cycle then exit"
            echo "  --interval=N   Bridge poll interval in seconds (default: 1.0)"
            echo "  -h, --help     Show this help"
            echo ""
            echo "Environment:"
            echo "  SANDSTAR_HTTP_PORT=8085   Override HTTP port"
            echo "  BAS_URL=http://...:5001   Override BASemulator URL"
            echo ""
            echo "Examples:"
            echo "  $0                    # Demo + bridge (most common)"
            echo "  $0 --config           # Full PID control loop"
            echo "  $0 --test             # Quick sanity check"
            echo "  $0 --config --no-bridge -v  # Server only, verbose"
            exit 0
            ;;
        *) die "Unknown argument: $arg (use -h for help)" ;;
    esac
done

# ── Pre-flight checks ───────────────────────────────────────

echo ""
echo -e "${BOLD}╔═══════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║     Sandstar Simulation Launcher          ║${NC}"
echo -e "${BOLD}╚═══════════════════════════════════════════╝${NC}"
echo ""

step "Pre-flight checks"

# Cargo
if ! command -v cargo &>/dev/null; then
    if [[ -f "$HOME/.cargo/bin/cargo" ]]; then
        export PATH="$HOME/.cargo/bin:$PATH"
    else
        die "Cargo not found. Install Rust: https://rustup.rs"
    fi
fi
info "Cargo: $(cargo --version 2>/dev/null | head -1)"

# Python (only needed for bridge/scenario)
if $BRIDGE || $SCENARIO; then
    if ! check_python; then
        ret=$?
        if (( ret == 1 )); then
            die "Python not found. Install Python 3: https://python.org"
        elif (( ret == 2 )); then
            die "Python 'requests' library not found. Run: pip install requests"
        fi
    fi
    PY_BIN=$(get_python)
info "Python: $("$PY_BIN" --version 2>&1)"
fi

# BASemulator (only for bridge mode)
if $BRIDGE; then
    if check_basemulator; then
        info "BASemulator: OK ($BAS_URL)"
    else
        warn "BASemulator not reachable at $BAS_URL"
        warn "Start BASemulator first, or use --no-bridge / --scenario"
        echo ""
        read -r -p "Continue without bridge? [y/N] " answer
        case "$answer" in
            y|Y) BRIDGE=false; warn "Continuing without bridge" ;;
            *)   exit 1 ;;
        esac
    fi
fi

# EacIo config (only for --config mode)
if [[ "$MODE" == "config" ]]; then
    if [[ ! -d "$EACIO_CONFIG" ]]; then
        die "EacIo config not found: $EACIO_CONFIG"
    fi
    if [[ ! -f "$CONTROL_TOML" ]]; then
        die "Control config not found: $CONTROL_TOML"
    fi
    info "EacIo config: $EACIO_CONFIG"
    info "Control TOML: $CONTROL_TOML"
fi

# Check if port is already in use
if curl -sf "http://127.0.0.1:${HTTP_PORT}/health" > /dev/null 2>&1; then
    die "Port $HTTP_PORT already in use (another Sandstar instance running?)"
fi

# ── Build ────────────────────────────────────────────────────

BINARY="$ROOT_DIR/target/debug/sandstar-engine-server.exe"
if [[ ! -f "$BINARY" ]] && [[ ! -f "${BINARY%.exe}" ]]; then
    BUILD=true
fi

if $BUILD; then
    step "Building sandstar-server (simulator-hal)..."
    (cd "$ROOT_DIR" && cargo build --no-default-features --features simulator-hal -p sandstar-server 2>&1) \
        || die "Build failed"
    info "Build complete."
else
    info "Binary exists, skipping build (use --build to force)"
fi

# ── Start server ─────────────────────────────────────────────

ENGINE_LOG_DIR="$LOG_DIR/engine"
BRIDGE_LOG_DIR="$LOG_DIR/bridge"
DATA_LOG_DIR="$LOG_DIR/data"
mkdir -p "$ENGINE_LOG_DIR" "$BRIDGE_LOG_DIR" "$DATA_LOG_DIR"
LOG_FILE="$ENGINE_LOG_DIR/engine-$(date +%Y%m%d_%H%M%S).log"

step "Starting Sandstar server"

SERVER_ARGS=()
SERVER_ARGS+=(--http-port "$HTTP_PORT")

case "$MODE" in
    demo)
        info "Mode: DEMO (18 channels + PID control, SimulatorHal)"
        # Demo mode now includes virtual channels + control config for full PID testing
        SERVER_ARGS+=(--control-config "$CONTROL_TOML")
        ;;
    config)
        info "Mode: CONFIG (EacIo 140 channels + PID control)"
        SERVER_ARGS+=(--config-dir "$EACIO_CONFIG")
        SERVER_ARGS+=(--control-config "$CONTROL_TOML")
        ;;
esac

SERVER_ARGS+=(--log-file "$LOG_FILE")

if $VERBOSE; then
    SERVER_ARGS+=(--log-level debug)
fi

# Register cleanup trap
trap cleanup EXIT INT TERM

cd "$ROOT_DIR"
cargo run -p sandstar-server --no-default-features --features simulator-hal -- \
    "${SERVER_ARGS[@]}" >> "$LOG_FILE" 2>&1 &
SERVER_PID=$!
echo "$SERVER_PID" > "$PID_FILE"

info "Server PID: $SERVER_PID"
info "Log file: $LOG_FILE"

# Wait for health endpoint
echo -n "   Waiting for server "
if ! wait_for_server; then
    echo ""
    warn "Server didn't respond within 30s. Last log lines:"
    echo "---"
    tail -20 "$LOG_FILE" 2>/dev/null || true
    echo "---"
    die "Server failed to start"
fi
echo -e " ${GREEN}ready!${NC}"

# Quick status
STATUS=$(curl -sf "http://127.0.0.1:${HTTP_PORT}/api/status" 2>/dev/null || echo "{}")
CHANNELS=$(echo "$STATUS" | python -c "import sys,json; print(json.load(sys.stdin).get('channelCount','?'))" 2>/dev/null || echo "?")
info "Server running: $CHANNELS channels on port $HTTP_PORT"

# ── Self-test mode ───────────────────────────────────────────

if $SELFTEST; then
    echo ""
    step "Running smoke test..."

    # Inject
    info "Injecting test values..."
    INJECT_RESP=$(curl -sf -X POST "http://127.0.0.1:${HTTP_PORT}/api/sim/inject" \
        -H 'Content-Type: application/json' \
        -d '{"points":[
            {"type":"analog","device":0,"address":0,"value":72.5},
            {"type":"analog","device":0,"address":1,"value":55.0},
            {"type":"digital","address":40,"value":true},
            {"type":"i2c","device":2,"address":64,"label":"sdp810","value":250.0}
        ]}')
    echo "   Inject response: $INJECT_RESP"

    # Wait for a poll cycle
    sleep 2

    # Read back
    info "Reading channels..."
    CHANNELS_DATA=$(curl -sf "http://127.0.0.1:${HTTP_PORT}/api/read?filter=point")
    echo "$CHANNELS_DATA" | python -c "
import sys, json
data = json.load(sys.stdin)
print(f'   Found {len(data)} channels:')
for ch in sorted(data, key=lambda x: x.get('id',0)):
    print(f'     ch {ch[\"id\"]:5d}  {ch[\"status\"]:8s}  raw={ch[\"raw\"]:8.1f}  cur={ch[\"cur\"]:8.1f}  {ch[\"label\"]}')
" 2>/dev/null || echo "   $CHANNELS_DATA"

    # Check state
    STATE=$(curl -sf "http://127.0.0.1:${HTTP_PORT}/api/sim/state")
    echo "   Sim state: $STATE"

    # Check outputs
    OUTPUTS=$(curl -sf "http://127.0.0.1:${HTTP_PORT}/api/sim/outputs")
    echo "   Outputs: $OUTPUTS"

    echo ""
    info "Smoke test complete!"
    SELFTEST_DONE=true
    exit 0
fi

# ── Scenario mode ────────────────────────────────────────────

if $SCENARIO; then
    echo ""
    step "Loading scenario: $SCENARIO_FILE"

    if [[ ! -f "$SCENARIO_FILE" ]]; then
        die "Scenario file not found: $SCENARIO_FILE"
    fi

    SCENARIO_RESP=$(curl -sf -X POST "http://127.0.0.1:${HTTP_PORT}/api/sim/scenario" \
        -H 'Content-Type: application/json' \
        -d @"$SCENARIO_FILE")
    echo "   Scenario response: $SCENARIO_RESP"

    info "Scenario running. Monitoring channel values..."
    echo ""

    # Monitor channels in a loop until scenario ends
    STEPS=$(echo "$SCENARIO_RESP" | python -c "import sys,json; print(json.load(sys.stdin).get('steps',0))" 2>/dev/null || echo "8")
    # Each step is ~10s apart typically; monitor for steps * 10s + buffer
    MONITOR_SECS=$(( STEPS * 10 + 10 ))
    END_TIME=$(( $(date +%s) + MONITOR_SECS ))

    while (( $(date +%s) < END_TIME )); do
        CHANNELS_DATA=$(curl -sf "http://127.0.0.1:${HTTP_PORT}/api/read?filter=point" 2>/dev/null || echo "[]")
        STATE=$(curl -sf "http://127.0.0.1:${HTTP_PORT}/api/sim/state" 2>/dev/null || echo "{}")
        OUTPUTS=$(curl -sf "http://127.0.0.1:${HTTP_PORT}/api/sim/outputs" 2>/dev/null || echo '{"outputs":[]}')

        NOW=$(date +%H:%M:%S)
        # Print compact status
        echo "$CHANNELS_DATA" | python -c "
import sys, json
data = json.load(sys.stdin)
vals = {ch['id']: ch['cur'] for ch in data}
parts = ' '.join(f'ch{k}={v:.1f}' for k,v in sorted(vals.items()))
print(f'  [$NOW] {parts}')
" NOW="$NOW" 2>/dev/null || echo "  [$NOW] $CHANNELS_DATA"

        sleep 2
    done

    info "Scenario monitoring complete."
    exit 0
fi

# ── Bridge mode ──────────────────────────────────────────────

if $BRIDGE; then
    echo ""
    step "Starting BASemulator bridge"

    BRIDGE_ARGS=("--mapping" "$MAPPING_FILE")
    BRIDGE_ARGS+=("--log-file" "$BRIDGE_LOG_DIR/bridge-$(date +%Y%m%d_%H%M%S).log")
    BRIDGE_ARGS+=("--data-dir" "$DATA_LOG_DIR")
    if $VERBOSE; then
        BRIDGE_ARGS+=("--verbose")
    fi
    if $ONCE; then
        BRIDGE_ARGS+=("--once")
    fi
    if [[ -n "$INTERVAL" ]]; then
        BRIDGE_ARGS+=("--interval" "$INTERVAL")
    fi

    PY=$(get_python)
    info "Bridge mapping: $MAPPING_FILE"
    info "Engine log:  $LOG_FILE"
    info "Bridge log:  $BRIDGE_LOG_DIR/"
    info "Data log:    $DATA_LOG_DIR/"
    info "Press Ctrl+C to stop both server and bridge"
    echo ""
    echo -e "${BOLD}━━━ Bridge Output ━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo ""

    "$PY" "$SCRIPT_DIR/basemulator-bridge.py" "${BRIDGE_ARGS[@]}" &
    BRIDGE_PID=$!

    # Wait for bridge to finish (or Ctrl+C)
    wait "$BRIDGE_PID" 2>/dev/null || true
else
    # No bridge — just keep server running
    echo ""
    info "Server running on http://127.0.0.1:${HTTP_PORT}"
    info "Sim endpoints:"
    info "  POST http://127.0.0.1:${HTTP_PORT}/api/sim/inject    — inject sensor values"
    info "  GET  http://127.0.0.1:${HTTP_PORT}/api/sim/outputs   — read control outputs"
    info "  GET  http://127.0.0.1:${HTTP_PORT}/api/sim/state     — debug state dump"
    info "  POST http://127.0.0.1:${HTTP_PORT}/api/sim/scenario  — run timed scenario"
    echo ""
    info "Press Ctrl+C to stop."

    # Wait for server
    wait "$SERVER_PID" 2>/dev/null || true
fi
