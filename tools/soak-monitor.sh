#!/bin/bash
# Soak test monitor — runs on BeagleBone alongside Rust validation service.
# Periodically checks engine health, memory, match rate, and alerts on anomalies.
#
# Usage:
#   ./soak-monitor.sh                   # Monitor forever (default)
#   ./soak-monitor.sh --duration 24h    # Monitor for 24 hours
#   ./soak-monitor.sh --interval 30     # Check every 30 seconds (default: 60)
#   ./soak-monitor.sh --alert-log /var/log/sandstar/soak-alerts.log

set -euo pipefail

# ── Configuration ─────────────────────────────────────────────
RUST_PORT="${RUST_PORT:-8086}"
C_PORT="${C_PORT:-8085}"
INTERVAL="${INTERVAL:-60}"
DURATION="${DURATION:-0}"  # 0 = forever
ALERT_LOG="${ALERT_LOG:-/var/log/sandstar/soak-alerts.log}"
MONITOR_LOG="${MONITOR_LOG:-/var/log/sandstar/soak-monitor.log}"

# Thresholds
MEM_MAX_MB=64           # Alert if RSS exceeds this
MEM_GROWTH_MAX_KB=512   # Alert if RSS grows more than this per check
MATCH_RATE_MIN=99.0     # Alert if match rate drops below this %
MAX_RESTARTS=0          # Alert if NRestarts exceeds this

# ── Parse args ────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --duration)   DURATION="$2"; shift 2 ;;
        --interval)   INTERVAL="$2"; shift 2 ;;
        --alert-log)  ALERT_LOG="$2"; shift 2 ;;
        --help|-h)
            echo "Usage: $0 [--duration 24h] [--interval 60] [--alert-log FILE]"
            exit 0 ;;
        *) echo "Unknown: $1"; exit 1 ;;
    esac
done

# Convert duration to seconds
duration_to_secs() {
    local val="$1"
    case "$val" in
        *h) echo $(( ${val%h} * 3600 )) ;;
        *m) echo $(( ${val%m} * 60 )) ;;
        *s) echo "${val%s}" ;;
        0)  echo 0 ;;
        *)  echo "$val" ;;
    esac
}
DURATION_SECS=$(duration_to_secs "$DURATION")

# ── Logging ───────────────────────────────────────────────────
mkdir -p "$(dirname "$ALERT_LOG")" "$(dirname "$MONITOR_LOG")" 2>/dev/null || true
PREV_RSS_KB=0
START_TIME=$(date +%s)
CHECK_COUNT=0
ALERT_COUNT=0

log()   { echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" | tee -a "$MONITOR_LOG"; }
alert() {
    ALERT_COUNT=$((ALERT_COUNT + 1))
    echo "[ALERT $(date '+%Y-%m-%d %H:%M:%S')] $*" | tee -a "$ALERT_LOG" "$MONITOR_LOG"
}

# ── Health check functions ────────────────────────────────────

check_service_status() {
    if ! systemctl is-active --quiet sandstar-rust-validate 2>/dev/null; then
        # Try sandstar-engine for production mode
        if ! systemctl is-active --quiet sandstar-engine 2>/dev/null; then
            alert "DEAD: Rust engine service not running"
            return 1
        fi
    fi
    return 0
}

check_restarts() {
    local restarts
    restarts=$(systemctl show sandstar-rust-validate -p NRestarts 2>/dev/null \
        || systemctl show sandstar-engine -p NRestarts 2>/dev/null \
        || echo "NRestarts=0")
    restarts="${restarts#NRestarts=}"
    if [ "$restarts" -gt "$MAX_RESTARTS" ]; then
        alert "RESTARTS: $restarts (max: $MAX_RESTARTS)"
    fi
    echo "$restarts"
}

check_memory() {
    local pid rss_kb
    pid=$(pgrep -f sandstar-engine-server 2>/dev/null | head -1)
    if [ -z "$pid" ]; then
        alert "MEMORY: Cannot find sandstar-engine-server process"
        return 1
    fi
    rss_kb=$(awk '/VmRSS/{print $2}' /proc/"$pid"/status 2>/dev/null || echo 0)
    local rss_mb=$((rss_kb / 1024))

    if [ "$rss_mb" -gt "$MEM_MAX_MB" ]; then
        alert "MEMORY: ${rss_mb}MB exceeds limit of ${MEM_MAX_MB}MB"
    fi

    if [ "$PREV_RSS_KB" -gt 0 ]; then
        local growth=$((rss_kb - PREV_RSS_KB))
        if [ "$growth" -gt "$MEM_GROWTH_MAX_KB" ]; then
            alert "MEMORY_GROWTH: +${growth}KB since last check (limit: ${MEM_GROWTH_MAX_KB}KB)"
        fi
    fi
    PREV_RSS_KB=$rss_kb
    echo "${rss_mb}MB (${rss_kb}KB)"
}

check_api_health() {
    local http_code
    http_code=$(curl -s -o /dev/null -w '%{http_code}' \
        --connect-timeout 5 --max-time 10 \
        "http://localhost:${RUST_PORT}/api/about" 2>/dev/null || echo "000")
    if [ "$http_code" != "200" ]; then
        alert "API: /api/about returned HTTP $http_code"
        return 1
    fi
    return 0
}

check_match_rate() {
    # Compare channel values between C and Rust
    local rust_polls c_polls
    rust_polls=$(curl -s --connect-timeout 5 --max-time 10 \
        "http://localhost:${RUST_PORT}/api/polls" 2>/dev/null)
    c_polls=$(curl -s --connect-timeout 5 --max-time 10 \
        "http://localhost:${C_PORT}/api/polls" 2>/dev/null)

    if [ -z "$rust_polls" ] || [ -z "$c_polls" ]; then
        log "SKIP: Cannot fetch polls from one or both engines"
        return 0
    fi

    # Parse and compare using python (more reliable than jq on BeagleBone)
    local result
    result=$(python3 -c "
import json, sys
try:
    rust = json.loads('''$rust_polls''')
    c_eng = json.loads('''$c_polls''')
    rust_map = {str(r.get('channel',r.get('id','?'))): r.get('lastCur',0) for r in rust.get('rows',rust) if isinstance(r, dict)}
    c_map = {str(r.get('channel',r.get('id','?'))): r.get('lastCur',0) for r in c_eng.get('rows',c_eng) if isinstance(r, dict)}
    total = max(len(rust_map), 1)
    matches = sum(1 for k in rust_map if k in c_map and abs(float(rust_map[k] or 0) - float(c_map[k] or 0)) < 0.5)
    rate = 100.0 * matches / total
    print(f'{rate:.1f} {matches}/{total}')
except Exception as e:
    print(f'ERR {e}')
" 2>/dev/null || echo "ERR parse")

    if [[ "$result" == ERR* ]]; then
        log "SKIP: Match rate parse error: $result"
        return 0
    fi

    local rate="${result%% *}"
    local detail="${result#* }"

    # Compare rate against threshold (integer comparison)
    local rate_int="${rate%%.*}"
    local min_int="${MATCH_RATE_MIN%%.*}"
    if [ "$rate_int" -lt "$min_int" ]; then
        alert "MATCH_RATE: ${rate}% ($detail) below threshold ${MATCH_RATE_MIN}%"
    fi
    echo "${rate}% ($detail)"
}

check_log_errors() {
    # Check for recent panics or fatal errors in last interval
    local since
    since=$(date -d "-${INTERVAL} seconds" '+%Y-%m-%d %H:%M:%S' 2>/dev/null || date '+%Y-%m-%d %H:%M:%S')
    local errors
    errors=$(journalctl -u sandstar-rust-validate --since "$since" --no-pager 2>/dev/null \
        | grep -cEi 'panic|fatal|SIGSE|SIGAB|thread.*panicked' || echo 0)
    if [ "$errors" -gt 0 ]; then
        alert "LOG_ERRORS: $errors panic/fatal entries in last ${INTERVAL}s"
    fi
    echo "$errors"
}

# ── Main loop ─────────────────────────────────────────────────
log "Soak monitor started: interval=${INTERVAL}s, duration=${DURATION}, port=${RUST_PORT}"
log "Thresholds: mem=${MEM_MAX_MB}MB, growth=${MEM_GROWTH_MAX_KB}KB, match=${MATCH_RATE_MIN}%, restarts=${MAX_RESTARTS}"

trap 'log "Soak monitor stopped after $CHECK_COUNT checks, $ALERT_COUNT alerts"; exit 0' INT TERM

while true; do
    CHECK_COUNT=$((CHECK_COUNT + 1))
    elapsed=$(( $(date +%s) - START_TIME ))

    # Duration check
    if [ "$DURATION_SECS" -gt 0 ] && [ "$elapsed" -ge "$DURATION_SECS" ]; then
        log "Duration reached ($DURATION). Stopping."
        break
    fi

    # Run all checks
    status="UP"
    check_service_status || status="DOWN"
    restarts=$(check_restarts)
    memory=$(check_memory 2>/dev/null || echo "?")
    api_ok="OK"
    check_api_health || api_ok="FAIL"
    match_rate=$(check_match_rate 2>/dev/null || echo "?")
    log_errors=$(check_log_errors 2>/dev/null || echo "?")

    # Summary line
    hours=$((elapsed / 3600))
    mins=$(( (elapsed % 3600) / 60 ))
    log "CHECK #${CHECK_COUNT} [${hours}h${mins}m] status=${status} api=${api_ok} mem=${memory} match=${match_rate} restarts=${restarts} errors=${log_errors}"

    # Periodic detailed summary (every 60 checks)
    if [ $((CHECK_COUNT % 60)) -eq 0 ]; then
        log "=== SUMMARY: ${CHECK_COUNT} checks, ${ALERT_COUNT} alerts, uptime ${hours}h${mins}m ==="
    fi

    sleep "$INTERVAL"
done

log "Soak monitor complete: ${CHECK_COUNT} checks, ${ALERT_COUNT} alerts"
exit $( [ "$ALERT_COUNT" -eq 0 ] && echo 0 || echo 1 )
