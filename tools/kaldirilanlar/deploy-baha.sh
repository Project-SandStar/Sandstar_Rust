#!/bin/bash
# deploy-baha.sh - Deploy Rust Sandstar to Baha BeagleBone (211-135)
#
# Usage:
#   ./deploy-baha.sh                    # Direct mode to 172.28.211.135
#   ./deploy-baha.sh 172.28.211.135     # Explicit IP (direct)
#   ./deploy-baha.sh 10.1.10.229        # Explicit IP (e.g. internal NAT IP)
#
# Modes:
#   Direct mode:     No jump host, SSH/SCP directly to device
#   Jump host mode:  Set SANDSTAR_JUMP_HOST=solidyne@172.28.109.221
#
# Environment variables:
#   SANDSTAR_SUDO_PASS  (required) - sudo password on device
#   SANDSTAR_SSH_KEY     (optional) - SSH key path (default: ~/.ssh/id_ed25519)
#   SANDSTAR_JUMP_HOST   (optional) - Jump host for proxied access
#                                     e.g. solidyne@172.28.109.221
#   SANDSTAR_SSH_PORT    (optional) - SSH port on device (default: 1919)
#   SANDSTAR_USER        (optional) - SSH user on device (default: eacio)
#   SANDSTAR_API_PORT    (optional) - Sandstar HTTP port (default: 8085)
#
# Note: Both Todd and Baha share the same EacIo config. No device-specific
# config preparation is needed.

set -euo pipefail

# ---------- Configuration ----------
DEVICE_IP="${1:-172.28.211.135}"
SSH_PORT="${SANDSTAR_SSH_PORT:-1919}"
SSH_USER="${SANDSTAR_USER:-eacio}"
SSH_KEY="${SANDSTAR_SSH_KEY:-$HOME/.ssh/id_ed25519}"
JUMP_HOST="${SANDSTAR_JUMP_HOST:-}"
API_PORT="${SANDSTAR_API_PORT:-8085}"
SUDO_PASS="${SANDSTAR_SUDO_PASS:?ERROR: Set SANDSTAR_SUDO_PASS env var}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RUST_ROOT="$(dirname "$SCRIPT_DIR")"
DEB_DIR="${RUST_ROOT}/target/debian"

# ---------- SSH options ----------
SSH_BASE_OPTS="-o StrictHostKeyChecking=no -o ConnectTimeout=15 -o ServerAliveInterval=10"
if [ -f "$SSH_KEY" ]; then
    SSH_BASE_OPTS="$SSH_BASE_OPTS -i $SSH_KEY"
fi

if [ -n "$JUMP_HOST" ]; then
    MODE="jump-host"
    SSH_OPTS="$SSH_BASE_OPTS -J $JUMP_HOST -p $SSH_PORT"
    SCP_PROXY="-o ProxyJump=$JUMP_HOST"
else
    MODE="direct"
    SSH_OPTS="$SSH_BASE_OPTS -p $SSH_PORT"
    SCP_PROXY=""
fi

# ---------- Helper functions ----------
remote_ssh() {
    ssh $SSH_OPTS "${SSH_USER}@${DEVICE_IP}" "$@"
}

remote_sudo() {
    # Run a command with sudo on the remote device
    remote_ssh "echo '$SUDO_PASS' | sudo -S $*"
}

header() {
    echo ""
    echo "==== $1 ===="
}

ok()   { echo "  [OK] $1"; }
fail() { echo "  [FAIL] $1"; }
info() { echo "  $1"; }

# ---------- Main deployment ----------
echo "=========================================================="
echo "  Sandstar Rust Deployment — Baha (211-135)"
echo "=========================================================="
echo "  Target:     ${SSH_USER}@${DEVICE_IP}:${SSH_PORT}"
echo "  Mode:       ${MODE}"
[ -n "$JUMP_HOST" ] && echo "  Jump host:  ${JUMP_HOST}"
echo "  API port:   ${API_PORT}"
echo "=========================================================="

# Step 1: Find latest .deb
header "Step 1/6: Find .deb package"

LATEST_DEB=$(ls -t "${DEB_DIR}"/sandstar_*.deb 2>/dev/null | head -1 || true)
if [ -z "$LATEST_DEB" ]; then
    LATEST_DEB=$(ls -t "${DEB_DIR}"/sandstar-*.deb 2>/dev/null | head -1 || true)
fi
if [ -z "$LATEST_DEB" ]; then
    fail "No .deb found in ${DEB_DIR}"
    echo "  Build first:"
    echo "    export PATH=\"\$HOME/.cargo/bin:\$PATH\""
    echo "    cargo arm-build && cargo arm-deb --no-strip"
    exit 1
fi

DEB_FILENAME=$(basename "$LATEST_DEB")
DEB_SIZE=$(ls -lh "$LATEST_DEB" | awk '{print $5}')
ok "Found: $DEB_FILENAME ($DEB_SIZE)"

# Step 2: Test connectivity
header "Step 2/6: Test connectivity"

if remote_ssh "echo ok" >/dev/null 2>&1; then
    ok "SSH connection established"
else
    fail "Cannot reach ${SSH_USER}@${DEVICE_IP}:${SSH_PORT}"
    if [ "$MODE" = "direct" ]; then
        echo ""
        echo "  Hint: If this device requires a jump host, set:"
        echo "    export SANDSTAR_JUMP_HOST=solidyne@172.28.109.221"
        echo "  Then re-run this script."
        echo ""
        echo "  Or try from a machine on the same network."
    fi
    exit 1
fi

# Step 3: Pre-flight checks
header "Step 3/6: Pre-flight checks"

REMOTE_ARCH=$(remote_ssh "uname -m" 2>/dev/null || echo "unknown")
REMOTE_OS=$(remote_ssh "cat /etc/debian_version" 2>/dev/null || echo "unknown")
REMOTE_MEM=$(remote_ssh "free -m | awk '/Mem:/{print \$2}'" 2>/dev/null || echo "?")
REMOTE_DISK=$(remote_ssh "df -h /home/eacio | awk 'NR==2{print \$4}'" 2>/dev/null || echo "?")

info "Architecture: ${REMOTE_ARCH}"
info "Debian:       ${REMOTE_OS}"
info "RAM:          ${REMOTE_MEM}MB"
info "Disk free:    ${REMOTE_DISK}"

if [ "$REMOTE_ARCH" != "armv7l" ]; then
    fail "Expected armv7l, got ${REMOTE_ARCH}. Wrong device?"
    exit 1
fi
ok "Pre-flight passed"

# Check what's currently running
CURRENT_SERVICE=$(remote_ssh "systemctl is-active sandstar-engine.service 2>/dev/null || echo inactive")
CURRENT_C_SERVICE=$(remote_ssh "systemctl is-active sandstar.service 2>/dev/null || echo inactive")
info "sandstar-engine.service (Rust): ${CURRENT_SERVICE}"
info "sandstar.service (C legacy):    ${CURRENT_C_SERVICE}"

# Step 4: Transfer .deb
header "Step 4/6: Transfer package"

info "Uploading ${DEB_FILENAME}..."
if [ -n "$SCP_PROXY" ]; then
    scp $SCP_PROXY -P "$SSH_PORT" $SSH_BASE_OPTS "$LATEST_DEB" "${SSH_USER}@${DEVICE_IP}:/home/${SSH_USER}/"
else
    scp -P "$SSH_PORT" $SSH_BASE_OPTS "$LATEST_DEB" "${SSH_USER}@${DEVICE_IP}:/home/${SSH_USER}/"
fi
ok "Transfer complete"

# Step 5: Install
header "Step 5/6: Install package"

info "Stopping services..."
remote_sudo "systemctl stop sandstar-engine.service 2>/dev/null || true"
remote_sudo "systemctl stop sandstar.service 2>/dev/null || true"
sleep 1

info "Installing ${DEB_FILENAME}..."
INSTALL_OUTPUT=$(remote_sudo "dpkg -i /home/${SSH_USER}/${DEB_FILENAME}" 2>&1) || {
    fail "dpkg install failed"
    echo "$INSTALL_OUTPUT"
    exit 1
}
ok "Package installed"

info "Ensuring log directory..."
remote_sudo "mkdir -p /var/log/sandstar"
remote_sudo "chown ${SSH_USER}:root /var/log/sandstar"

info "Reloading systemd and starting service..."
remote_sudo "systemctl daemon-reload"
remote_sudo "systemctl enable sandstar-engine.service 2>/dev/null || true"
remote_sudo "systemctl start sandstar-engine.service"
ok "Service started"

# Step 6: Health check
header "Step 6/6: Health check"

info "Waiting 5 seconds for startup..."
sleep 5

# Check service is running
SVC_STATUS=$(remote_ssh "systemctl is-active sandstar-engine.service 2>/dev/null || echo failed")
if [ "$SVC_STATUS" = "active" ]; then
    ok "Service is active"
else
    fail "Service status: ${SVC_STATUS}"
    info "Last 20 lines of journal:"
    remote_ssh "journalctl -u sandstar-engine.service -n 20 --no-pager 2>/dev/null" || true
    exit 1
fi

# Check /api/about
ABOUT=$(remote_ssh "curl -sf --connect-timeout 5 http://127.0.0.1:${API_PORT}/api/about 2>/dev/null" || echo "")
if [ -n "$ABOUT" ]; then
    ok "/api/about responded"
    info "$ABOUT" | head -5
else
    fail "/api/about not responding (may still be starting)"
fi

# Check /health
HEALTH=$(remote_ssh "curl -sf --connect-timeout 5 http://127.0.0.1:${API_PORT}/health 2>/dev/null" || echo "")
if [ -n "$HEALTH" ]; then
    ok "/health responded"
    info "$HEALTH" | head -3
else
    fail "/health not responding"
fi

# Check /api/status for channel count
STATUS=$(remote_ssh "curl -sf --connect-timeout 5 http://127.0.0.1:${API_PORT}/api/status 2>/dev/null" || echo "")
if [ -n "$STATUS" ]; then
    ok "/api/status responded"
    info "$STATUS" | head -5
else
    info "/api/status not available yet"
fi

# Memory usage
MEM_RSS=$(remote_ssh "ps -o rss= -p \$(pgrep sandstar-engine 2>/dev/null || echo 1) 2>/dev/null" || echo "?")
if [ "$MEM_RSS" != "?" ] && [ -n "$MEM_RSS" ]; then
    MEM_MB=$(echo "$MEM_RSS" | awk '{printf "%.1f", $1/1024}')
    info "Memory usage: ${MEM_MB}MB RSS"
fi

# ---------- Summary ----------
echo ""
echo "=========================================================="
echo "  DEPLOYMENT COMPLETE"
echo "=========================================================="
echo "  Package:    ${DEB_FILENAME}"
echo "  Device:     ${SSH_USER}@${DEVICE_IP}:${SSH_PORT}"
echo "  Service:    ${SVC_STATUS}"
echo "  API:        http://${DEVICE_IP}:${API_PORT}"
echo ""
echo "  Quick checks:"
echo "    curl http://${DEVICE_IP}:${API_PORT}/api/status"
echo "    curl http://${DEVICE_IP}:${API_PORT}/api/about"
echo "    curl http://${DEVICE_IP}:${API_PORT}/health"
echo "=========================================================="
