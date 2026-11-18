#!/bin/sh
# initialize.sh - Sandstar Rust engine hardware initialization
# Run via ExecStartPre=+ (as root) in sandstar-engine.service

set -e

# Clean up stale PID file from previous crash
rm -f /var/run/sandstar/sandstar-engine.pid

# ── I2C Configuration ──
config-pin P9.19 i2c 2>/dev/null || true
config-pin P9.20 i2c 2>/dev/null || true
chown eacio:root /dev/i2c-* 2>/dev/null || true

# ── UART Configuration ──
config-pin P9.21 uart 2>/dev/null || true
config-pin P9.22 uart 2>/dev/null || true
config-pin P9.24 uart 2>/dev/null || true
config-pin P9.26 uart 2>/dev/null || true

# ── PWM Configuration ──
config-pin P9.14 pwm 2>/dev/null || true
config-pin P8.19 pwm 2>/dev/null || true
config-pin P9.16 pwm 2>/dev/null || true
config-pin P8.13 pwm 2>/dev/null || true

sleep 1

# Export PWM channels
echo 0 > /sys/class/pwm/pwmchip4/export 2>/dev/null || true
echo 1 > /sys/class/pwm/pwmchip4/export 2>/dev/null || true
echo 0 > /sys/class/pwm/pwmchip7/export 2>/dev/null || true
echo 1 > /sys/class/pwm/pwmchip7/export 2>/dev/null || true

sleep 0.5

# Fix PWM sysfs ownership
chown -R eacio:root /sys/class/pwm/pwmchip4/pwm-4:0/ 2>/dev/null || true
chown -R eacio:root /sys/class/pwm/pwmchip4/pwm-4:1/ 2>/dev/null || true
chown -R eacio:root /sys/class/pwm/pwmchip7/pwm-7:0/ 2>/dev/null || true
chown -R eacio:root /sys/class/pwm/pwmchip7/pwm-7:1/ 2>/dev/null || true

# ── GPIO60 Hardware Watchdog (TPL5010) ──
# On some starts GPIO60 is missing when the engine opens it (R-42). The
# result is printed so it reaches the journal; export is retried. Never
# fails the start: the engine runs (and retries GPIO60) without it.
GPIO60=/sys/class/gpio/gpio60
config-pin P9.12 gpio 2>/dev/null || true
if [ -d "$GPIO60" ]; then
    echo "GPIO60: already exported"
else
    n=0
    err=""
    while [ ! -d "$GPIO60" ] && [ "$n" -lt 5 ]; do
        n=$((n + 1))
        err=$( { echo 60 > /sys/class/gpio/export; } 2>&1 ) || true
        sleep 0.2
    done
    if [ -d "$GPIO60" ]; then
        echo "GPIO60: exported (attempt $n)"
    else
        echo "GPIO60: export failed after $n attempts: ${err:-no error text}; pinmux: $(config-pin -q P9.12 2>&1 || true)" >&2
    fi
fi
if [ -d "$GPIO60" ]; then
    { echo out > "$GPIO60/direction"; } 2>/dev/null || echo "GPIO60: cannot set direction" >&2
    { echo 0 > "$GPIO60/value"; } 2>/dev/null || true
    chown eacio:root "$GPIO60/value" "$GPIO60/direction" 2>/dev/null || true
fi

echo "Sandstar hardware initialization complete"
