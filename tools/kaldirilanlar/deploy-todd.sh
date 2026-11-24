#!/bin/bash
# deploy-todd.sh - Deploy Rust Sandstar to Todd Air Flow BeagleBone (30-113)
#
# This is a convenience wrapper around deploy-baha.sh for the Todd device.
# Todd Air Flow is directly reachable (no jump host needed).
#
# Usage:
#   ./deploy-todd.sh
#
# The Todd device was the first Rust deployment (Phase 5.9, 2026-03-18).
# Current known IP: 192.168.1.104 (was previously 192.168.30.113)
# Port: 1919, User: eacio
#
# Environment variables:
#   SANDSTAR_SUDO_PASS  (required) - sudo password on device
#   SANDSTAR_SSH_KEY     (optional) - SSH key path (default: ~/.ssh/id_ed25519)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Todd Air Flow - direct connection, no jump host
export SANDSTAR_SSH_PORT="${SANDSTAR_SSH_PORT:-1919}"
export SANDSTAR_USER="${SANDSTAR_USER:-eacio}"

# Unset jump host to ensure direct mode
unset SANDSTAR_JUMP_HOST 2>/dev/null || true

exec "$SCRIPT_DIR/deploy-baha.sh" "${1:-192.168.1.104}"
