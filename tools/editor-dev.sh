#!/usr/bin/env bash
# Start the Vue Sedona editor dev server against a real Sandstar device.
#
# The editor runs at http://localhost:<port>/ and its dev proxy forwards
# /live, /api, /file and /auth to http://<device>:8085 (vue.config.js,
# SOX_PROXY_URL). Same origin for the browser, so the S-32 session cookie
# works when auth is on.
#
# Usage:
#   bash tools/editor-dev.sh [device[:port]]
#
# Device address, first match wins (host/IP is never written in the repo):
#   1. first argument;
#   2. SANDSTAR_DEVICE environment variable;
#   3. tools/editor-dev.env (git-ignored), a line: SANDSTAR_DEVICE=<host>
#
# Optional environment:
#   SEDONA_EDITOR_DIR  editor checkout (default C:/Coding/swe/sedonaWebEditor)
#   EDITOR_PORT        dev server port (default 8091)
#
# With auth on, the device must allow this page origin:
#   --ws-allowed-origins http://localhost:<port>
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="${SCRIPT_DIR}/editor-dev.env"

DEVICE="${1:-${SANDSTAR_DEVICE:-}}"
if [ -z "$DEVICE" ] && [ -f "$ENV_FILE" ]; then
  DEVICE="$(sed -n 's/^SANDSTAR_DEVICE=//p' "$ENV_FILE" | tr -d '\r' | head -1)"
fi
if [ -z "$DEVICE" ]; then
  echo "error: no device address. Pass it as an argument, set SANDSTAR_DEVICE," >&2
  echo "       or write SANDSTAR_DEVICE=<host> to ${ENV_FILE}" >&2
  exit 2
fi
case "$DEVICE" in
  *:*) TARGET="http://${DEVICE}" ;;
  *) TARGET="http://${DEVICE}:8085" ;;
esac

EDITOR_DIR="${SEDONA_EDITOR_DIR:-C:/Coding/swe/sedonaWebEditor}"
PORT="${EDITOR_PORT:-8091}"

if [ ! -f "${EDITOR_DIR}/vue.config.js" ]; then
  echo "error: editor not found at ${EDITOR_DIR} (set SEDONA_EDITOR_DIR)" >&2
  exit 2
fi
if [ ! -d "${EDITOR_DIR}/node_modules" ]; then
  echo "error: ${EDITOR_DIR}/node_modules missing; run 'npm ci' there first" >&2
  exit 2
fi

# Refuse to start a second server on the same port.
if curl -s -o /dev/null -m 2 "http://localhost:${PORT}/"; then
  echo "error: something already answers on localhost:${PORT}" >&2
  exit 1
fi

echo "== device"
if ! about="$(curl -sf -m 5 "${TARGET}/api/about")"; then
  echo "error: device not reachable (GET /api/about failed)" >&2
  exit 1
fi
version="$(printf '%s' "$about" | grep -oE '2\.[0-9]+\.[0-9]+\+[0-9a-f]+' | head -1 || true)"
echo "   version: ${version:-unknown}"
gate="$(curl -s -o /dev/null -m 5 -w '%{http_code}' -I "${TARGET}/live/sedonaState" || true)"
case "$gate" in
  200) echo "   auth: off (editor gate open)" ;;
  401) echo "   auth: on (log in with a Sandstar user)" ;;
  *) echo "   auth: unknown (HEAD /live/sedonaState -> ${gate})" ;;
esac

echo "== editor"
branch="$(git -C "$EDITOR_DIR" branch --show-current 2>/dev/null || echo '?')"
commit="$(git -C "$EDITOR_DIR" log -1 --format=%h 2>/dev/null || echo '?')"
echo "   ${EDITOR_DIR} (${branch} ${commit})"
echo "   open http://localhost:${PORT}/ and add a connection with host localhost:${PORT}"
echo "   this is a real device: edits in the editor go to the running app"
echo

cd "$EDITOR_DIR"
export SOX_PROXY_URL="$TARGET"
exec npx vue-cli-service serve --port "$PORT"
