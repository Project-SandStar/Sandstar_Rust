# Hardcoded Limits Audit

Comprehensive list of all hardcoded limits and constants across the Sandstar Rust codebase.
Audited: 2026-03-10

## Legend

- **Keep**: Correct as a compile-time constant. Changing would require protocol/spec changes.
- **Configurable**: Already exposed via CLI flag or environment variable.
- **Consider**: Could benefit from being configurable in a future release.
- **Document**: Acceptable as-is but should be documented for operators.

---

## REST API & HTTP Server

| Constant | Value | File | Line | Recommendation |
|----------|-------|------|------|----------------|
| `MAX_WATCHES` | 64 | `sandstar-server/src/cmd_handler.rs` | 30 | **Consider** — operators with many clients may need more. CLI flag `--max-watches`. |
| `MAX_CHANNELS_PER_WATCH` | 256 | `sandstar-server/src/cmd_handler.rs` | 34 | **Document** — generous for typical use. Keep hardcoded. |
| `WATCH_LEASE_SECS` | 3600 (1h) | `sandstar-server/src/cmd_handler.rs` | 27 | **Document** — matches Haystack convention. Keep hardcoded. |
| `MAX_PARSE_DEPTH` | 32 | `sandstar-server/src/rest/filter.rs` | 79 | **Keep** — security limit to prevent stack overflow in filter parser. |
| Rate limit default | 100 req/s | `sandstar-server/src/args.rs` | 65 | **Configurable** — already via `--rate-limit` / `SANDSTAR_RATE_LIMIT`. |
| HTTP port default | 8085 | `sandstar-server/src/args.rs` | 43 | **Configurable** — already via `--http-port` / `SANDSTAR_HTTP_PORT`. |
| HTTP bind default | 127.0.0.1 | `sandstar-server/src/args.rs` | 47 | **Configurable** — already via `--http-bind` / `SANDSTAR_HTTP_BIND`. |
| CORS max-age | 3600s (1h) | `sandstar-server/src/rest/mod.rs` | 724 | **Document** — standard value. Keep hardcoded. |

## WebSocket

| Constant | Value | File | Line | Recommendation |
|----------|-------|------|------|----------------|
| `MAX_WS_CONNECTIONS` | 32 | `sandstar-server/src/rest/ws.rs` | 23 | **Consider** — may need tuning for multi-client deployments. |
| `MIN_POLL_INTERVAL_MS` | 200ms | `sandstar-server/src/rest/ws.rs` | 24 | **Keep** — floor to prevent CPU overload. |
| `MAX_POLL_INTERVAL_MS` | 60,000ms (1min) | `sandstar-server/src/rest/ws.rs` | 25 | **Keep** — ceiling to ensure eventual delivery. |
| `DEFAULT_POLL_INTERVAL_MS` | 1,000ms | `sandstar-server/src/rest/ws.rs` | 26 | **Document** — sensible default for building automation. |
| `CLIENT_TIMEOUT_SECS` | 120s | `sandstar-server/src/rest/ws.rs` | 27 | **Document** — idle WS client disconnect timeout. |

## Authentication (SCRAM-SHA-256)

| Constant | Value | File | Line | Recommendation |
|----------|-------|------|------|----------------|
| `DEFAULT_ITERATIONS` | 10,000 | `sandstar-server/src/auth.rs` | 31 | **Document** — PBKDF2 iteration count. RFC 5802 minimum is 4096. |
| `NONCE_LEN` | 24 bytes | `sandstar-server/src/auth.rs` | 32 | **Keep** — cryptographic parameter. |
| `HANDSHAKE_TIMEOUT_SECS` | 30s | `sandstar-server/src/auth.rs` | 33 | **Keep** — prevents hanging auth handshakes. |
| `SESSION_LIFETIME_SECS` | 86,400 (24h) | `sandstar-server/src/auth.rs` | 34 | **Consider** — operators may want shorter sessions for security. |
| `MAX_SESSIONS` | 256 | `sandstar-server/src/auth.rs` | 35 | **Consider** — may need tuning for high-traffic deployments. |

## Engine Core

| Constant | Value | File | Line | Recommendation |
|----------|-------|------|------|----------------|
| `MAX_LEVELS` | 17 | `sandstar-engine/src/priority.rs` | 15 | **Keep** — matches BACnet priority array (levels 1-17). Protocol constant. |
| `MAX_WHO_LEN` | 16 | `sandstar-engine/src/priority.rs` | 18 | **Keep** — writer identity string cap. Sufficient for identifiers. |
| `MAX_STAGES` | 16 | `sandstar-engine/src/sequencer.rs` | 8 | **Keep** — max stages in a lead sequencer. 16 matches typical HVAC systems. |
| `RETRY_COOLDOWN` | 30 polls | `sandstar-engine/src/engine.rs` | 33 | **Document** — polls before retrying a failed channel. |
| `CONSECUTIVE_FAIL_THRESHOLD` | 5 | `sandstar-engine/src/engine.rs` | 36 | **Document** — consecutive failures before marking channel as errored. |
| `POLL_RETRY_INTERVAL` | 30 polls | `sandstar-engine/src/poll.rs` | 7 | **Document** — mirrors `RETRY_COOLDOWN`. |
| `SMOOTH_BUFFER_MAX` | 10 | `sandstar-engine/src/conversion/filters.rs` | 11 | **Keep** — max smoothing window size. Matches C engine. |

## Sensor & Conversion Constants

| Constant | Value | File | Line | Recommendation |
|----------|-------|------|------|----------------|
| `I2C_RAW_MAX` | 32,767.0 | `sandstar-engine/src/channel.rs` | 10 | **Keep** — 15-bit I2C ADC range. Hardware constant. |
| `ZERO_DROP_BASELINE` | 50.0 | `sandstar-engine/src/channel.rs` | 13 | **Document** — threshold for zero-drop detection. |
| `SDP810_SPIKE_RATIO` | 5.0 | `sandstar-engine/src/channel.rs` | 16 | **Document** — spike rejection ratio for SDP810 sensor. |
| `DEFAULT_DEAD_BAND` | 5.0 Pa | `sandstar-engine/src/conversion/sdp610.rs` | 20 | **Document** — SDP610 dead band. |
| `DEFAULT_K_FACTOR` | 14,000.0 | `sandstar-engine/src/conversion/sdp610.rs` | 23 | **Document** — SDP610 flow coefficient. |
| `DEFAULT_HYST_ON` | 16.0 Pa | `sandstar-engine/src/conversion/sdp610.rs` | 26 | **Document** — SDP610 fan hysteresis on threshold. |
| `DEFAULT_HYST_OFF` | 8.0 Pa | `sandstar-engine/src/conversion/sdp610.rs` | 29 | **Document** — SDP610 fan hysteresis off threshold. |
| `DEFAULT_SCALE_FACTOR` | 60.0 | `sandstar-engine/src/conversion/sdp610.rs` | 32 | **Document** — SDP610 scaling factor. |

## HAL (Hardware Abstraction — Linux)

| Constant | Value | File | Line | Recommendation |
|----------|-------|------|------|----------------|
| `MAX_RETRIES` | 3 | `sandstar-hal-linux/src/i2c.rs` | 38 | **Keep** — I2C retry count before failure. |
| `RETRY_BASE_MS` | 10ms | `sandstar-hal-linux/src/i2c.rs` | 41 | **Keep** — I2C retry backoff base. |
| `SDP810_TRIGGER_DELAY_MS` | 45ms | `sandstar-hal-linux/src/i2c.rs` | 45 | **Keep** — sensor datasheet timing. |
| `SDP810_RESET_DELAY_MS` | 20ms | `sandstar-hal-linux/src/i2c.rs` | 49 | **Keep** — sensor datasheet timing. |
| `MAX_PORTS` | 8 | `sandstar-hal-linux/src/uart.rs` | 46 | **Keep** — max UART ports on BeagleBone. |
| `READ_RETRIES` | 10 | `sandstar-hal-linux/src/uart.rs` | 35 | **Keep** — UART read retry attempts. |
| `READ_RETRY_DELAY_MS` | 100ms | `sandstar-hal-linux/src/uart.rs` | 39 | **Keep** — UART retry delay. |
| `RX_BUF_SIZE` | 64 bytes | `sandstar-hal-linux/src/uart.rs` | 43 | **Keep** — UART receive buffer. |

## IPC & Networking

| Constant | Value | File | Line | Recommendation |
|----------|-------|------|------|----------------|
| `MAX_FRAME_SIZE` | 1,048,576 (1MB) | `sandstar-ipc/src/protocol.rs` | 11 | **Document** — max IPC message size. Prevents unbounded allocation. |
| IPC read timeout | 5s | `sandstar-server/src/ipc.rs` | 58,68 | **Document** — prevents blocking IPC reads indefinitely. |
| Command channel capacity | 64 | `sandstar-server/src/main.rs` | 344 | **Document** — mpsc channel buffer for REST commands. |

## Sedona VM (SVM)

| Constant | Value | File | Line | Recommendation |
|----------|-------|------|------|----------------|
| `STACK_SIZE` | 65,536 (64KB) | `sandstar-svm/src/runner.rs` | 104 | **Keep** — generous for ARM. Matches Sedona VM spec. |
| SVM yield sleep | 10ms | `sandstar-svm/src/runner.rs` | 149 | **Keep** — Sedona yield period. |
| SVM hibernate sleep | 100ms | `sandstar-svm/src/runner.rs` | 154 | **Keep** — Sedona hibernate period. |
| SVM error retry sleep | 1s | `sandstar-svm/src/runner.rs` | 173 | **Keep** — cooldown after SVM error. |
| `ERR_YIELD` | 253 | `sandstar-svm/src/types.rs` | 77 | **Keep** — Sedona VM protocol constant. |
| `ERR_RESTART` | 254 | `sandstar-svm/src/types.rs` | 78 | **Keep** — Sedona VM protocol constant. |
| `ERR_HIBERNATE` | 255 | `sandstar-svm/src/types.rs` | 79 | **Keep** — Sedona VM protocol constant. |

## Control Engine

| Constant | Value | File | Line | Recommendation |
|----------|-------|------|------|----------------|
| Default write level | 8 | `sandstar-server/src/control.rs` | 119-120 | **Keep** — BACnet priority 8 (manual operator). Standard default. |
| Default PID kp | 1.0 | `sandstar-server/src/control.rs` | 122-123 | **Keep** — safe default proportional gain. |
| Default PID max output | 100.0 | `sandstar-server/src/control.rs` | 125-126 | **Keep** — 0-100% output range. |
| Default PID bias | 50.0 | `sandstar-server/src/control.rs` | 128-129 | **Keep** — midpoint output bias. |
| Default PID interval | 1000ms | `sandstar-server/src/control.rs` | 134-135 | **Keep** — 1 Hz PID execution rate. |
| Default sequencer hysteresis | 0.5 | `sandstar-server/src/control.rs` | 137-138 | **Keep** — prevents stage oscillation. |
| Poll interval default | 1000ms | `sandstar-server/src/args.rs` | 31 | **Configurable** — already via `--poll-interval-ms`. |

## Server Infrastructure

| Constant | Value | File | Line | Recommendation |
|----------|-------|------|------|----------------|
| Watch expiry timer | 60s | `sandstar-server/src/main.rs` | 316 | **Document** — periodic stale watch cleanup interval. |
| Poll result timeout | 5s | `sandstar-server/src/main.rs` | 705 | **Document** — max wait for in-flight poll during shutdown. |
| Slow poll warning threshold | 1s | `sandstar-server/src/main.rs` | 560 | **Document** — logs warning when poll exceeds 1 second. |

---

## Summary: Candidates for Future CLI Flags

These limits are the best candidates for runtime configurability if operators need tuning:

1. **`MAX_WATCHES`** (64) — `--max-watches`
2. **`MAX_WS_CONNECTIONS`** (32) — `--max-ws-connections`
3. **`MAX_SESSIONS`** (256) — `--max-sessions`
4. **`SESSION_LIFETIME_SECS`** (86,400) — `--session-lifetime`

All other limits are either already configurable, protocol/hardware constants, or safety
guards that should not be changed without careful analysis.
