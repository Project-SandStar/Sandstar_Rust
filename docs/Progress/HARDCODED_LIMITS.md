# Hardcoded Limits Audit

> **Status (2026-11-18)**: re-checked against `master` `b48484b`. Most values are unchanged, but many line numbers moved. Search by constant name rather than trusting the `file:line` below.
>
> **Moved (same value):**
> - `MAX_PARSE_DEPTH` → `rest/filter.rs:115`; CORS max-age → `rest/mod.rs:1217` (`with_global_layers`, S-35); `ws.rs` constants → lines 29-33 (S-22).
> - `CONSECUTIVE_FAIL_THRESHOLD` → `engine.rs:42`. The code comment describes it as the count before the fail counter resets.
> - `ERR_YIELD` / `ERR_RESTART` / `ERR_HIBERNATE` → `sandstar-svm/src/types.rs:72-74`.
> - `args.rs`: rate limit line 75, port 53, bind 57.
> - `main.rs`: command channel 423, watch expiry 380, poll result timeout 1020, slow-poll warning 873.
>
> **Changed or removed:**
> - `RETRY_COOLDOWN` is gone. It is now `RETRY_COOLDOWN_BASE = 30` and `RETRY_COOLDOWN_MAX = 300` with exponential backoff (`engine.rs:35,39`).
> - `sandstar-svm/src/runner.rs` was deleted with the C path. Its `STACK_SIZE` and sleep constants no longer exist. The unused `VmConfig.max_stack_size` is 64 KB, while the interpreter hardcodes a 4096-cell (16 KB) stack.
>
> **Added since the audit:**
> - `MAX_SESSIONS = 4` in `sox/dasp.rs:35`. Different from `auth.rs` `MAX_SESSIONS = 256`.
> - `CLIENT_TIMEOUT_SECS` also in `rest/live.rs` and `rest/rows.rs`.
> - Driver write level 16, duration 30 s and poll interval 5 s (`drivers/loader.rs`).
> - MQTT cache max age 600 s; BACnet discovery 2 s, COV renewal 240 s, foreign-device TTL 300 s, COV cache 600 s.
> - Request body limit 1 MB (`rest/mod.rs:1231`, `with_global_layers`); since S-35 it also covers the tags, alerts, roxWarp and simulator routes, which had axum's 2 MB `Json` default before. Driver actor channel capacity 64.
> - Log files kept: 14 by default (`--log-max-files`, `logging.rs`).
> - roxWarp timer bounds: 1..=86400 s (`roxwarp/cluster.rs`).
> - `--cluster-insecure` default off (`args.rs`, R-54, S-36): without it and without mTLS certificates the cluster listener and peer connections stay off. CLI only; the cluster JSON config cannot set it (`ClusterConfig.allow_plain` is `serde(skip)`).
> - roxWarp `PEER_ANOMALY_WARN_INTERVAL_MS` = 60,000 ms (`roxwarp/cluster.rs:225`, R-45/R-27, 2026-11-19): at most one `warn` per minute about bad peer points (NaN/inf value, future timestamp); the rest go to `debug`. Keep hardcoded.
> - roxWarp `MAX_PEER_FUTURE_SKEW_MS` = 300,000 ms (5 min) (`roxwarp/cluster.rs:230`, R-27, 2026-11-19): a peer point whose timestamp is further ahead of the local clock is rejected (logged, rate-limited), so a peer cannot pin a channel under last-writer-wins. **Consider** making it configurable if field clocks drift more.
> - roxWarp `MAX_FILTER_DEPTH` = 32 (`roxwarp/handler.rs:611`, S-24): deepest `and`/`or` nesting evaluated in a point filter (REST cluster query, roxWarp `query`); terms past it do not match. Without it a long chain overflowed the stack and killed the process. Keep hardcoded.
> - BACnet `MALFORMED_WARN_INTERVAL_SECS` = 60 s (`drivers/bacnet/mod.rs:79`, S-24): at most one `warn` per minute for frames that fail to decode; the rest go to `debug`. Keep hardcoded.
> - `MAX_WRITE_DURATION_SECS` = 31,536,000 s (1 year) (`sandstar-engine/src/priority.rs:25`, R-10/S-25): longest timed priority write. `channel_write_level` rejects a longer, NaN or infinite duration (REST `pointWrite` answers 400); a negative duration is still a permanent write. Without it a huge `duration` panicked in `Duration::from_secs_f64` and killed the process. Keep hardcoded.
> - `MAX_INTERNED_NAMES` = 65,534 (`sandstar-server/src/sox/name_intern.rs:25`, R-47/S-26): most SOX component names the intern table holds. When full, names no component carries any more (left by renames and removals) are dropped and the new name is interned; only a table full of names in use refuses it (one `warn`), and components keep their names either way. Persistence writes only names still in use. Before S-26 the 65,535th name hit an `assert!` and killed the process.
> - `MAX_INTERNED_TAG_NAMES` = 65,536 (`sandstar-server/src/sox/dyn_slots.rs:76`, R-47/S-26): most dynamic tag names the interner holds (the whole `u16` id space). When full, new names are not interned (one `warn`); only accepted keys are interned. Before S-26 the id wrapped to 0 in release.
> - `/live` `LOG_PREVIEW_BYTES` = 240 (`rest/live.rs:57`, R-6/S-24): RX/TX log preview, cut back to a UTF-8 character boundary.
> - GPIO60 retry every 20 watchdog kicks, at most 30 retries (`watchdog.rs`).
> - systemd unit: `WatchdogSec=10`, `MemoryLimit=128M`.

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
| CORS max-age | 3600s (1h) | `sandstar-server/src/rest/mod.rs` | 1217 | **Document** — standard value. Keep hardcoded. |

## WebSocket

| Constant | Value | File | Line | Recommendation |
|----------|-------|------|------|----------------|
| `MAX_WS_CONNECTIONS` | 32 | `sandstar-server/src/rest/ws.rs` | 23 | **Consider** — may need tuning for multi-client deployments. |
| `MIN_POLL_INTERVAL_MS` | 200ms | `sandstar-server/src/rest/ws.rs` | 24 | **Keep** — floor to prevent CPU overload. |
| `MAX_POLL_INTERVAL_MS` | 60,000ms (1min) | `sandstar-server/src/rest/ws.rs` | 25 | **Keep** — ceiling to ensure eventual delivery. |
| `DEFAULT_POLL_INTERVAL_MS` | 1,000ms | `sandstar-server/src/rest/ws.rs` | 26 | **Document** — sensible default for building automation. |
| `CLIENT_TIMEOUT_SECS` | 120s | `sandstar-server/src/rest/ws.rs` | 27 | **Document** — idle WS client disconnect timeout. |
| `PUSH_BACKOFF_AFTER_TIMEOUT` | 10s (`ENGINE_REPLY_TIMEOUT`) | `sandstar-server/src/rest/ws.rs` | 37 | **Keep** — after an engine timeout a connection starts its next push round no earlier than this (R-28, S-22). |
| `CLIENT_QUEUE_CAPACITY` | 16 requests | `sandstar-server/src/rest/ws.rs` | 41 | **Keep** — client engine requests (`subscribe`, `unsubscribe`, `refresh`) a connection queues for its single engine slot; when full the connection stops reading until one finishes (R-28, S-23). |
| `MAX_CLIENT_MESSAGE_BYTES` | 16 KiB | `sandstar-server/src/rest/ws.rs` | 46 | **Keep** — largest `/api/ws` client message and frame; larger closes the connection (library default 64 MiB). A subscribe with 256 ids is under 4 KiB (S-23). |
| `CLEANUP_QUEUE_CAPACITY` | 256 watch ids | `sandstar-server/src/rest/ws.rs` | 50 | **Keep** — shared queue of watches to close after disconnect; overflow is logged and left to the lease (S-22). |
| `CLEANUP_RETRY_DELAY` | 10s | `sandstar-server/src/rest/ws.rs` | 54 | **Keep** — first pause before retrying a timed-out `watch_unsub`; doubles on each further timeout (S-22). |
| `CLEANUP_RETRY_MAX_DELAY` | 600s | `sandstar-server/src/rest/ws.rs` | 57 | **Keep** — longest pause between cleanup attempts for one watch; about ten attempts within the hour (S-22). |
| `CLEANUP_MAX_AGE` | 3600s | `sandstar-server/src/rest/ws.rs` | 61 | **Keep** — cleanup gives up on a watch after this; equals `WATCH_LEASE_SECS` (S-22). |
| `CLEANUP_FULL_WARN_INTERVAL_SECS` | 60s | `sandstar-server/src/rest/ws.rs` | 64 | **Keep** — at most one "cleanup queue full" `warn` per interval (S-22). |

## Authentication (SCRAM-SHA-256)

| Constant | Value | File | Line | Recommendation |
|----------|-------|------|------|----------------|
| `DEFAULT_ITERATIONS` | 10,000 | `sandstar-server/src/auth.rs` | 31 | **Document** — PBKDF2 iteration count. RFC 5802 minimum is 4096. |
| `NONCE_LEN` | 24 bytes | `sandstar-server/src/auth.rs` | 32 | **Keep** — cryptographic parameter. |
| `HANDSHAKE_TIMEOUT_SECS` | 30s | `sandstar-server/src/auth.rs` | 33 | **Keep** — prevents hanging auth handshakes. |
| `SESSION_LIFETIME_SECS` | 86,400 (24h) | `sandstar-server/src/auth.rs` | 34 | **Consider** — operators may want shorter sessions for security. |
| `MAX_SESSIONS` | 256 | `sandstar-server/src/auth.rs` | 35 | **Consider** — may need tuning for high-traffic deployments. |
| `SESSION_COOKIE` | `sandstar_session` | `sandstar-server/src/rest/mod.rs` | — | **Keep** — set by `POST /api/auth` on SCRAM success (`HttpOnly; SameSite=Strict; Path=/`, no `Secure` because the device serves plain HTTP). Accepted on `/live`, `/live/sedonaState`, `/api/rows`, `/api/ws` (S-32). |
| WS allowed origins | empty (same host only) | `sandstar-server/src/args.rs` | — | **Configurable** — `--ws-allowed-origins` / `SANDSTAR_WS_ALLOWED_ORIGINS` (comma separated). Only checked when auth is on (S-32). |

## Engine Core

| Constant | Value | File | Line | Recommendation |
|----------|-------|------|------|----------------|
| `MAX_LEVELS` | 17 | `sandstar-engine/src/priority.rs` | 15 | **Keep** — matches BACnet priority array (levels 1-17). Protocol constant. |
| `MAX_WRITE_DURATION_SECS` | 31,536,000 (1 year) | `sandstar-engine/src/priority.rs` | 25 | **Keep** — longest timed write; longer, NaN or infinite durations are rejected (S-25). |
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
| `ENGINE_REPLY_TIMEOUT` | 10s | `sandstar-server/src/rest/mod.rs` | 146 | **Document** — max wait for an `EngineHandle` command (send + reply). On timeout the caller gets `engine timeout` (REST: 503); a state-changing command may still run later (R-28, S-20). |
| Engine timeout warn interval | 60s | `sandstar-server/src/rest/mod.rs` | 160 | **Keep** — first timeout per 60 s logged at `warn`, the rest at `debug`. |
| `HEALTH_ENGINE_TIMEOUT` | 2s | `sandstar-server/src/rest/handlers.rs` | 369 | **Document** — `/health` waits at most this long for the engine; otherwise 503 with `healthy:false` (R-28, S-21). |
| `REFRESH_BACKOFF_AFTER_TIMEOUT` | 10s | `sandstar-server/src/sox/mod.rs` | 40 | **Keep** — pause before the next SOX tree refresh after an engine timeout (S-22). |
| `REFRESH_WARN_INTERVAL` | 60s | `sandstar-server/src/sox/mod.rs` | 37 | **Keep** — SOX tree refresh runs every second; a non-timeout failure is logged at `warn` at most once per interval, the rest at `debug` (S-21). |
| `MAX_INTERNED_NAMES` | 65,534 | `sandstar-server/src/sox/name_intern.rs` | 25 | **Keep** — SOX name intern table size; a full table drops unused names, never panics (S-26). |
| `MAX_INTERNED_TAG_NAMES` | 65,536 | `sandstar-server/src/sox/dyn_slots.rs` | 76 | **Keep** — dynamic tag name interner size (whole `u16` space); full interner stops interning (S-26). |
| Command channel capacity | 64 | `sandstar-server/src/main.rs` | 423 | **Document** — mpsc channel buffer for REST commands. |

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
