#!/bin/bash
# Soak monitor — runs on the device next to the sandstar-engine service.
# Periodically checks that the service is up, has not restarted, answers
# /health, does not grow in memory and logs no error. Read-only towards
# the engine; it writes two log files.
#
# Written for the device as it is (Debian 9, systemd 232, run as the
# service user): systemd 232 has no NRestarts property, so restarts are
# counted from changes of the unit's start time; the journal is not
# readable without the systemd-journal group, so errors are read from the
# engine's own log files. A panic aborts the process and shows as a restart.
#
# Usage:
#   ./soak-monitor.sh                   # Monitor forever (default)
#   ./soak-monitor.sh --duration 24h    # Monitor for 24 hours
#   ./soak-monitor.sh --interval 30     # Check every 30 seconds (default: 60)
#   ./soak-monitor.sh --alert-log /var/log/sandstar/soak-alerts.log
#
# Exit code: 0 if no alert was raised, 1 otherwise.

set -uo pipefail

# ── Configuration ─────────────────────────────────────────────
UNIT="${UNIT:-sandstar-engine}"
PORT="${PORT:-8085}"
INTERVAL="${INTERVAL:-60}"
DURATION="${DURATION:-0}"  # 0 = forever
ALERT_LOG="${ALERT_LOG:-/var/log/sandstar/soak-alerts.log}"
MONITOR_LOG="${MONITOR_LOG:-/var/log/sandstar/soak-monitor.log}"
ENGINE_LOG_DIR="${ENGINE_LOG_DIR:-/var/log/sandstar}"

# Thresholds
MEM_MAX_MB=64           # Alert if RSS exceeds this
MEM_GROWTH_MAX_KB=512   # Alert if RSS grows more than this per check
MAX_RESTARTS=0          # Alert if the service restarted more often than this

# ── Parse args ────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --duration|--interval|--alert-log)
            if [ $# -lt 2 ]; then echo "$1 needs a value"; exit 2; fi
            case "$1" in
                --duration)  DURATION="$2" ;;
                --interval)  INTERVAL="$2" ;;
                --alert-log) ALERT_LOG="$2" ;;
            esac
            shift 2 ;;
        --help|-h)
            echo "Usage: $0 [--duration 24h] [--interval 60] [--alert-log FILE]"
            exit 0 ;;
        *) echo "Unknown: $1"; exit 2 ;;
    esac
done

# Duration: whole number with an optional h, m or s; 0 = forever.
is_number() { case "$1" in ""|*[!0-9]*) return 1 ;; *) return 0 ;; esac; }
case "$DURATION" in
    *h) DUR_NUM="${DURATION%h}"; DUR_MUL=3600 ;;
    *m) DUR_NUM="${DURATION%m}"; DUR_MUL=60 ;;
    *s) DUR_NUM="${DURATION%s}"; DUR_MUL=1 ;;
    *)  DUR_NUM="$DURATION";      DUR_MUL=1 ;;
esac
if ! is_number "$DUR_NUM"; then
    echo "duration must be a whole number, optionally ending in h, m or s"; exit 2
fi
if ! is_number "$INTERVAL" || [ "$INTERVAL" -lt 1 ]; then
    echo "interval must be a whole number of seconds, at least 1"; exit 2
fi
DURATION_SECS=$((DUR_NUM * DUR_MUL))

# ── Logging ───────────────────────────────────────────────────
mkdir -p "$(dirname "$ALERT_LOG")" "$(dirname "$MONITOR_LOG")" 2>/dev/null || true
PREV_RSS_KB=0
PREV_START=""           # unit start time at the previous check
RESTART_COUNT=0         # restarts seen since the monitor started
ENGINE_LOG=""           # engine log file being followed
ENGINE_LOG_LINES=0      # lines of it already counted
START_TIME=$(date +%s)
CHECK_COUNT=0
ALERT_COUNT=0

log()   { echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" | tee -a "$MONITOR_LOG"; }
# Alerts are counted here, so no check may run in a subshell ($(...)):
# each check sets its result in a global instead.
alert() {
    ALERT_COUNT=$((ALERT_COUNT + 1))
    echo "[ALERT $(date '+%Y-%m-%d %H:%M:%S')] $*" | tee -a "$ALERT_LOG" "$MONITOR_LOG"
}

# ── Checks (each sets its R_* result) ─────────────────────────

check_service_status() {
    R_STATUS="UP"
    if ! systemctl is-active --quiet "$UNIT" 2>/dev/null; then
        R_STATUS="DOWN"
        alert "DEAD: $UNIT is not running"
    fi
}

check_restarts() {
    # A restart changes the time the unit entered the active state.
    local out start
    out=$(systemctl show "$UNIT" -p ActiveEnterTimestampMonotonic 2>/dev/null)
    start="${out#ActiveEnterTimestampMonotonic=}"
    case "$start" in
        ""|0|*[!0-9]*)
            R_RESTARTS="?"
            alert "RESTARTS: cannot read the start time of $UNIT"
            return ;;
    esac
    if [ -n "$PREV_START" ] && [ "$start" != "$PREV_START" ]; then
        RESTART_COUNT=$((RESTART_COUNT + 1))
        PREV_RSS_KB=0   # a new process: do not compare with the old one
        alert "RESTARTED: $UNIT restarted (restarts seen: $RESTART_COUNT)"
    fi
    PREV_START="$start"
    R_RESTARTS="$RESTART_COUNT"
    if [ "$RESTART_COUNT" -gt "$MAX_RESTARTS" ] && [ $((CHECK_COUNT % 60)) -eq 0 ]; then
        alert "RESTARTS: $RESTART_COUNT since the monitor started (max: $MAX_RESTARTS)"
    fi
}

check_memory() {
    local out pid rss_kb
    R_MEMORY="?"
    out=$(systemctl show "$UNIT" -p MainPID 2>/dev/null)
    pid="${out#MainPID=}"
    case "$pid" in
        ""|0|*[!0-9]*)
            PREV_RSS_KB=0
            alert "MEMORY: $UNIT has no main process"
            return ;;
    esac
    rss_kb=$(awk '/^VmRSS:/{print $2}' "/proc/$pid/status" 2>/dev/null)
    case "$rss_kb" in
        ""|*[!0-9]*)
            PREV_RSS_KB=0
            alert "MEMORY: cannot read RSS of pid $pid"
            return ;;
    esac
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
    R_MEMORY="${rss_mb}MB (${rss_kb}KB)"
}

check_api_health() {
    local http_code
    R_API="OK"
    # /health answers 503 when the engine does not respond in time.
    http_code=$(curl -s -o /dev/null -w '%{http_code}' \
        --connect-timeout 5 --max-time 10 \
        "http://localhost:${PORT}/health" 2>/dev/null)
    if [ "$http_code" != "200" ]; then
        R_API="FAIL"
        alert "API: /health returned HTTP ${http_code:-none}"
    fi
}

# Count the ERROR lines of `$1` after line `$2`; print "<count> <total lines>".
# Prints nothing if the file cannot be read.
errors_after() {
    local total count
    [ -r "$1" ] || return 1
    total=$(wc -l < "$1" 2>/dev/null)
    is_number "$total" || return 1
    if [ "$total" -lt "$2" ]; then set -- "$1" 0; fi   # the file was replaced
    # `grep -c` prints the count and exits 1 when it is 0.
    count=$(tail -n "+$(($2 + 1))" "$1" 2>/dev/null | grep -c ' ERROR ')
    is_number "$count" || return 1
    echo "$count $total"
}

check_log_errors() {
    # ERROR lines the engine wrote since the last check. The engine names
    # its files by date, so the newest is the last one by name (mtime is
    # not used: a copied or compressed old file must not be picked).
    local newest res count=0
    R_ERRORS="?"
    newest=$(ls -1 "$ENGINE_LOG_DIR"/sandstar-engine.log.[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] 2>/dev/null | sort | tail -1)
    if [ -z "$newest" ]; then
        alert "LOG: no engine log file in $ENGINE_LOG_DIR"
        return
    fi
    if [ -z "$ENGINE_LOG" ]; then
        # First file seen: start at its current end.
        if ! res=$(errors_after "$newest" 0); then
            alert "LOG: cannot read $newest"
            return
        fi
        ENGINE_LOG="$newest"
        ENGINE_LOG_LINES="${res#* }"
        R_ERRORS=0
        return
    fi
    if [ "$newest" != "$ENGINE_LOG" ]; then
        # The daily file changed: first the rest of the old one.
        if res=$(errors_after "$ENGINE_LOG" "$ENGINE_LOG_LINES"); then
            count="${res%% *}"
        else
            alert "LOG: cannot read the rest of $ENGINE_LOG"
        fi
        ENGINE_LOG="$newest"
        ENGINE_LOG_LINES=0
    fi
    if ! res=$(errors_after "$ENGINE_LOG" "$ENGINE_LOG_LINES"); then
        alert "LOG: cannot read $ENGINE_LOG"
        return
    fi
    count=$((count + ${res%% *}))
    ENGINE_LOG_LINES="${res#* }"
    R_ERRORS="$count"
    if [ "$count" -gt 0 ]; then
        alert "LOG_ERRORS: $count ERROR lines in the engine log since the last check"
    fi
}

# ── Main loop ─────────────────────────────────────────────────
finish() {
    log "Soak monitor stopped: ${CHECK_COUNT} checks, ${ALERT_COUNT} alerts"
    if [ "$ALERT_COUNT" -eq 0 ]; then exit 0; else exit 1; fi
}
trap finish INT TERM

log "Soak monitor started: unit=${UNIT}, port=${PORT}, interval=${INTERVAL}s, duration=${DURATION}"
log "Thresholds: mem=${MEM_MAX_MB}MB, growth=${MEM_GROWTH_MAX_KB}KB, restarts=${MAX_RESTARTS}"

while true; do
    elapsed=$(( $(date +%s) - START_TIME ))
    if [ "$DURATION_SECS" -gt 0 ] && [ "$elapsed" -ge "$DURATION_SECS" ]; then
        log "Duration reached ($DURATION). Stopping."
        break
    fi
    CHECK_COUNT=$((CHECK_COUNT + 1))

    check_service_status
    check_restarts
    check_memory
    check_api_health
    check_log_errors

    hours=$((elapsed / 3600))
    mins=$(( (elapsed % 3600) / 60 ))
    log "CHECK #${CHECK_COUNT} [${hours}h${mins}m] status=${R_STATUS} api=${R_API} mem=${R_MEMORY} restarts=${R_RESTARTS} errors=${R_ERRORS}"

    if [ $((CHECK_COUNT % 60)) -eq 0 ]; then
        log "=== SUMMARY: ${CHECK_COUNT} checks, ${ALERT_COUNT} alerts, uptime ${hours}h${mins}m ==="
    fi

    sleep "$INTERVAL"
done

finish
