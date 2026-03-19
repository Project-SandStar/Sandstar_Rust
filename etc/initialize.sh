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
config-pin P9.12 gpio 2>/dev/null || true
echo 60 > /sys/class/gpio/export 2>/dev/null || true
sleep 0.1
echo out > /sys/class/gpio/gpio60/direction 2>/dev/null || true
echo 0 > /sys/class/gpio/gpio60/value 2>/dev/null || true
chown eacio:root /sys/class/gpio/gpio60/value 2>/dev/null || true
chown eacio:root /sys/class/gpio/gpio60/direction 2>/dev/null || true

echo "Sandstar hardware initialization complete"
