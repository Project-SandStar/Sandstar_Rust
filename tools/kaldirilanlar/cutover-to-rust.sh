#!/bin/bash
# Cutover production from C engine to Rust engine.
#
# This script:
# 1. Verifies the Rust validation service is healthy
# 2. Stops the C engine (sandstar.service)
# 3. Reconfigures the Rust engine for production (port 8085, no --read-only)
# 4. Starts the Rust engine as the primary service
#
# Prerequisites:
#   - Soak test passed (see validation-runbook.md)
#   - Both engines running: C on 8085, Rust on 8086
#
# Usage:
#   ./cutover-to-rust.sh              # Interactive (asks for confirmation)
#   ./cutover-to-rust.sh --force      # Skip confirmation (for automation)
#
# Rollback:  ./rollback-to-c.sh

set -euo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'

info()  { echo -e "${CYAN}[INFO]${NC}  $*"; }
ok()    { echo -e "${GREEN}[ OK ]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
die()   { echo -e "${RED}[FAIL]${NC} $*" >&2; exit 1; }

FORCE=0
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        --help|-h)
            echo "Usage: $0 [--force]"
            echo "Cutover production from C engine to Rust engine."
            exit 0 ;;
    esac
done

RUST_PORT=8086
C_PORT=8085
PROD_PORT=8085
BACKUP_DIR="/home/eacio/sandstar/backup/cutover-$(date +%Y%m%d_%H%M%S)"

echo "=========================================="
echo -e " ${BOLD}Sandstar: Cutover C → Rust${NC}"
echo "=========================================="
echo ""

# ── Step 1: Pre-flight checks ────────────────────────────────
info "Running pre-flight checks..."

# Check Rust validation service is running
if ! systemctl is-active --quiet sandstar-rust-validate 2>/dev/null; then
    die "Rust validation service not running. Start it first and pass soak test."
fi
ok "Rust validation service running"

# Check Rust API health
HTTP_CODE=$(curl -s -o /dev/null -w '%{http_code}' --connect-timeout 5 \
    "http://localhost:${RUST_PORT}/api/about" 2>/dev/null || echo "000")
if [ "$HTTP_CODE" != "200" ]; then
    die "Rust API not healthy (HTTP $HTTP_CODE on port $RUST_PORT)"
fi
ok "Rust API responding (port $RUST_PORT)"

# Check C engine is running
if ! systemctl is-active --quiet sandstar 2>/dev/null; then
    warn "C engine (sandstar.service) not running — may already be cut over"
fi

# Get channel counts from both engines
RUST_CHANNELS=$(curl -s --connect-timeout 5 "http://localhost:${RUST_PORT}/api/status" 2>/dev/null \
    | python3 -c "import json,sys; print(json.load(sys.stdin).get('channelCount','?'))" 2>/dev/null || echo "?")
C_CHANNELS=$(curl -s --connect-timeout 5 "http://localhost:${C_PORT}/api/status" 2>/dev/null \
    | python3 -c "import json,sys; print(json.load(sys.stdin).get('channelCount','?'))" 2>/dev/null || echo "?")
info "Channel counts — C: $C_CHANNELS, Rust: $RUST_CHANNELS"

if [ "$RUST_CHANNELS" != "$C_CHANNELS" ] && [ "$C_CHANNELS" != "?" ]; then
    warn "Channel count mismatch! C=$C_CHANNELS Rust=$RUST_CHANNELS"
fi

# ── Step 2: Confirmation ─────────────────────────────────────
echo ""
if [ "$FORCE" -eq 0 ]; then
    echo -e "${YELLOW}This will:${NC}"
    echo "  1. Stop the C engine (sandstar.service)"
    echo "  2. Stop the Rust validation service"
    echo "  3. Start the Rust engine as production (port $PROD_PORT)"
    echo ""
    read -rp "Proceed with cutover? [y/N] " confirm
    if [[ ! "$confirm" =~ ^[Yy] ]]; then
        die "Aborted by user"
    fi
fi

# ── Step 3: Create backup point ──────────────────────────────
info "Creating backup snapshot..."
mkdir -p "$BACKUP_DIR"
# Save current service states
systemctl status sandstar 2>/dev/null > "$BACKUP_DIR/c-engine-status.txt" || true
systemctl status sandstar-rust-validate 2>/dev/null > "$BACKUP_DIR/rust-validate-status.txt" || true
# Save current config
cp /etc/systemd/system/sandstar-engine.service "$BACKUP_DIR/" 2>/dev/null || true
cp /etc/systemd/system/sandstar-rust-validate.service "$BACKUP_DIR/" 2>/dev/null || true
date > "$BACKUP_DIR/cutover-timestamp.txt"
ok "Backup saved to $BACKUP_DIR"

# ── Step 4: Stop Rust validation service ──────────────────────
info "Stopping Rust validation service..."
systemctl stop sandstar-rust-validate 2>/dev/null || true
systemctl disable sandstar-rust-validate 2>/dev/null || true
ok "Rust validation service stopped"

# ── Step 5: Stop C engine ────────────────────────────────────
info "Stopping C engine..."
systemctl stop sandstar 2>/dev/null || true
systemctl disable sandstar 2>/dev/null || true
ok "C engine stopped"

# Wait for ports to free up
sleep 2

# Verify port 8085 is free
if ss -tlnp | grep -q ":${PROD_PORT} " 2>/dev/null; then
    warn "Port $PROD_PORT still in use — waiting..."
    sleep 3
    if ss -tlnp | grep -q ":${PROD_PORT} " 2>/dev/null; then
        die "Port $PROD_PORT still occupied after 5s"
    fi
fi
ok "Port $PROD_PORT is free"

# ── Step 6: Start Rust engine in production mode ─────────────
info "Starting Rust engine on port $PROD_PORT..."
systemctl enable sandstar-engine 2>/dev/null || true
systemctl start sandstar-engine
sleep 2

# Verify it started
if ! systemctl is-active --quiet sandstar-engine; then
    die "Rust engine failed to start! Run: rollback-to-c.sh"
fi
ok "Rust engine started"

# Verify API
HTTP_CODE=$(curl -s -o /dev/null -w '%{http_code}' --connect-timeout 5 \
    "http://localhost:${PROD_PORT}/api/about" 2>/dev/null || echo "000")
if [ "$HTTP_CODE" != "200" ]; then
    warn "Rust API returned HTTP $HTTP_CODE — checking again in 5s..."
    sleep 5
    HTTP_CODE=$(curl -s -o /dev/null -w '%{http_code}' --connect-timeout 5 \
        "http://localhost:${PROD_PORT}/api/about" 2>/dev/null || echo "000")
    if [ "$HTTP_CODE" != "200" ]; then
        die "Rust API not healthy after cutover! Run: rollback-to-c.sh"
    fi
fi
ok "Rust API healthy on port $PROD_PORT"

# ── Step 7: Post-cutover verification ────────────────────────
RUST_STATUS=$(curl -s --connect-timeout 5 "http://localhost:${PROD_PORT}/api/status" 2>/dev/null)
CHANNELS=$(echo "$RUST_STATUS" | python3 -c "import json,sys; print(json.load(sys.stdin).get('channelCount','?'))" 2>/dev/null || echo "?")
info "Production engine serving $CHANNELS channels on port $PROD_PORT"

echo ""
echo "=========================================="
echo -e " ${GREEN}${BOLD}Cutover complete!${NC}"
echo ""
echo "  Engine:   Rust (sandstar-engine)"
echo "  Port:     $PROD_PORT"
echo "  Channels: $CHANNELS"
echo ""
echo "  Rollback: ./rollback-to-c.sh"
echo "  Monitor:  journalctl -u sandstar-engine -f"
echo "=========================================="
