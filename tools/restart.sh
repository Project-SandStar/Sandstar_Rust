#!/usr/bin/env bash
# restart.sh — Restart the Sandstar Rust engine server
#
# Usage:
#   ./restart.sh                     # Restart in demo mode
#   ./restart.sh --config            # Restart with EacIo config
#   ./restart.sh --sedona            # Restart with Sedona VM
#   ./restart.sh -- --http-port 9090 # Pass extra args to server
#
# All arguments are forwarded to start.sh.
# Stop is always graceful unless the server is unresponsive.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "=== Stopping ==="
"$SCRIPT_DIR/stop.sh"

echo ""
echo "=== Starting ==="
"$SCRIPT_DIR/start.sh" "$@"
