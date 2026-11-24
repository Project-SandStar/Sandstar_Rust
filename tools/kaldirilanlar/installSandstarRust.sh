#!/bin/bash
# installSandstarRust.sh - Deploy Rust sandstar to BeagleBone device
#
# Usage: ./installSandstarRust.sh [DEVICE]
#   DEVICE defaults to "30-113" (Todd Air Flow)
#
# Prerequisites:
#   - ARM .deb built: cargo arm-build && cargo arm-deb --no-strip
#   - SSH key configured for target device
#   - Connection file exists at tools/connections/{DEVICE}.sh

set -e

DEVICE="${1:-30-113}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RUST_ROOT="$(dirname "$SCRIPT_DIR")"
DEB_DIR="${RUST_ROOT}/target/debian"
CONNECTIONS_DIR="${SCRIPT_DIR}/../../tools/connections"
SSH_KEY="$HOME/.ssh/id_ed25519"
SSH_OPTS="-i $SSH_KEY -o StrictHostKeyChecking=no -o ConnectTimeout=10"
SUDO_PASS="${SANDSTAR_SUDO_PASS:?Set SANDSTAR_SUDO_PASS env var}"

echo "=================================================="
echo "Sandstar Rust Deployment"
echo "=================================================="
echo "Device: $DEVICE"
echo ""

# Step 1: Find latest .deb
echo "[1/5] Finding latest .deb package..."
LATEST_DEB=$(ls -t "${DEB_DIR}"/sandstar_*.deb 2>/dev/null | head -1)
if [ -z "$LATEST_DEB" ]; then
    echo "ERROR: No .deb found in ${DEB_DIR}"
    echo "Build first: cargo arm-build && cargo arm-deb --no-strip"
    exit 1
fi
DEB_FILENAME=$(basename "$LATEST_DEB")
DEB_SIZE=$(ls -lh "$LATEST_DEB" | awk '{print $5}')
echo "Found: $DEB_FILENAME ($DEB_SIZE)"

# Step 2: Resolve device connection
echo ""
echo "[2/5] Resolving device connection..."
CONNECTION_FILE="${CONNECTIONS_DIR}/${DEVICE}.sh"
if [ ! -f "$CONNECTION_FILE" ]; then
    echo "ERROR: Unknown device '$DEVICE'. Valid options:"
    ls -1 "${CONNECTIONS_DIR}"/*.sh 2>/dev/null | xargs -n1 basename | sed 's/.sh$//' | sort
    exit 1
fi

# Extract IP and port from connection file
IP=$(grep -oP 'eacio@\K[0-9.]+' "$CONNECTION_FILE" | head -1)
PORT=$(grep -oP '\-p\s+\K[0-9]+' "$CONNECTION_FILE" | head -1)
PORT="${PORT:-22}"

if [ -z "$IP" ]; then
    echo "ERROR: Could not extract IP from $CONNECTION_FILE"
    exit 1
fi
echo "Target: eacio@${IP}:${PORT}"

# Step 3: Transfer .deb
echo ""
echo "[3/5] Transferring $DEB_FILENAME..."
scp -P "$PORT" $SSH_OPTS "$LATEST_DEB" "eacio@${IP}:/home/eacio/"
echo "Transfer complete"

# Step 4: Install on device
echo ""
echo "[4/5] Installing on device..."
ssh -p "$PORT" $SSH_OPTS "eacio@${IP}" "
    echo '$SUDO_PASS' | sudo -S systemctl stop sandstar-engine.service 2>/dev/null || true;
    echo '$SUDO_PASS' | sudo -S systemctl stop sandstar.service 2>/dev/null || true;
    echo '$SUDO_PASS' | sudo -S dpkg -i /home/eacio/$DEB_FILENAME 2>&1;
    echo '$SUDO_PASS' | sudo -S mkdir -p /var/log/sandstar;
    echo '$SUDO_PASS' | sudo -S chown eacio:root /var/log/sandstar;
"
echo "Installation complete"

# Step 5: Verify
echo ""
echo "[5/5] Verifying deployment..."
sleep 3
ssh -p "$PORT" $SSH_OPTS "eacio@${IP}" "
    echo '=== Service Status ===';
    systemctl is-active sandstar-engine.service 2>&1;
    echo;
    echo '=== API Status ===';
    STATUS=\$(curl -s --connect-timeout 5 http://127.0.0.1:8085/api/status 2>&1);
    echo \"\$STATUS\";
    echo;
    CHANNELS=\$(echo \"\$STATUS\" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get(\"channelCount\",0))' 2>/dev/null || echo '?');
    TABLES=\$(echo \"\$STATUS\" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get(\"tableCount\",0))' 2>/dev/null || echo '?');
    UPTIME=\$(echo \"\$STATUS\" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get(\"uptimeSecs\",0))' 2>/dev/null || echo '?');
    echo \"Channels: \$CHANNELS | Tables: \$TABLES | Uptime: \${UPTIME}s\";
"

echo ""
echo "=================================================="
echo "Deployment complete!"
echo "Package: $DEB_FILENAME"
echo "Device:  $DEVICE ($IP:$PORT)"
echo "=================================================="
