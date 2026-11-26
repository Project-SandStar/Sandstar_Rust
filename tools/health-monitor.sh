#!/bin/bash
# Sandstar health monitor — runs via cron every 5 minutes.
# Reads /health and /api/diagnostics of the local engine and appends one
# line to the log; state changes get an extra WARNING line.
LOG="${LOG:-/var/log/sandstar/health-monitor.log}"
STATE="${STATE:-/tmp/sandstar-health-last}"
URL="${SANDSTAR_URL:-http://127.0.0.1:8085}"
UNIT="${UNIT:-sandstar-engine}"
TS=$(date -u +%Y-%m-%dT%H:%M:%S)

prev=$(cat "$STATE" 2>/dev/null)

# /health answers 503 when the engine does not respond in time.
code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$URL/health" 2>/dev/null)
if [ "$code" != "200" ]; then
  echo "$TS CRITICAL engine not healthy (http=${code:-none})" >> "$LOG"
  if [ "${prev:0:4}" != "DOWN" ]; then
    echo "$TS WARNING engine went DOWN" >> "$LOG"
    echo "DOWN" > "$STATE"
  fi
  exit 1
fi
if [ "${prev:0:4}" = "DOWN" ]; then
  echo "$TS WARNING engine recovered" >> "$LOG"
fi

# Fields of DiagnosticsInfo (sandstar-ipc). A reply that cannot be read is
# reported; it is not written as zeros.
diag=$(curl -s --max-time 5 "$URL/api/diagnostics" 2>/dev/null)
parsed=$(printf '%s' "$diag" | python3 -c '
import sys, json
d = json.load(sys.stdin)
keys = ("channels_fault", "channels_down", "poll_overrun_count",
        "last_poll_duration_ms", "max_poll_duration_ms")
print(" ".join(str(int(d[k])) for k in keys))
' 2>/dev/null)
if [ -z "$parsed" ]; then
  echo "$TS WARNING diagnostics unreadable" >> "$LOG"
  # Keep the last known fault count, so that a change across this gap is
  # still reported; only a DOWN state is replaced.
  if [ "${prev:0:4}" = "DOWN" ] || [ -z "$prev" ]; then
    echo "UP fault=?" > "$STATE"
  fi
  exit 1
fi
read -r fault down overruns poll_ms max_poll_ms <<< "$parsed"

# Resident memory of the engine process.
pid=$(systemctl show "$UNIT" -p MainPID 2>/dev/null)
pid="${pid#MainPID=}"
mem_kb=$(awk '/^VmRSS:/{print $2}' "/proc/${pid:-0}/status" 2>/dev/null)
mem_kb="${mem_kb:-?}"

echo "$TS OK fault=$fault down=$down overruns=$overruns poll_ms=$poll_ms mem_kb=$mem_kb max_poll_ms=$max_poll_ms" >> "$LOG"

prev_fault=$(printf '%s' "$prev" | sed -n 's/.*fault=\([0-9][0-9]*\).*/\1/p')
if [ -n "$prev_fault" ] && [ "$prev_fault" != "$fault" ]; then
  echo "$TS WARNING channels_fault changed $prev_fault -> $fault" >> "$LOG"
fi

echo "UP fault=$fault" > "$STATE"
