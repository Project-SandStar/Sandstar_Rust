#!/bin/bash
# Sandstar health monitor — runs via cron every 5 minutes
LOG="/var/log/sandstar/health-monitor.log"
STATE="/tmp/sandstar-health-last"
TS=$(date -u +%Y-%m-%dT%H:%M:%S)

health=$(curl -s --max-time 5 http://127.0.0.1:8085/health 2>/dev/null)
if [ $? -ne 0 ] || [ -z "$health" ]; then
  echo "$TS CRITICAL engine not responding" >> "$LOG"
  if [ "$(cat "$STATE" 2>/dev/null)" != "DOWN" ]; then
    echo "$TS WARNING engine went DOWN" >> "$LOG"
    echo "DOWN" > "$STATE"
  fi
  exit 1
fi

diag=$(curl -s --max-time 5 http://127.0.0.1:8085/api/diagnostics 2>/dev/null)
read -r fault down overruns poll_ms mem_kb <<< $(echo "$diag" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    print(d.get('channels_fault',0), d.get('channels_down',0),
          d.get('poll_overrun_count',0), d.get('poll_cycle_ms',0),
          d.get('memory_kb',0))
except: print('0 0 0 0 0')
")

echo "$TS OK fault=$fault down=$down overruns=$overruns poll_ms=$poll_ms mem_kb=$mem_kb" >> "$LOG"

prev_fault=$(cat "$STATE" 2>/dev/null | grep -oP 'fault=\K[0-9]+')
prev_state=$(cat "$STATE" 2>/dev/null | head -c4)
if [ "$prev_state" = "DOWN" ]; then
  echo "$TS WARNING engine recovered" >> "$LOG"
fi
if [ -n "$prev_fault" ] && [ "$prev_fault" != "$fault" ]; then
  echo "$TS WARNING channels_fault changed $prev_fault -> $fault" >> "$LOG"
fi

echo "UP fault=$fault" > "$STATE"
