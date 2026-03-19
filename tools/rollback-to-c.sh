#!/bin/bash
# Rollback from Rust engine to C engine.
#
# This script:
# 1. Stops the Rust engine (sandstar-engine.service)
# 2. Starts the C engine (sandstar.service)
# 3. Optionally restarts the Rust validation service for continued testing
#
# Usage:
#   ./rollback-to-c.sh                # Interactive
#   ./rollback-to-c.sh --force        # Skip confirmation
#   ./rollback-to-c.sh --no-validate  # Don't restart Rust in validation mode

set -euo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'

info()  { echo -e "${CYAN}[INFO]${NC}  $*"; }
ok()    { echo -e "${GREEN}[ OK ]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
die()   { echo -e "${RED}[FAIL]${NC} $*" >&2; exit 1; }

FORCE=0
NO_VALIDATE=0

for arg in "$@"; do
    case "$arg" in
        --force)        FORCE=1 ;;
        --no-validate)  NO_VALIDATE=1 ;;
        --help|-h)
            echo "Usage: $0 [--force] [--no-validate]"
            echo "Rollback from Rust engine to C engine."
            exit 0 ;;
    esac
done

PROD_PORT=8085

echo "=========================================="
echo -e " ${BOLD}Sandstar: Rollback Rust → C${NC}"
echo "=========================================="
echo ""

# ── Confirmation ──────────────────────────────────────────────
if [ "$FORCE" -eq 0 ]; then
    echo -e "${YELLOW}This will:${NC}"
    echo "  1. Stop the Rust engine"
    echo "  2. Start the C engine on port $PROD_PORT"
    if [ "$NO_VALIDATE" -eq 0 ]; then
        echo "  3. Restart Rust in validation mode (port 8086)"
    fi
    echo ""
    read -rp "Proceed with rollback? [y/N] " confirm
    if [[ ! "$confirm" =~ ^[Yy] ]]; then
        die "Aborted by user"
    fi
fi

# ── Step 1: Stop Rust engine ─────────────────────────────────
info "Stopping Rust engine..."
systemctl stop sandstar-engine 2>/dev/null || true
systemctl disable sandstar-engine 2>/dev/null || true
ok "Rust engine stopped"

# Wait for port to free
sleep 2

# ── Step 2: Start C engine ───────────────────────────────────
info "Starting C engine..."
systemctl enable sandstar 2>/dev/null || true
systemctl start sandstar
sleep 3

if ! systemctl is-active --quiet sandstar 2>/dev/null; then
    die "C engine failed to start! Check: journalctl -u sandstar -n 50"
fi
ok "C engine started"

# Verify C engine API
HTTP_CODE=$(curl -s -o /dev/null -w '%{http_code}' --connect-timeout 5 \
    "http://localhost:${PROD_PORT}/api/about" 2>/dev/null || echo "000")
if [ "$HTTP_CODE" = "200" ]; then
    ok "C engine API healthy on port $PROD_PORT"
else
    warn "C engine API returned HTTP $HTTP_CODE (may need time to initialize)"
fi

# ── Step 3: Optionally restart Rust in validation mode ────────
if [ "$NO_VALIDATE" -eq 0 ]; then
    info "Restarting Rust in validation mode (port 8086)..."
    sleep 2
    systemctl enable sandstar-rust-validate 2>/dev/null || true
    systemctl start sandstar-rust-validate 2>/dev/null || true
    sleep 2

    if systemctl is-active --quiet sandstar-rust-validate 2>/dev/null; then
        ok "Rust validation service running on port 8086"
    else
        warn "Rust validation service failed to start (non-critical)"
    fi
fi

echo ""
echo "=========================================="
echo -e " ${GREEN}${BOLD}Rollback complete!${NC}"
echo ""
echo "  Engine:   C (sandstar.service)"
echo "  Port:     $PROD_PORT"
echo ""
echo "  Monitor:  journalctl -u sandstar -f"
echo "=========================================="
