#!/bin/bash
# validate-engines.sh — Compare C engine vs Rust engine side-by-side
#
# Reads all polled channels from both REST APIs and compares values.
# Logs discrepancies with timestamps and produces periodic summaries.
#
# Usage:
#   ./validate-engines.sh [device_ip] [interval_secs] [duration_mins]
#
# Examples:
#   ./validate-engines.sh 172.28.211.135           # default: 2s interval, run forever
#   ./validate-engines.sh 172.28.211.135 1 60      # 1s interval, stop after 60 minutes
#   ./validate-engines.sh localhost 5 0            # local test, 5s interval, run forever

set -euo pipefail

DEVICE="${1:-172.28.211.135}"
C_PORT=8085
RUST_PORT=8086
INTERVAL="${2:-2}"
DURATION_MINS="${3:-0}"  # 0 = run forever

TIMESTAMP=$(date +%Y%m%d_%H%M%S)
LOG_DIR="/tmp/sandstar_validate"
LOG_FILE="${LOG_DIR}/validate_${TIMESTAMP}.log"
SUMMARY_FILE="${LOG_DIR}/summary_${TIMESTAMP}.txt"

mkdir -p "$LOG_DIR"

# ── Counters ────────────────────────────────────────────────
CYCLES=0
TOTAL_COMPARISONS=0
TOTAL_MATCH=0
TOTAL_CLOSE=0
TOTAL_MISMATCH=0
TOTAL_C_ERROR=0
TOTAL_RUST_ERROR=0

# Tolerance: ADC jitter + timing offset
RAW_TOLERANCE=5.0       # ADC counts
CUR_TOLERANCE=0.5       # Engineering units (absolute)

START_EPOCH=$(date +%s)

# ── Helpers ─────────────────────────────────────────────────
log() { echo "$(date '+%Y-%m-%dT%H:%M:%S') $*" | tee -a "$LOG_FILE"; }
log_only() { echo "$(date '+%Y-%m-%dT%H:%M:%S') $*" >> "$LOG_FILE"; }

check_connectivity() {
    local label=$1 url=$2
    if ! curl -sf -o /dev/null --connect-timeout 3 "$url"; then
        log "FATAL: Cannot reach $label at $url"
        return 1
    fi
    return 0
}

# Compare two floats: returns 0 if within tolerance
within_tolerance() {
    local a=$1 b=$2 tol=$3
    awk -v a="$a" -v b="$b" -v tol="$tol" 'BEGIN {
        diff = a - b; if (diff < 0) diff = -diff;
        exit (diff <= tol) ? 0 : 1
    }'
}

# ── Pre-flight checks ──────────────────────────────────────
log "=== Sandstar Engine Validation ==="
log "C engine:    http://${DEVICE}:${C_PORT}"
log "Rust engine: http://${DEVICE}:${RUST_PORT}"
log "Interval:    ${INTERVAL}s"
log "Log file:    ${LOG_FILE}"
log ""

check_connectivity "C engine" "http://${DEVICE}:${C_PORT}/api/status" || exit 1
check_connectivity "Rust engine" "http://${DEVICE}:${RUST_PORT}/api/status" || exit 1

# Print engine info
C_STATUS=$(curl -sf "http://${DEVICE}:${C_PORT}/api/status" 2>/dev/null || echo '{"error":"unavailable"}')
R_STATUS=$(curl -sf "http://${DEVICE}:${RUST_PORT}/api/status" 2>/dev/null || echo '{"error":"unavailable"}')
log "C engine status:    $C_STATUS"
log "Rust engine status: $R_STATUS"
log ""

# ── Main comparison loop ───────────────────────────────────
compare_cycle() {
    local cycle_match=0 cycle_close=0 cycle_mismatch=0

    # Fetch polls from both engines (JSON format)
    local c_json r_json
    c_json=$(curl -sf "http://${DEVICE}:${C_PORT}/api/polls" 2>/dev/null) || {
        TOTAL_C_ERROR=$((TOTAL_C_ERROR + 1))
        log "ERR  C engine /api/polls failed"
        return
    }
    r_json=$(curl -sf "http://${DEVICE}:${RUST_PORT}/api/polls" 2>/dev/null) || {
        TOTAL_RUST_ERROR=$((TOTAL_RUST_ERROR + 1))
        log "ERR  Rust engine /api/polls failed"
        return
    }

    # Parse channel data: extract channel, lastCur, lastStatus for each entry
    # Use simple grep/sed since jq may not be available on BeagleBone
    # Format: one line per channel: "CHANNEL CUR STATUS"
    local c_channels r_channels

    # Try jq first, fall back to python, fall back to grep
    if command -v jq &>/dev/null; then
        c_channels=$(echo "$c_json" | jq -r '.[] | "\(.channel) \(.lastCur) \(.lastStatus)"' 2>/dev/null) || return
        r_channels=$(echo "$r_json" | jq -r '.[] | "\(.channel) \(.lastCur) \(.lastStatus)"' 2>/dev/null) || return
    elif command -v python3 &>/dev/null; then
        c_channels=$(python3 -c "
import json, sys
for p in json.loads(sys.stdin.read()):
    print(f\"{p['channel']} {p['lastCur']} {p['lastStatus']}\")
" <<< "$c_json" 2>/dev/null) || return
        r_channels=$(python3 -c "
import json, sys
for p in json.loads(sys.stdin.read()):
    print(f\"{p['channel']} {p['lastCur']} {p['lastStatus']}\")
" <<< "$r_json" 2>/dev/null) || return
    else
        log "ERR  No JSON parser available (install jq or python3)"
        return
    fi

    # Build associative array from Rust channels
    declare -A rust_cur rust_status
    while IFS=' ' read -r ch cur status; do
        [ -z "$ch" ] && continue
        rust_cur[$ch]="$cur"
        rust_status[$ch]="$status"
    done <<< "$r_channels"

    # Compare each C channel against Rust
    while IFS=' ' read -r ch c_cur c_status; do
        [ -z "$ch" ] && continue
        TOTAL_COMPARISONS=$((TOTAL_COMPARISONS + 1))

        local r_cur="${rust_cur[$ch]:-MISSING}"
        local r_stat="${rust_status[$ch]:-MISSING}"

        if [ "$r_cur" = "MISSING" ]; then
            log "MISS ch=$ch — not found in Rust engine"
            cycle_mismatch=$((cycle_mismatch + 1))
            continue
        fi

        # Compare status
        if [ "$c_status" != "$r_stat" ]; then
            log "STAT ch=$ch C:status=$c_status Rust:status=$r_stat"
            cycle_mismatch=$((cycle_mismatch + 1))
            continue
        fi

        # Compare cur value
        if within_tolerance "$c_cur" "$r_cur" "$CUR_TOLERANCE"; then
            # Exact or within tolerance
            if within_tolerance "$c_cur" "$r_cur" "0.001"; then
                cycle_match=$((cycle_match + 1))
                log_only "OK   ch=$ch cur=$c_cur status=$c_status"
            else
                cycle_close=$((cycle_close + 1))
                log_only "NEAR ch=$ch C:cur=$c_cur Rust:cur=$r_cur delta=$(awk "BEGIN{d=$c_cur-$r_cur; if(d<0)d=-d; printf \"%.4f\",d}")"
            fi
        else
            cycle_mismatch=$((cycle_mismatch + 1))
            log "DIFF ch=$ch C:cur=$c_cur Rust:cur=$r_cur delta=$(awk "BEGIN{d=$c_cur-$r_cur; if(d<0)d=-d; printf \"%.4f\",d}") status=$c_status"
        fi
    done <<< "$c_channels"

    TOTAL_MATCH=$((TOTAL_MATCH + cycle_match))
    TOTAL_CLOSE=$((TOTAL_CLOSE + cycle_close))
    TOTAL_MISMATCH=$((TOTAL_MISMATCH + cycle_mismatch))
}

print_summary() {
    local elapsed=$(( $(date +%s) - START_EPOCH ))
    local total=$((TOTAL_MATCH + TOTAL_CLOSE + TOTAL_MISMATCH))
    local pct_ok=0
    [ "$total" -gt 0 ] && pct_ok=$(awk "BEGIN{printf \"%.2f\", ($TOTAL_MATCH + $TOTAL_CLOSE) / $total * 100}")

    cat <<EOF | tee -a "$LOG_FILE"

=== Validation Summary (${CYCLES} cycles, ${elapsed}s elapsed) ===
Total comparisons:  $TOTAL_COMPARISONS
  Exact match:      $TOTAL_MATCH
  Within tolerance:  $TOTAL_CLOSE
  MISMATCH:         $TOTAL_MISMATCH
  C engine errors:  $TOTAL_C_ERROR
  Rust engine errors: $TOTAL_RUST_ERROR
  Match rate:       ${pct_ok}%
===============================================

EOF
}

# Trap Ctrl+C for clean exit with summary
trap 'echo ""; print_summary; exit 0' INT TERM

log "Starting comparison loop (Ctrl+C to stop and see summary)..."
log ""

while true; do
    CYCLES=$((CYCLES + 1))
    compare_cycle

    # Print summary every 60 cycles
    if [ $((CYCLES % 60)) -eq 0 ]; then
        print_summary
    fi

    # Check duration limit
    if [ "$DURATION_MINS" -gt 0 ]; then
        local_elapsed=$(( $(date +%s) - START_EPOCH ))
        if [ "$local_elapsed" -ge $((DURATION_MINS * 60)) ]; then
            log "Duration limit reached (${DURATION_MINS} minutes)"
            print_summary
            exit 0
        fi
    fi

    sleep "$INTERVAL"
done
