# 15. ROX Protocol: Trio-over-WebSocket & Northbound Clustering

## Overview

This document analyzes the current SOX/DASP/UDP communication stack in the Sedona Framework, documents its limitations, and presents the **ROX** (Rust Object eXchange) protocol — a modern WebSocket-based replacement using **Trio encoding** for human-readable messages and SCRAM-SHA-256 authentication. SOX is retained for backward compatibility; ROX operates alongside it on a separate port.

Additionally, it details a northbound clustering architecture where multiple Sandstar devices form a secured mesh via mutual-TLS WebSocket connections using RSA keys. The cluster gossip protocol (**roxWarp**) uses a binary Trio diff encoding for efficient state synchronization — see [16_ROXWARP_PROTOCOL.md](16_ROXWARP_PROTOCOL.md) for the full roxWarp specification.

**This is an ongoing enhancement report, continuing from documents 12-14 on Sedona VM architecture, porting strategy, and scalability limits.**

**Key Deliverables:**
1. Define **ROX** protocol: Trio-over-WebSocket for device communication (coexists with SOX/UDP)
2. Implement SCRAM-SHA-256 authentication (Haxall-compatible)
3. Add northbound clustering via secured WebSocket mesh with RSA certificate-based mutual TLS
4. Define **roxWarp** gossip protocol for binary Trio diff exchange (see doc 16)
5. All implemented in Rust using `axum`, `tokio-tungstenite`, `rustls`, `scram-rs`, and `rmp-serde`

---

## Table of Contents

1. [Current SOX/DASP/UDP Architecture](#1-current-soxdaspudp-architecture)
2. [Problems with SOX/DASP/UDP](#2-problems-with-soxdaspudp)
3. [Existing WebSocket Bridge (bassgSoxWebSocket)](#3-existing-websocket-bridge-bassgsoxwebsocket)
4. [New Architecture: ROX Protocol (Trio-over-WebSocket)](#4-new-architecture-rox-protocol-trio-over-websocket)
5. [SCRAM-SHA-256 Authentication (Haxall-Compatible)](#5-scram-sha-256-authentication-haxall-compatible)
6. [Rust Implementation: Southbound (ROX Device Communication)](#6-rust-implementation-southbound-rox-device-communication)
7. [Northbound Clustering via Secured WebSocket (roxWarp)](#7-northbound-clustering-via-secured-websocket-roxwarp)
8. [RSA Key Management & Certificate Infrastructure](#8-rsa-key-management--certificate-infrastructure)
9. [Rust Implementation: roxWarp Cluster Node](#9-rust-implementation-roxwarp-cluster-node)
10. [Wire Protocol Specification](#10-wire-protocol-specification)
11. [Migration Path](#11-migration-path)
12. [Dependency Summary](#12-dependency-summary)

---

## 1. Current SOX/DASP/UDP Architecture

### 1.1 Protocol Stack

```
┌──────────────────────────┐
│  SOX (Sedona Object      │  Application protocol
│  eXchange)               │  Commands: v, c, s, u, e, w, a, d, ...
├──────────────────────────┤
│  DASP (Datagram          │  Session layer
│  Authenticated Session   │  Authentication, reliability, flow control
│  Protocol)               │
├──────────────────────────┤
│  UDP                     │  Transport
│  Port 1876 (default)     │  Unreliable datagrams
└──────────────────────────┘
```

### 1.2 SOX Command Format

Every SOX message has a 2-byte header:

```
Offset  Size  Field       Description
──────  ────  ─────       ───────────
0       1     command     ASCII letter: lowercase=request, uppercase=response
1       1     replyNum    Correlates request with response (0-254, 0xFF=none)
2+      var   payload     Command-specific binary data
```

**Source:** `EacIo/src/sedona/src/sedona/sox/Msg.java`
```java
static Msg prepareRequest(int cmd, int replyNum) {
    Msg msg = new Msg();
    msg.u1(cmd);       // command byte
    msg.u1(replyNum);  // correlation ID
    return msg;
}
```

### 1.3 SOX Commands (Complete)

| Cmd | Response | Name           | Payload Format |
|-----|----------|----------------|----------------|
| `v` | `V`      | readVersion    | → (empty); ← kit count + kit names/checksums |
| `c` | `C`      | readComp       | → u2 compId, u1 what ('t'/'c'/'r'/'l'); ← component data |
| `s` | `S`      | subscribe      | → u2 compId, u1 whatMask; ← success |
| `u` | `U`      | unsubscribe    | → u2 compId, u1 whatMask; ← success |
| `e` | —        | event (COV)    | ← u2 compId + changed data (server push) |
| `w` | `W`      | write          | → u2 compId, u1 slotId, value; ← success |
| `a` | `A`      | add            | → u2 parentId, u1 kitId, u1 typeId, str name; ← u2 newCompId |
| `d` | `D`      | delete         | → u2 compId; ← success |
| `r` | `R`      | rename         | → u2 compId, str newName; ← success |
| `o` | `O`      | reorder        | → u2 compId, u2[] newOrder; ← success |
| `k` | `K`      | invoke         | → u2 compId, u1 slotId, args; ← return value |
| `l` | `L`      | link           | → link data; ← success |
| `n` | `N`      | readSchema     | → (empty); ← schema data |
| `f` | `F`      | fileOpen       | → str uri, str mode; ← u2 fileHandle |
| `g` | `G`      | fileRead       | → u2 handle, u2 size; ← byte[] data |
| `p` | `P`      | fileWrite      | → u2 handle, byte[] data; ← success |
| `q` | `Q`      | fileClose      | → u2 handle; ← success |
| `!` | —        | error          | ← error text (any request can produce this) |

**Source:** `EacIo/src/sedona/src/sedona/sox/SoxClient.java`, `sox/SoxCommands.html`

### 1.4 DASP Session Layer

DASP provides session management over UDP:

**Source:** `EacIo/src/sedona/src/sedona/dasp/DaspSession.java`

```java
// Session tuning parameters
int idealMax        = 512;    // preferred max packet bytes
int absMax          = 1400;   // absolute max packet bytes
int receiveTimeout  = 60000;  // 60s receive timeout
int connectTimeout  = 10000;  // 10s connect timeout
```

**DASP handshake (4-way):**
```
Client                              Server
  │                                    │
  │─── HELLO (nonce) ────────────────→ │
  │                                    │
  │←── CHALLENGE (server nonce) ────── │
  │                                    │
  │─── AUTHENTICATE (digest) ────────→ │
  │                                    │
  │←── WELCOME (session IDs) ──────── │
  │                                    │
  │    ═══ Session Established ═══     │
```

**Authentication:** Uses SHA-1 digest: `SHA1(serverNonce + ":" + SHA1(username + ":" + password))`

**Reliability:** DASP implements its own reliability layer over UDP with:
- Sequence numbers per session
- Sliding window for flow control (configurable `localReceiveMax`, `remoteReceiveMax`)
- Retransmission with configurable retry (`sendRetry`)
- Keep-alive heartbeats

**Source:** `EacIo/src/sedona/src/sedona/dasp/DaspSocket.java`
```java
// DaspSocket manages a UDP socket used by multiple sessions
// Sessions use sequence numbers and sliding windows for reliability
```

### 1.5 SOX C Implementation (Embedded Side)

On the Sedona VM/device side, SOX is implemented in C within the `sox` kit:

**Source:** `EacIo/src/vm/` - The native VM handles SOX server-side:
- Listens on UDP port 1876
- Authenticates via DASP 4-way handshake
- Dispatches SOX commands to component tree
- Pushes COV events to subscribed clients

---

## 2. Problems with SOX/DASP/UDP

### 2.1 NAT/Firewall Traversal

**Critical issue for modern deployments.** UDP-based protocols have severe NAT traversal problems:

- Corporate firewalls typically block inbound UDP
- NAT routers require UDP hole-punching or STUN/TURN servers
- Cloud-to-device communication requires VPN or port forwarding
- WebSocket (over HTTP/HTTPS) traverses virtually all networks

**Impact:** Cannot connect to devices behind firewalls without VPN infrastructure.

### 2.2 No Encryption

DASP provides authentication (SHA-1 digest) but **no encryption**. All SOX traffic is plaintext:

- Component values, property writes, and file transfers are unencrypted
- SHA-1 authentication is cryptographically weak (collision attacks known since 2005)
- No forward secrecy — captured sessions can be replayed
- No integrity protection beyond DASP's own checksums

**Impact:** Insecure for any network that isn't isolated.

### 2.3 Packet Size Limitation

UDP datagrams are limited by MTU (typically 1400-1500 bytes). DASP's `absMax` default is 1400 bytes:

```java
// DaspSession.java
int absMax = 1400;  // absolute max packet bytes
```

- Large component trees must be fragmented across multiple SOX messages
- File transfers require chunking into ~500-byte pieces
- Schema reads for large applications require multiple round-trips
- No built-in compression

**Impact:** Poor performance for large payloads. The existing bassgSoxWebSocket bridge already works around this by buffering.

### 2.4 Reinvented TCP (Poorly)

DASP reimplements reliability, ordering, and flow control over UDP — features TCP provides natively:

| Feature | DASP (custom) | TCP/WebSocket (standard) |
|---------|---------------|--------------------------|
| Reliability | Custom seq numbers + retransmit | OS kernel, battle-tested |
| Flow control | Custom sliding window | TCP congestion control |
| Ordering | Not guaranteed ("reliable but unordered") | Guaranteed in-order |
| Keep-alive | Custom heartbeat | TCP keep-alive, WebSocket ping/pong |
| Tooling | Custom Wireshark dissector (`sox.lua`) | Standard browser DevTools |

**Impact:** More bugs, harder to debug, less performant than standard TCP.

### 2.5 Limited Concurrency

SOX uses a 1-byte `replyNum` (0-254) for request-response correlation:

```java
// Msg.java
public void setReplyNum(int num) {
    if (num > 0xff) throw new IllegalStateException("replyNum=" + num);
    bytes[1] = (byte)(num & 0xff);
}
```

**Limit:** Maximum 255 concurrent outstanding requests per session.

### 2.6 Binary Protocol Opacity

SOX uses a custom binary encoding with no self-describing format:

- Debugging requires hex dumps and protocol knowledge
- No standard tooling (Wireshark needs custom `sox.lua` dissector)
- Error messages are opaque binary
- No schema evolution — protocol changes require coordinated updates

**Impact:** Difficult to debug, extend, and maintain.

### 2.7 Weak Authentication

DASP uses SHA-1 based digest authentication:

```
digest = SHA1(serverNonce + ":" + SHA1(username + ":" + password))
```

- SHA-1 is deprecated (NIST deprecated in 2011, collision found 2017)
- No salting of stored passwords
- No channel binding (vulnerable to man-in-the-middle)
- No token-based authentication for long-lived sessions

---

## 3. Existing WebSocket Bridge (bassgSoxWebSocket)

### 3.1 Architecture

The `bassgSoxWebSocket` Fantom pod (in `/home/parallels/code/bassgSoxWebSocket/`) provides a bridge between WebSocket clients and SOX/DASP/UDP devices:

```
[Browser/Vue App]
      │
      │ WebSocket JSON (port 7070)
      v
[SoxWebSocketMod]        ← Fantom WebSocket server
      │
      │ Message routing + session management
      v
[SoxSessionPool]          ← Max 2 sessions per device
      │
      │ DASP/UDP (port 1876)
      v
[Sedona Device]
```

**Source:** `bassgSoxWebSocket/doc/index.fandoc`

### 3.2 JSON Message Format (Legacy Bridge)

The bassgSoxWebSocket bridge uses JSON. **ROX replaces this with Trio encoding** — same command structure, more compact, native Haystack types:

```json
// Legacy JSON (bassgSoxWebSocket):
{"type": "v", "host": "192.168.1.100", "port": 1876}
{"type": "r", "compId": 5, "host": "...", "port": 1876}
{"type": "w", "compId": 5, "slotId": 3, "value": 72.5, "host": "...", "port": 1876}
{"type": "sub", "compIds": [1, 2, 3], "tree": false, "host": "...", "port": 1876}
{"type": "cov", "compId": 5, "slotId": 3, "value": 72.5}
{"type": "fileGet", "uri": "/app.sab", "host": "...", "port": 1876}
```

**ROX equivalent (Trio encoding — see Section 10):**
```
// Same operations, Trio-encoded:
type:r
compId:5

type:w
compId:5
slotId:3
value:72.5degF

type:sub
compIds:[1, 2, 3]

type:cov
compId:5
slotId:3
value:72.5degF
```

### 3.3 Key Components

| Component | File | Purpose |
|-----------|------|---------|
| `SoxWebSocketExt` | `SoxWebSocketExt.fan` | SkySpark extension, starts/stops service |
| `SoxMessage` | `SoxMessage.fan` | Base JSON message class |
| `SoxActorPool` | `SoxActorPool.fan` | Parallel SOX subscribe operations |
| `WatchManager` | `WatchManager.fan` | Subscription lifecycle (30s timeout) |
| `ComponentCache` | `ComponentCache.fan` | Server-side LRU cache (500 per device) |
| `FileTransferManager` | `FileTransferManager.fan` | Chunked file transfer tracking |
| Message models | `fan/model/*.fan` | ReadMsg, WriteMsg, SubscribeMsg, etc. |

### 3.4 Limitations of the Bridge Approach

1. **Double serialization:** JSON → Fantom objects → binary SOX → UDP
2. **Extra hop:** Every request goes Client → SkySpark → Device instead of Client → Device
3. **SkySpark dependency:** Requires running SkySpark server as intermediary
4. **Session bottleneck:** 2-session limit per device across all WebSocket clients
5. **No direct device access:** Browser cannot talk directly to Sandstar device

---

## 4. New Architecture: ROX Protocol (Trio-over-WebSocket)

### 4.1 Goal

Add ROX as a modern WebSocket-based protocol alongside SOX. ROX uses **Trio encoding** (Project Haystack's text format) for human-readable, type-safe messages. SOX/DASP/UDP remains available on port 1876 for backward compatibility with legacy Sedona tools.

**Why Trio instead of JSON?**
- **~66% smaller** than Haystack 4 JSON (no `_kind` wrappers, no quoted keys)
- **~30% smaller** than Hayson JSON
- **Native Haystack types** — markers are implicit (tag presence = marker), refs use `@id` syntax, numbers include units inline (`72.3degF`)
- **Human-readable** — can be read/debugged without tooling
- **No grid metadata needed** — ROX messages are dicts (single records), not grids; Trio encodes dicts naturally
- **Familiar to Haystack developers** — same scalar grammar as Zinc

**Why not JSON?**
- JSON requires type wrappers for Haystack values (`{"_kind":"marker"}`, `{"_kind":"ref","val":"..."}`)
- Verbose for marker-heavy ontology data (every marker tag needs `"m:"` or `{"_kind":"marker"}`)
- No unit support in numbers (must use string encoding `"n:72.3 degF"`)

### 4.2 Architecture

```
┌──────────────────────────────────────────────────────────┐
│  Sandstar Device (Rust)                                   │
│                                                           │
│  ┌──────────────┐  ┌──────────────┐  ┌────────────────┐ │
│  │ Axum HTTP    │  │ ROX WebSocket│  │ roxWarp WS     │ │
│  │ Server       │  │ Server       │  │ (mTLS/RSA)     │ │
│  │ :8085        │  │ :7070        │  │ :7443          │ │
│  │ Haystack API │  │ Trio-over-WS │  │ Binary Trio    │ │
│  └──────┬───────┘  └──────┬───────┘  │ Diff Gossip    │ │
│         │                  │          └──────┬─────────┘ │
│         └─────────┬────────┘                 │           │
│                   │                          │           │
│         ┌─────────▼─────────┐   ┌────────────▼────────┐ │
│         │  Engine Core      │   │  roxWarp Cluster    │ │
│         │  (channels, I/O)  │   │  Manager (gossip)   │ │
│         └─────────┬─────────┘   └─────────────────────┘ │
│                   │                                      │
│  ┌────────────────┼────────────────┐                     │
│  │                │                │                     │
│  │  ┌─────────────▼──────────┐    │                     │
│  │  │  Sedona VM (C FFI)     │    │                     │
│  │  └────────────────────────┘    │                     │
│  │                                │                     │
│  │  SOX/DASP/UDP :1876 (legacy)   │                     │
│  └────────────────────────────────┘                     │
└──────────────────────────────────────────────────────────┘
```

### 4.3 Protocol Comparison

| Feature | SOX/DASP/UDP (legacy) | ROX (new) | roxWarp (cluster) |
|---------|------------------------|-----------|-------------------|
| Transport | UDP/1876 | TCP+WebSocket/7070 | WSS+mTLS/7443 |
| Encoding | Custom binary | **Trio** (text, human-readable) | **Binary Trio diffs** (MessagePack) |
| Authentication | SHA-1 digest (weak) | SCRAM-SHA-256 + TLS | mTLS (RSA certificates) |
| Encryption | None | TLS 1.3 (rustls) | TLS 1.3 (mandatory) |
| NAT traversal | Broken by most firewalls | Works everywhere (HTTP upgrade) | Works everywhere |
| Max message size | ~1400 bytes | Unlimited (WebSocket framing) | Unlimited |
| Max concurrent requests | 255 (1-byte replyNum) | Unlimited (string requestId) | N/A (async gossip) |
| Reliability | Custom DASP (reimplented TCP) | TCP (OS kernel) | TCP (OS kernel) |
| Ordering | Unordered | Guaranteed in-order | Guaranteed in-order |
| Compression | None | permessage-deflate (RFC 7692) | Delta encoding + permessage-deflate |
| Debugging | Custom Wireshark dissector | Browser DevTools, `wscat` | Trio text fallback mode |
| Browser support | None (requires bridge) | Native | N/A (device-to-device) |
| Bidirectional | Yes (events) | Yes (native full-duplex) | Yes (gossip) |

---

## 5. SCRAM-SHA-256 Authentication (Haxall-Compatible)

### 5.1 Why SCRAM

SCRAM (Salted Challenge Response Authentication Mechanism, RFC 5802) is the authentication mechanism used by Haxall/SkySpark for HTTP API access. Implementing SCRAM-SHA-256 ensures:

1. **Haxall compatibility** — same auth flow used by SkySpark connectors
2. **No plaintext passwords** — salted + iterated hash, never sent over wire
3. **Mutual authentication** — both client and server prove identity
4. **Channel binding** — when used with TLS (SCRAM-SHA-256-PLUS), prevents MITM

### 5.2 SCRAM Handshake Over WebSocket (Trio Encoding)

```
Client                                     Server
  │                                           │
  │─── WS Connect to wss://device:7070 ────→ │  (TLS handshake via rustls)
  │                                           │
  │─── type:hello                             │
  │    username:admin ───────────────────────→ │
  │                                           │
  │←── type:challenge                         │
  │    hash:SHA-256                           │
  │    salt:base64...                         │
  │    iterations:10000                       │
  │    handshakeToken:"r=nonce...,s=salt,     │
  │      i=10000" ────────────────────────── │
  │                                           │
  │─── type:authenticate                      │
  │    proof:"c=biws,r=...,p=..." ──────────→ │
  │                                           │
  │←── type:authenticated                     │
  │    serverSignature:"v=base64..."          │
  │    token:session-id ─────────────────── │
  │                                           │
  │    ═══ Authenticated Session ═══          │
  │                                           │
  │─── type:v                                 │
  │    requestId:1 ─────────────────────────→ │  (normal ROX commands)
```

Each WebSocket text frame contains one Trio-encoded dict (no `---` separator needed since each frame is one message).

### 5.3 Haxall HTTP Auth Compatibility

The Haxall/Project Haystack HTTP API uses a similar SCRAM flow via HTTP headers:

```
GET /api/about HTTP/1.1
Authorization: HELLO username=admin

HTTP/1.1 401
WWW-Authenticate: SCRAM hash=SHA-256, salt=..., iterations=10000, ...

GET /api/about HTTP/1.1
Authorization: SCRAM data=...client-final-message...

HTTP/1.1 200
Authentication-Info: scram data=...server-final...
```

Our WebSocket implementation uses the same SCRAM algorithm but over WebSocket messages instead of HTTP headers, maintaining algorithmic compatibility.

### 5.4 Password Storage

```rust
// Server-side: stored credentials (never store plaintext)
struct StoredCredential {
    username: String,
    salt: [u8; 32],          // random, unique per user
    iterations: u32,          // 10000+ (NIST recommendation)
    stored_key: [u8; 32],     // SHA-256(HMAC(salted_password, "Client Key"))
    server_key: [u8; 32],     // HMAC(salted_password, "Server Key")
}
```

---

## 6. Rust Implementation: Southbound (ROX Device Communication)

### 6.1 Trio Parser Module

Since `libhaystack` does not include a Trio parser, ROX requires a custom Trio encoder/decoder. The format is simple enough (~600 lines of Rust) leveraging libhaystack's existing `Value` types and Zinc scalar grammar.

```rust
use libhaystack::val::{Value, Dict, Marker, Number, Ref, Str};
use std::collections::HashMap;

/// Parse a Trio-encoded string into a Dict
/// Each line is `name:value` (or just `name` for Marker)
/// Multi-line strings: if newline follows colon, value is indented block
pub fn trio_decode(input: &str) -> Result<Dict, TrioError> {
    let mut dict = Dict::new();
    let mut lines = input.lines().peekable();

    while let Some(line) = lines.next() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }

        if let Some(colon_pos) = line.find(':') {
            let name = line[..colon_pos].trim();
            let val_str = line[colon_pos + 1..].trim();

            if val_str.is_empty() {
                // Check for multi-line string (next line indented)
                if lines.peek().map_or(false, |l| l.starts_with(' ') || l.starts_with('\t')) {
                    let multi = collect_indented_block(&mut lines);
                    dict.insert(name.into(), Value::Str(Str::from(multi)));
                } else {
                    // Empty value after colon = Marker
                    dict.insert(name.into(), Value::Marker(Marker));
                }
            } else {
                // Parse scalar value using Zinc grammar
                let value = parse_trio_scalar(val_str)?;
                dict.insert(name.into(), value);
            }
        } else {
            // No colon = Marker tag
            dict.insert(line.into(), Value::Marker(Marker));
        }
    }

    Ok(dict)
}

/// Encode a Dict as Trio text
pub fn trio_encode(dict: &Dict) -> String {
    let mut out = String::new();
    for (name, value) in dict.iter() {
        match value {
            Value::Marker(_) => {
                out.push_str(name);
                out.push('\n');
            }
            Value::Str(s) if s.value().contains('\n') => {
                // Multi-line string
                out.push_str(name);
                out.push_str(":\n");
                for line in s.value().lines() {
                    out.push_str("  ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
            _ => {
                out.push_str(name);
                out.push(':');
                out.push_str(&zinc_encode_scalar(value));
                out.push('\n');
            }
        }
    }
    out
}
```

### 6.2 ROX WebSocket Server with Axum

```rust
use axum::{
    Router,
    extract::ws::{WebSocket, WebSocketUpgrade, Message},
    routing::get,
};
use tokio::sync::broadcast;

/// ROX WebSocket server (southbound, client-facing)
/// Uses Trio encoding for text frames
pub struct RoxWebSocketServer {
    port: u16,
    engine_tx: mpsc::Sender<EngineCommand>,
    auth_store: Arc<AuthStore>,
}

impl RoxWebSocketServer {
    pub async fn run(self) -> anyhow::Result<()> {
        let state = Arc::new(RoxServerState {
            engine_tx: self.engine_tx,
            auth_store: self.auth_store,
            subscriptions: DashMap::new(),
            cov_broadcast: broadcast::channel(1024).0,
        });

        let app = Router::new()
            .route("/rox", get(rox_ws_handler))
            .route("/ws", get(rox_ws_handler))    // backward compat alias
            .route("/live", get(rox_ws_handler))   // backward compat with bassgSoxWebSocket
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(
            format!("0.0.0.0:{}", self.port)
        ).await?;

        axum::serve(listener, app).await?;
        Ok(())
    }
}

async fn rox_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<RoxServerState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_rox_socket(socket, state))
}

async fn handle_rox_socket(mut socket: WebSocket, state: Arc<RoxServerState>) {
    // Phase 1: SCRAM authentication (Trio-encoded messages)
    let session = match rox_authenticate(&mut socket, &state.auth_store).await {
        Ok(session) => session,
        Err(e) => {
            let error_trio = format!("type:error\ncode:401\nmessage:{}\n", e);
            let _ = socket.send(Message::Text(error_trio)).await;
            return;
        }
    };

    // Phase 2: Message dispatch loop
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Subscribe to COV broadcast for this session
    let mut cov_rx = state.cov_broadcast.subscribe();

    loop {
        tokio::select! {
            // Incoming ROX message (Trio text frame)
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(trio_text))) => {
                        let response = dispatch_rox_message(&trio_text, &state, &session).await;
                        let _ = ws_tx.send(Message::Text(response)).await;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = ws_tx.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
            // Outgoing COV events (Trio text)
            cov = cov_rx.recv() => {
                if let Ok(event) = cov {
                    if session.is_subscribed(event.comp_id) {
                        let trio = trio_encode(&event.to_dict());
                        if ws_tx.send(Message::Text(trio)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }

    // Cleanup subscriptions on disconnect
    session.cleanup_subscriptions(&state).await;
}
```

### 6.3 ROX Message Dispatch (Trio)

```rust
/// ROX request parsed from Trio-encoded WebSocket text frame
enum RoxRequest {
    Version { request_id: Option<String> },
    Read { request_id: Option<String>, comp_id: u32 },
    Write { request_id: Option<String>, comp_id: u32, slot_id: u8, value: Value },
    Subscribe { request_id: Option<String>, comp_ids: Vec<u32>, tree: bool },
    Unsubscribe { request_id: Option<String>, comp_ids: Vec<u32> },
    Add { request_id: Option<String>, parent_id: u32, kit: String, r#type: String, name: String },
    Delete { request_id: Option<String>, comp_id: u32 },
    Invoke { request_id: Option<String>, comp_id: u32, slot_id: u8, args: Option<Value> },
    FileGet { request_id: Option<String>, uri: String },
    FilePut { request_id: Option<String>, uri: String, content: Vec<u8> },
}

/// Parse a Trio text frame into a RoxRequest
fn parse_rox_request(trio_text: &str) -> Result<RoxRequest, RoxError> {
    let dict = trio_decode(trio_text)?;

    let msg_type = dict.get_str("type")
        .ok_or(RoxError::MissingField("type"))?;
    let request_id = dict.get_str("requestId").map(String::from);

    match msg_type {
        "v" => Ok(RoxRequest::Version { request_id }),
        "r" => Ok(RoxRequest::Read {
            request_id,
            comp_id: dict.get_number("compId")? as u32,
        }),
        "w" => Ok(RoxRequest::Write {
            request_id,
            comp_id: dict.get_number("compId")? as u32,
            slot_id: dict.get_number("slotId")? as u8,
            value: dict.get("value").cloned().ok_or(RoxError::MissingField("value"))?,
        }),
        "sub" => Ok(RoxRequest::Subscribe {
            request_id,
            comp_ids: dict.get_number_list("compIds")?,
            tree: dict.has("tree"),  // Marker presence = true
        }),
        "unsub" => Ok(RoxRequest::Unsubscribe {
            request_id,
            comp_ids: dict.get_number_list("compIds")?,
        }),
        "fileGet" => Ok(RoxRequest::FileGet {
            request_id,
            uri: dict.get_str("uri").ok_or(RoxError::MissingField("uri"))?.into(),
        }),
        _ => Err(RoxError::UnknownType(msg_type.into())),
    }
}

async fn dispatch_rox_message(
    trio_text: &str,
    state: &RoxServerState,
    session: &AuthenticatedSession,
) -> String {
    let request = match parse_rox_request(trio_text) {
        Ok(r) => r,
        Err(e) => return format!("type:error\ncode:400\nmessage:{}\n", e),
    };

    match request {
        RoxRequest::Version { request_id } => {
            handle_version_trio(request_id, state).await
        }
        RoxRequest::Read { request_id, comp_id } => {
            handle_read_trio(request_id, comp_id, state).await
        }
        RoxRequest::Write { request_id, comp_id, slot_id, value } => {
            handle_write_trio(request_id, comp_id, slot_id, value, state).await
        }
        RoxRequest::Subscribe { request_id, comp_ids, tree } => {
            handle_subscribe_trio(request_id, comp_ids, tree, session, state).await
        }
        _ => format!("type:error\ncode:501\nmessage:Not implemented\n"),
    }
}
```

### 6.4 COV (Change of Value) Events — Trio Encoding

```rust
/// COV event data
#[derive(Clone)]
struct CovEvent {
    comp_id: u32,
    slot_id: u8,
    value: Value,
    what: String,   // "t", "c", "r", "l"
    timestamp: i64,
}

impl CovEvent {
    /// Encode as Trio dict for WebSocket text frame
    fn to_dict(&self) -> Dict {
        let mut dict = Dict::new();
        dict.insert("type".into(), Value::Str("cov".into()));
        dict.insert("compId".into(), Value::Number(Number::from(self.comp_id as f64)));
        dict.insert("slotId".into(), Value::Number(Number::from(self.slot_id as f64)));
        dict.insert("value".into(), self.value.clone());
        dict.insert("what".into(), Value::Str(self.what.clone().into()));
        dict.insert("ts".into(), Value::Number(Number::from(self.timestamp as f64)));
        dict
    }
}

/// COV event Trio output example:
/// ```trio
/// type:cov
/// compId:5
/// slotId:0
/// value:72.3degF
/// what:r
/// ts:1706000000
/// ```

/// The engine pushes COV events whenever a subscribed component changes
async fn cov_publisher(
    mut engine_events: mpsc::Receiver<EngineEvent>,
    broadcast_tx: broadcast::Sender<CovEvent>,
) {
    while let Some(event) = engine_events.recv().await {
        if let EngineEvent::ComponentChanged { comp_id, slot_id, value, what } = event {
            let cov = CovEvent {
                comp_id,
                slot_id,
                value: value.to_haystack_value(),
                what,
                timestamp: chrono::Utc::now().timestamp_millis(),
            };
            let _ = broadcast_tx.send(cov);
        }
    }
}
```

---

## 7. Northbound Clustering via Secured WebSocket (roxWarp)

### 7.1 Concept

Multiple Sandstar devices form a cluster for:

1. **Aggregated visibility** — single pane of glass for all devices
2. **Data replication** — critical points mirrored across nodes
3. **Failover** — if one device goes down, others have its last-known state
4. **Edge computing** — distributed analytics across the mesh

### 7.2 Cluster Topology

```
                    ┌─────────────────────┐
                    │  SkySpark / Haxall   │
                    │  (Cloud/On-Prem)     │
                    │  Northbound Client   │
                    └──────────┬──────────┘
                               │ WSS (SCRAM auth)
                    ┌──────────▼──────────┐
                    │  Cluster Gateway     │
                    │  (elected leader or  │
                    │   any node)          │
                    └──────────┬──────────┘
              ┌────────────────┼────────────────┐
              │                │                │
    ┌─────────▼──────┐ ┌──────▼─────────┐ ┌───▼──────────────┐
    │ Sandstar Node A │ │ Sandstar Node B │ │ Sandstar Node C  │
    │ WSS :7443       │ │ WSS :7443       │ │ WSS :7443        │
    │ mTLS (RSA cert) │ │ mTLS (RSA cert) │ │ mTLS (RSA cert)  │
    │                 │ │                 │ │                  │
    │ ┌─────────────┐ │ │ ┌─────────────┐ │ │ ┌──────────────┐ │
    │ │ Engine      │ │ │ │ Engine      │ │ │ │ Engine       │ │
    │ │ Sensors     │ │ │ │ Sensors     │ │ │ │ Sensors      │ │
    │ │ Sedona VM   │ │ │ │ Sedona VM   │ │ │ │ Sedona VM    │ │
    │ └─────────────┘ │ │ └─────────────┘ │ │ └──────────────┘ │
    └─────────────────┘ └─────────────────┘ └──────────────────┘
           ◄════════════════════════════════════════►
                  Peer-to-peer WSS mesh (mTLS/RSA)
                  Gossip protocol for state sync
```

### 7.3 Cluster Communication Model

Each node maintains WebSocket connections to all other known nodes:

| Connection Type | Port | Auth | Encryption | Protocol | Encoding |
|-----------------|------|------|------------|----------|----------|
| Client → Device | 7070 | SCRAM-SHA-256 | TLS (optional) | **ROX** | Trio (text) |
| Device → Device | 7443 | mTLS (RSA certs) | TLS 1.3 required | **roxWarp** | Binary Trio diffs |
| SkySpark → Device | 7070 | SCRAM-SHA-256 | TLS (recommended) | **ROX** | Trio (text) |

### 7.4 roxWarp Gossip Protocol

roxWarp uses a Scuttlebutt-style gossip protocol over the mTLS WebSocket mesh. Messages are sent as **binary frames** using MessagePack-encoded Trio dicts. A Trio text fallback mode is available for debugging.

See **[16_ROXWARP_PROTOCOL.md](16_ROXWARP_PROTOCOL.md)** for the complete roxWarp specification including:
- Binary Trio wire format (MessagePack encoding of Haystack dict key-value pairs)
- Delta encoding with version vectors (only changed tags since last sync)
- Gossip state machine and convergence guarantees
- Fantom pod (`bassgRoxWarp`) for SkySpark/Haxall integration

#### roxWarp Message Types (Trio text debug view)

```
// Heartbeat (every 5s)
type:cluster:heartbeat
nodeId:sandstar-a-001
ts:1706000000000
cpuPercent:23.5
memUsedMb:128
componentCount:450
version:"2.0.0"

// State sync (delta — only changed points since peer's last version)
type:cluster:state
nodeId:sandstar-a-001
stateVersion:1542
points: Zinc:
  ch,val,st,ts
  1113,72.5,"ok",1706000000
  1206,4.2,"ok",1706000000

// Topology change
type:cluster:join
nodeId:sandstar-d-004
address:"192.168.1.104:7443"
certFingerprint:"SHA256:ab:cd:ef:..."
```

---

## 8. RSA Key Management & Certificate Infrastructure

### 8.1 Certificate Hierarchy

```
┌─────────────────────────────────┐
│  Cluster CA (Root Certificate)  │  Generated once, stored securely
│  RSA 4096-bit                   │  Used to sign node certificates
│  validity: 10 years             │
└────────────────┬────────────────┘
                 │ signs
    ┌────────────┼────────────┐
    │            │            │
┌───▼──────┐ ┌──▼───────┐ ┌──▼───────┐
│ Node A   │ │ Node B   │ │ Node C   │
│ RSA 2048 │ │ RSA 2048 │ │ RSA 2048 │
│ CN=A-001 │ │ CN=B-002 │ │ CN=C-003 │
└──────────┘ └──────────┘ └──────────┘
```

### 8.2 Certificate Generation (Rust)

```rust
use rcgen::{Certificate, CertificateParams, DistinguishedName, KeyPair};
use rcgen::PKCS_RSA_SHA256;

/// Generate cluster CA certificate (one-time setup)
fn generate_cluster_ca() -> (Certificate, KeyPair) {
    let mut params = CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);

    let mut dn = DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, "Sandstar Cluster CA");
    dn.push(rcgen::DnType::OrganizationName, "AnkaLabs");
    params.distinguished_name = dn;

    // RSA 4096-bit for CA
    let key_pair = KeyPair::generate_for(&PKCS_RSA_SHA256).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    (cert, key_pair)
}

/// Generate node certificate signed by CA
fn generate_node_cert(
    node_id: &str,
    ip_address: &str,
    ca_cert: &Certificate,
    ca_key: &KeyPair,
) -> (CertificateDer, KeyPair) {
    let mut params = CertificateParams::default();
    params.is_ca = rcgen::IsCa::NoCa;

    let mut dn = DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, node_id);
    params.distinguished_name = dn;

    // Add IP as SAN for direct IP connections
    params.subject_alt_names = vec![
        rcgen::SanType::IpAddress(ip_address.parse().unwrap()),
        rcgen::SanType::DnsName(node_id.try_into().unwrap()),
    ];

    // RSA 2048-bit for nodes
    let key_pair = KeyPair::generate_for(&PKCS_RSA_SHA256).unwrap();
    let cert = params.signed_by(&key_pair, ca_cert, ca_key).unwrap();
    (cert, key_pair)
}
```

### 8.3 Mutual TLS Configuration (rustls)

```rust
use rustls::{ServerConfig, ClientConfig};
use rustls::server::WebPkiClientVerifier;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::Arc;

/// Create TLS server config requiring client certificates (mTLS)
fn create_mtls_server_config(
    server_cert: Vec<CertificateDer<'static>>,
    server_key: PrivateKeyDer<'static>,
    ca_cert: CertificateDer<'static>,
) -> Arc<ServerConfig> {
    // Build trust store with cluster CA
    let mut root_store = rustls::RootCertStore::empty();
    root_store.add(ca_cert).unwrap();

    // Require client certificates signed by our CA
    let client_verifier = WebPkiClientVerifier::builder(Arc::new(root_store))
        .build()
        .unwrap();

    let config = ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(server_cert, server_key)
        .unwrap();

    Arc::new(config)
}

/// Create TLS client config for connecting to peer nodes
fn create_mtls_client_config(
    client_cert: Vec<CertificateDer<'static>>,
    client_key: PrivateKeyDer<'static>,
    ca_cert: CertificateDer<'static>,
) -> Arc<ClientConfig> {
    let mut root_store = rustls::RootCertStore::empty();
    root_store.add(ca_cert).unwrap();

    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(client_cert, client_key)
        .unwrap();

    Arc::new(config)
}
```

### 8.4 Key Storage on Device

```
/home/eacio/sandstar/etc/cluster/
├── ca.pem                    # Cluster CA certificate (public)
├── node.pem                  # This node's certificate (public)
├── node.key                  # This node's private key (chmod 600)
├── known_nodes.json          # Peer node registry
└── cluster.toml              # Cluster configuration
```

```toml
# cluster.toml
[cluster]
node_id = "sandstar-a-001"
listen_port = 7443
heartbeat_interval_secs = 5
state_sync_interval_secs = 10

[[cluster.peers]]
node_id = "sandstar-b-002"
address = "192.168.1.102:7443"

[[cluster.peers]]
node_id = "sandstar-c-003"
address = "192.168.1.103:7443"

[cluster.replication]
# Which channels to replicate to peers
channels = [1113, 1206, 1300, 2100]
```

---

## 9. Rust Implementation: roxWarp Cluster Node

### 9.1 roxWarp Cluster Manager

```rust
use tokio_tungstenite::{connect_async_tls_with_config, Connector};
use dashmap::DashMap;

pub struct ClusterManager {
    node_id: String,
    tls_config: Arc<ServerConfig>,
    client_tls: Arc<ClientConfig>,
    peers: DashMap<String, PeerConnection>,
    shared_state: Arc<RwLock<ClusterState>>,
}

struct PeerConnection {
    node_id: String,
    address: String,
    ws_tx: Option<mpsc::Sender<String>>,
    last_heartbeat: Instant,
    status: PeerStatus,
}

#[derive(Clone)]
struct ClusterState {
    /// Aggregated point values from all nodes
    points: HashMap<String, HashMap<u16, PointValue>>,
    /// Node health status
    nodes: HashMap<String, NodeHealth>,
}

impl ClusterManager {
    /// Start the cluster listener (accepts incoming peer connections)
    pub async fn start_listener(&self) -> anyhow::Result<()> {
        let tls_acceptor = tokio_rustls::TlsAcceptor::from(self.tls_config.clone());
        let listener = tokio::net::TcpListener::bind(
            format!("0.0.0.0:7443")
        ).await?;

        loop {
            let (stream, addr) = listener.accept().await?;

            // TLS handshake (mTLS — verifies client cert)
            let tls_stream = tls_acceptor.accept(stream).await?;

            // Extract peer identity from client certificate
            let peer_cert = tls_stream.get_ref().1
                .peer_certificates()
                .and_then(|certs| certs.first());

            let peer_id = extract_cn_from_cert(peer_cert)?;
            log::info!("Cluster peer connected: {} from {}", peer_id, addr);

            // Upgrade to WebSocket
            let ws_stream = tokio_tungstenite::accept_async(tls_stream).await?;
            self.handle_peer_connection(peer_id, ws_stream).await;
        }
    }

    /// Connect to a peer node
    pub async fn connect_to_peer(&self, address: &str) -> anyhow::Result<()> {
        let connector = Connector::Rustls(self.client_tls.clone());
        let url = format!("wss://{}/cluster", address);

        let (ws_stream, _) = connect_async_tls_with_config(
            &url,
            None,
            false,
            Some(connector),
        ).await?;

        let (ws_tx, mut ws_rx) = ws_stream.split();
        // ... handle bidirectional cluster messages
        Ok(())
    }

    /// Broadcast state update to all peers
    pub async fn broadcast_state(&self, points: &[PointValue]) {
        let msg = serde_json::to_string(&ClusterMessage::State {
            node_id: self.node_id.clone(),
            points: points.to_vec(),
        }).unwrap();

        for peer in self.peers.iter() {
            if let Some(tx) = &peer.ws_tx {
                let _ = tx.send(msg.clone()).await;
            }
        }
    }
}
```

### 9.2 roxWarp Cluster Messages

```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum ClusterMessage {
    #[serde(rename = "cluster:heartbeat")]
    Heartbeat {
        node_id: String,
        timestamp: i64,
        load: NodeLoad,
        version: String,
    },

    #[serde(rename = "cluster:state")]
    State {
        node_id: String,
        points: Vec<PointValue>,
    },

    #[serde(rename = "cluster:join")]
    Join {
        node_id: String,
        address: String,
        certificate_fingerprint: String,
    },

    #[serde(rename = "cluster:leave")]
    Leave {
        node_id: String,
        reason: String,
    },

    #[serde(rename = "cluster:query")]
    Query {
        request_id: String,
        filter: String,  // Haystack filter expression
    },

    #[serde(rename = "cluster:query_result")]
    QueryResult {
        request_id: String,
        node_id: String,
        points: Vec<PointValue>,
    },
}

#[derive(Serialize, Deserialize, Clone)]
struct PointValue {
    channel: u16,
    value: f64,
    status: String,
    unit: Option<String>,
    timestamp: i64,
}

#[derive(Serialize, Deserialize, Clone)]
struct NodeLoad {
    cpu_percent: f32,
    mem_used_mb: u32,
    component_count: u32,
    uptime_secs: u64,
}
```

### 9.3 roxWarp Gossip-Based State Synchronization

```rust
/// Scuttlebutt-style gossip: each node maintains version vectors
/// and only sends deltas since the peer's last known version
struct GossipState {
    /// Version vector: node_id -> sequence number
    versions: HashMap<String, u64>,
    /// Point values with version stamps
    points: HashMap<(String, u16), VersionedPoint>,
}

struct VersionedPoint {
    value: f64,
    status: String,
    version: u64,      // monotonically increasing per node
    timestamp: i64,
}

impl GossipState {
    /// Get delta since peer's last known versions
    fn delta_since(&self, peer_versions: &HashMap<String, u64>) -> Vec<PointValue> {
        let mut deltas = Vec::new();
        for ((node_id, channel), point) in &self.points {
            let peer_ver = peer_versions.get(node_id).copied().unwrap_or(0);
            if point.version > peer_ver {
                deltas.push(PointValue {
                    channel: *channel,
                    value: point.value,
                    status: point.status.clone(),
                    unit: None,
                    timestamp: point.timestamp,
                });
            }
        }
        deltas
    }
}
```

---

## 10. Wire Protocol Specification

### 10.1 Southbound (Client ↔ Device) — ROX: Trio over WebSocket

**Endpoint:** `ws://device:7070/rox` or `wss://device:7070/rox`
**Legacy aliases:** `/ws`, `/live` (backward compatibility with bassgSoxWebSocket)

All messages are Trio-encoded dicts sent as WebSocket **text frames**. Each frame contains exactly one dict (no `---` separators). Every message has a `type` tag. Request messages include an optional `requestId` for correlation.

#### Authentication Messages (Trio)

```
// Client → Server: Initiate SCRAM
type:hello
username:admin

// Server → Client: SCRAM challenge
type:challenge
hash:SHA-256
salt:base64encodedSalt
iterations:10000
handshakeToken:"r=clientNonce+serverNonce,s=salt,i=10000"

// Client → Server: SCRAM proof
type:authenticate
proof:"c=biws,r=combinedNonce,p=clientProof"

// Server → Client: Authentication success
type:authenticated
serverSignature:"v=base64serverSig"
token:session-id-opaque
```

#### ROX Operation Messages (Trio)

```
// Read component
// Request:
type:r
requestId:uuid-1
compId:5

// Response:
type:R
requestId:uuid-1
compId:5
name:temp
compType:"control::NumericConst"
parentId:1
slots: Zinc:
  id,name,slotType,value,flags
  0,"out","float",72.5,4

// Write slot
// Request:
type:w
requestId:uuid-2
compId:5
slotId:0
value:75degF

// Response:
type:W
requestId:uuid-2
success

// Subscribe to COV (marker tag = subscribe to tree)
// Request:
type:sub
requestId:uuid-3
compIds:[5, 6, 7]
tree
mask:15

// Response:
type:S
requestId:uuid-3
subscribed:[5, 6, 7]

// Server pushes COV events (no requestId — server-initiated)
type:cov
compId:5
slotId:0
value:73.2degF
what:r
ts:1706000000

// File transfer (no chunking needed — WebSocket handles framing)
// Request:
type:fileGet
requestId:uuid-4
uri:`/app.sab`

// Response (binary content as base64 string):
type:file
requestId:uuid-4
uri:`/app.sab`
size:45320
content:"base64-encoded-data..."

// Error response:
type:error
requestId:uuid-4
code:404
message:"File not found: /app.sab"
```

#### Trio Encoding Rules for ROX

1. **One dict per WebSocket text frame** — no `---` separators
2. **Markers** — tag name alone on a line (e.g., `success`, `tree`, `point`)
3. **Numbers with units** — inline units using Zinc scalar grammar (`72.3degF`, `4.2mA`)
4. **Refs** — `@id` or `@id "Display Name"` syntax
5. **Strings** — unquoted if safe chars only, quoted with `"..."` otherwise
6. **Lists** — Zinc list syntax `[1, 2, 3]` on the value line
7. **Nested grids** — use `Zinc:` prefix for multi-line grid data (e.g., slots)
8. **URIs** — backtick syntax `` `http://...` ``
9. **Comments** — `//` line comments (stripped by parser)
10. **Boolean** — `T` or `F` (Zinc syntax)

#### Message Size Comparison (Single COV Event)

| Encoding | Size | vs JSON |
|----------|------|---------|
| SOX binary | ~9 bytes | -95% |
| **ROX Trio** | **~62 bytes** | **-66%** |
| JSON (Hayson) | ~110 bytes | -39% |
| JSON (Haystack 4) | ~180 bytes | baseline |
| **roxWarp binary** | **~12-20 bytes** | **-89%** |

### 10.2 Northbound (Device ↔ Device) — roxWarp over WSS+mTLS

**Endpoint:** `wss://device:7443/roxwarp`

The northbound cluster protocol uses **roxWarp** — binary Trio diff encoding over WebSocket **binary frames**. Authentication is via mTLS; no SCRAM needed for device-to-device.

See **[16_ROXWARP_PROTOCOL.md](16_ROXWARP_PROTOCOL.md)** for the complete roxWarp specification.

#### roxWarp Trio Text Fallback (Debug Mode)

For debugging, roxWarp supports a Trio text mode where cluster messages use text frames:

```
// Heartbeat (every 5s)
type:cluster:heartbeat
nodeId:sandstar-a-001
ts:1706000000
cpuPercent:23.5
memUsedMb:128
componentCount:450
version:"2.0.0"

// State delta sync
type:cluster:state
nodeId:sandstar-a-001
stateVersion:1542
points: Zinc:
  ch,val,st,ts
  1113,72.5,"ok",1706000000
  1206,4.2,"ok",1706000000

// Query across cluster
type:cluster:query
requestId:q-1
filter:"channel >= 1100 and channel < 1200"

// Query result
type:cluster:queryResult
requestId:q-1
nodeId:sandstar-b-002
points: Zinc:
  ch,val,st,unit,ts
  1113,72.5,"ok","degF",1706000000
```

---

## 11. Migration Path

### Phase 1: ROX Dual-Stack (SOX + ROX Coexistence)

**Goal:** Add ROX WebSocket server alongside existing SOX/UDP. Both protocols serve the same engine, operating simultaneously.

```
Sandstar Device
├── SOX/DASP/UDP :1876     ← existing, unchanged
├── Haystack HTTP :8085     ← existing, unchanged
└── ROX WebSocket :7070     ← NEW (Rust, Trio encoding)
```

**Tasks:**
1. Write Trio parser/encoder in Rust (~600 LOC, leveraging libhaystack `Value` types)
2. Implement `RoxWebSocketServer` in Rust with Trio message dispatch
3. Wire ROX handlers to same engine command channel used by SOX
4. Implement SCRAM-SHA-256 authentication with Trio message encoding
5. Test with modified sedonaWebEditor (Trio messages over WebSocket)
6. Build bassgRoxWebSocket Fantom pod for SkySpark (see doc 16)

**Estimated Rust LOC:** ~2,600 (includes Trio parser)

### Phase 2: TLS + roxWarp Cluster Foundation

**Goal:** Add TLS encryption for ROX, implement roxWarp cluster infrastructure with binary Trio diffs.

**Tasks:**
1. Add `rustls` TLS to ROX WebSocket server (port 7070 → WSS)
2. Generate cluster CA and node certificates using `rcgen`
3. Design binary Trio encoding format (MessagePack-based, see doc 16)
4. Implement `RoxWarpClusterManager` with mTLS on port 7443
5. Implement heartbeat and peer discovery over roxWarp
6. Add `cluster.toml` configuration

**Estimated Rust LOC:** ~3,500

### Phase 3: roxWarp State Replication & Gossip

**Goal:** Implement roxWarp gossip-based state synchronization with binary Trio diffs.

**Tasks:**
1. Implement `GossipState` with version vectors and delta encoding
2. Add binary Trio diff sync over roxWarp WebSocket (binary frames)
3. Implement cluster query (distributed Haystack filter evaluation)
4. Add failover detection (missed heartbeats → peer down)
5. Aggregated API endpoint for SkySpark consumption
6. Build bassgRoxWarp Fantom pod for SkySpark/Haxall (see doc 16)

**Estimated Rust LOC:** ~3,000

### Phase 4: Optional SOX Deprecation

**Goal:** Optionally remove DASP/UDP stack. ROX/roxWarp become primary protocols. SOX can be retained for legacy tool compatibility.

**Tasks:**
1. Make SOX listener optional via configuration (`sox.enabled = false`)
2. Update `SoxClient` (Java) to support ROX/Trio as alternative
3. Update all tooling to prefer ROX endpoints
4. Document migration guide for SOX → ROX clients
5. SOX port 1876 remains available but disabled by default in new installs

### Phase Summary

| Phase | Focus | Rust LOC | Dependencies |
|-------|-------|----------|-------------|
| 1 | ROX (Trio-over-WS) + SCRAM | ~2,600 | axum, tokio-tungstenite, scram-rs |
| 2 | TLS + roxWarp cluster | ~3,500 | rustls, rcgen, tokio-rustls, rmp-serde |
| 3 | roxWarp gossip + replication | ~3,000 | dashmap, rmp-serde |
| 4 | Optional SOX deprecation | ~500 (config) | — |
| **Total** | | **~9,600 net new** | |

---

## 12. Dependency Summary

### 12.1 New Rust Crates Required

| Crate | Version | Purpose | Size Impact |
|-------|---------|---------|-------------|
| `axum` (ws feature) | 0.7+ | WebSocket server (already in project for Haystack API) | — (already included) |
| `tokio-tungstenite` | 0.24+ | WebSocket client (for cluster outbound connections) | ~50KB |
| `rustls` | 0.23+ | TLS 1.3 implementation (pure Rust, no OpenSSL) | ~200KB |
| `tokio-rustls` | 0.26+ | Async TLS integration with tokio | ~20KB |
| `rcgen` | 0.13+ | X.509 certificate generation (RSA key pairs) | ~100KB |
| `scram-rs` | 0.17+ | SCRAM-SHA-256 authentication (client + server) | ~30KB |
| `dashmap` | 6.0+ | Concurrent HashMap for peer/subscription tracking | ~30KB |
| `webpki` | 0.22+ | Certificate verification (used by rustls) | ~50KB |
| `rmp-serde` | 1.3+ | MessagePack serialization for roxWarp binary Trio diffs | ~40KB |
| `rmp` | 0.8+ | Low-level MessagePack (dependency of rmp-serde) | ~20KB |

### 12.2 Crate Details

**`scram-rs`** — Full SCRAM implementation:
- Supports SCRAM-SHA-256 and SCRAM-SHA-256-PLUS (channel binding)
- Both client and server sides
- Async support
- Source: https://codeberg.org/4neko/scram-rs

**`rustls`** — Modern TLS in pure Rust:
- TLS 1.2 and 1.3
- No OpenSSL dependency (critical for ARM cross-compilation)
- `WebPkiClientVerifier` for mTLS client certificate validation
- Source: https://github.com/rustls/rustls

**`rcgen`** — Certificate generation:
- Generate self-signed CA certificates
- Generate node certificates signed by CA
- RSA 2048/4096-bit key pair generation (requires `aws-lc-rs` backend)
- Source: https://github.com/rustls/rcgen

**`tokio-tungstenite`** — Async WebSocket:
- Client and server WebSocket implementation
- TLS support via `rustls` connector
- Binary and text message support
- Source: https://github.com/snapview/tokio-tungstenite

**`rmp-serde`** — MessagePack for roxWarp binary Trio:
- Fastest schema-less binary format in Rust benchmarks
- 57% smaller than JSON, 2.3x faster serialization
- Native serde integration (`#[derive(Serialize, Deserialize)]`)
- Used for binary Trio diff encoding in roxWarp cluster protocol
- Source: https://github.com/3Hren/msgpack-rust

### 12.3 Custom Components (No External Crate)

| Component | Estimated LOC | Purpose |
|-----------|---------------|---------|
| Trio parser/encoder | ~600 | Parse/encode Haystack Trio format for ROX messages |
| Binary Trio encoder | ~400 | MessagePack-based binary encoding of Trio dicts for roxWarp |
| Delta encoder | ~300 | Version-vector based diff computation for COV state sync |

**Note:** `libhaystack` (j2inn) does not include Trio support. The Trio parser leverages libhaystack's `Value` types and Zinc scalar grammar but implements Trio's line-based dict format from scratch.

### 12.4 Total Binary Size Impact

| Component | Estimated Size |
|-----------|---------------|
| WebSocket server (axum ws) | ~0 (already in binary) |
| tokio-tungstenite | ~50KB |
| rustls + webpki | ~250KB |
| rcgen (build-time only for cert gen) | ~0 (optional runtime) |
| scram-rs | ~30KB |
| rmp-serde + rmp | ~60KB |
| Trio parser (custom) | ~10KB |
| **Total additional** | **~400KB** |

On the BeagleBone (512MB RAM), this is negligible. The entire Rust binary including Haystack API, engine, ROX, and roxWarp clustering is estimated at ~5-8MB stripped.

---

## Appendix A: Security Comparison

| Aspect | SOX/DASP/UDP | ROX + SCRAM + mTLS |
|--------|-------------|--------------------------|
| Password transport | SHA-1 digest (weak) | SCRAM-SHA-256 (NIST approved) |
| Encryption | None | TLS 1.3 (AEAD ciphers) |
| Forward secrecy | No | Yes (ECDHE key exchange) |
| Mutual authentication | No (server only) | Yes (SCRAM mutual + mTLS certs) |
| Channel binding | No | SCRAM-SHA-256-PLUS with tls-server-end-point |
| Certificate revocation | N/A | CRL or OCSP (via rustls) |
| Key rotation | Manual | Automated cert renewal via rcgen |
| Replay protection | DASP seq numbers | TLS record layer + session tokens |

## Appendix B: Relationship to Prior Documents

| Document | Relationship |
|----------|-------------|
| [04 REST API / Axum](04_REST_API_AXUM_MIGRATION.md) | ROX WebSocket server shares Axum router; same HTTP port can serve both REST and WS |
| [06 Sedona FFI Strategy](06_SEDONA_FFI_STRATEGY.md) | ROX commands dispatch through same FFI bridge to Sedona VM |
| [07 IPC Bridge](07_IPC_BRIDGE.md) | ROX handlers use same `tokio::sync::mpsc` channels as REST handlers |
| [09 Dependency Mapping](09_DEPENDENCY_MAPPING.md) | New crates (rustls, scram-rs, rcgen, rmp-serde) added to dependency graph |
| [11 Migration Roadmap](11_MIGRATION_ROADMAP.md) | ROX/roxWarp migration is a new Phase 6 after Phase 5 (Sedona FFI) |
| [12 Sedona VM Architecture](12_SEDONA_VM_ARCHITECTURE_ANALYSIS.md) | ROX commands ultimately interact with VM component tree |
| [14 Scalability Limits](14_SEDONA_VM_SCALABILITY_LIMITS.md) | Removing 255-request limit (replyNum) eliminates a scalability bottleneck |
| [16 roxWarp Protocol](16_ROXWARP_PROTOCOL.md) | Binary Trio diff encoding, gossip protocol, Fantom pod for SkySpark |
