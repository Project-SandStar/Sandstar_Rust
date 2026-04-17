# Implementation Plan: Haystack Filter Improvements + BACnet/IP Driver

**Date:** 2026-04-13 (created), 2026-04-16 (final completion)
**Scope:** Two projects — a small filter improvement (~1 day) followed by a multi-session BACnet/IP driver
**Target:** Sandstar Rust v2.0.0 → v2.7.0 running on BeagleBone 1-11 at 192.168.1.11
**Status:** ✅ **ALL PHASES COMPLETE** — Project A (Filter) 2026-04-13, Project B (BACnet) 2026-04-10 → 2026-04-16. Live-validated end-to-end against `tools/bacnet_sim.py`. Driver is no longer "read-only" — the original plan was read-only, but B7 (WriteProperty) was added mid-sprint and is also shipped. Full details per phase in the sections below.

**Shipped versions (BACnet-related):**
- v2.1.0 — B1 (frame codec) deployed
- v2.1.1 — bugfix (open_all after register)
- v2.1.2 — discovery debug logging (surfaced the firewalld issue on 1-11)
- v2.2.0 — B7 WriteProperty
- v2.3.0 — B9 ReadPropertyMultiple
- v2.4.0 — B8 SubscribeCOV (wire)
- v2.5.0 — B8.1 COV reception + B10 BBMD
- v2.5.1 — B8.2 COV renewal (240s timer on sync_cur)
- v2.6.0 — register_point + add_poll_bucket wiring
- v2.6.1 — tick task making sync_cur actually fire
- v2.7.0 — Stage 2: sync_cur results → engine.write_channel → /read visible

---

## Table of Contents

1. [Phase 0: Discovery Findings (already gathered)](#phase-0-discovery-findings)
2. [Project A: Haystack Filter Improvements](#project-a-haystack-filter-improvements)
3. [Project B: BACnet/IP Read-Only Driver](#project-b-bacnetip-read-only-driver)
4. [Cross-cutting Concerns](#cross-cutting-concerns)
5. [Verification Strategy](#verification-strategy)

---

## Phase 0: Discovery Findings

### Haystack filter current state

**File:** `crates/sandstar-server/src/rest/filter.rs` (1,212 lines)

**Already implemented:**
- AST: `Expr::Has`, `Expr::Missing`, `Expr::Cmp`, `Expr::And`, `Expr::Or`
- All comparison operators (`==`, `!=`, `<`, `<=`, `>`, `>=`)
- Parentheses with `MAX_PARSE_DEPTH=32` DoS protection
- `not` operator (lines 131-138) — but with a BUG when applied to non-marker terms
- Dynamic tag evaluation via `DynSlotStore` (lines 330-339)
- Integration with `/api/read` handler (handlers.rs line 113)
- 698 lines of tests

**Gaps:**
1. **`->` path dereference is NOT implemented.** The tokenizer doesn't recognize `->`, the `Cmp` AST variant only holds a single tag name (not a path), and there's no "pather" callback to resolve Refs.
2. **`not` with compound expressions is buggy.** Line 136 does `Expr::Missing(format!("{:?}", other))` which creates nonsense like `Missing("And(...)")`. It needs a proper `Expr::Not(Box<Expr>)` AST variant.
3. **No other major Haystack filter features are missing** (function calls, patterns, date literals are out of scope).

### Driver framework current state

**Files:** `crates/sandstar-server/src/drivers/` (mod.rs, async_driver.rs, modbus.rs, bacnet.rs, etc.)

**AsyncDriver trait** (async_driver.rs:31-84) — the contract for network drivers:
- `driver_type() -> &'static str`
- `id() -> &str`
- `status() -> &DriverStatus`
- `poll_mode() -> PollMode` (default: Buckets)
- `async open() -> Result<DriverMeta, DriverError>`
- `async close()`
- `async ping() -> Result<DriverMeta, DriverError>`
- `async learn(path: Option<&str>) -> Result<LearnGrid, DriverError>`
- `async sync_cur(&[DriverPointRef]) -> Vec<(u32, Result<f64, DriverError>)>`
- `async write(&[(u32, f64)]) -> Vec<(u32, Result<(), DriverError>)>`
- `async on_watch(&[DriverPointRef]) -> Result<(), DriverError>`
- `async on_unwatch(&[DriverPointRef]) -> Result<(), DriverError>`

**ModbusDriver** (modbus.rs, 781 lines) is the reference implementation:
- Full MBAP TCP framing
- Function codes 01-06, 16 (read coils, read discrete inputs, read holding, read input, write single coil, write single register, write multiple registers)
- Per-point scale/offset via `ModbusRegister`
- Lifecycle: `new()` → `open()` (try_connect) → `sync_cur()` / `write()` → `close()`
- 16 unit tests

**BACnet stub** (bacnet.rs, 120 lines) currently returns `NotSupported` for all methods except `open()`/`close()`/`sync_cur()` (which returns empty vec).

### BACnet ecosystem reality check

- **No mature Rust BACnet crate exists.** Searching crates.io for `bacnet` and `bacnet-rs` returns nothing production-ready.
- **bacnet-stack (C library)** is mature and Apache 2.0-licensed, but wrapping it via FFI contradicts the project's "pure Rust" philosophy (we just deleted all C code in v2.0.0).
- **Hand-rolling the protocol** is the only sensible option and aligns with how Modbus was done.

---

## Project A: Haystack Filter Improvements

**Estimated effort:** 1 day (6-8 hours)
**Risk:** Low
**Outcome:** Filter parser supports `not` for compound expressions and `->` path dereferences

### A.1 Scope

| Feature | Status | Work |
|---------|--------|------|
| `not <path>` (marker negation) | **DONE** | Fixed |
| `not <compound>` | **DONE** | `Expr::Not(Box<Expr>)` added |
| `<path> -> <path>` dereference | Not implemented | Full feature build |
| Pather callback | Not implemented | Design + thread through API |

### A.2 Phase A1: Proper `not` operator — ✅ COMPLETE (2026-04-13)

**Goal:** `not (enabled and point)` should work correctly.

**Changes:**

1. **`filter.rs:28-42`** — Add new AST variant:
   ```rust
   pub enum Expr {
       Has(String),
       Missing(String),
       Not(Box<Expr>),              // NEW
       Cmp(Path, CmpOp, Value),     // change String → Path (see Phase A2)
       And(Box<Expr>, Box<Expr>),
       Or(Box<Expr>, Box<Expr>),
   }
   ```

2. **`filter.rs:131-138`** — Fix `parse_term()` not handling:
   ```rust
   if tokens[*pos] == Token::Not {
       *pos += 1;
       let inner = parse_term(tokens, pos, depth)?;
       return Ok(match inner {
           // Marker negation — keep as Missing for fast-path evaluation
           Expr::Has(name) => Expr::Missing(name),
           // Compound negation — wrap in Not
           other => Expr::Not(Box::new(other)),
       });
   }
   ```

3. **`filter.rs:315+`** — Add `matches()` arm for `Expr::Not`:
   ```rust
   Expr::Not(inner) => !matches(inner, ch),
   ```

4. **Same for `matches_with_tags()`** at line 330.

**Verification:**
- Existing 14 parser tests still pass
- New tests:
  - `test_parse_not_compound` — `not (a and b)` parses to `Not(And(Has("a"), Has("b")))`
  - `test_eval_not_compound` — negation of compound expression evaluates correctly
  - `test_eval_double_not` — `not not point` evaluates as `point`

### A.3 Phase A2: `->` Path Dereference — ✅ COMPLETE (2026-04-13)

**Goal:** `siteRef->area == "Zone 1"` resolves the ref, then checks the target record.

**Changes:**

1. **New `Path` type** in `filter.rs`:
   ```rust
   #[derive(Debug, Clone, PartialEq)]
   pub struct Path {
       segments: Vec<String>,
   }

   impl Path {
       pub fn single(name: &str) -> Self {
           Self { segments: vec![name.to_string()] }
       }
       pub fn is_single(&self) -> bool {
           self.segments.len() == 1
       }
       pub fn head(&self) -> &str { &self.segments[0] }
       pub fn tail(&self) -> &[String] { &self.segments[1..] }
   }
   ```

2. **Update `Expr::Cmp`** to use `Path` instead of `String`:
   ```rust
   Cmp(Path, CmpOp, Value)
   ```
   `Expr::Has` and `Expr::Missing` keep `String` — paths only make sense for comparisons.

3. **Tokenizer (`filter.rs:220-248`)** — Recognize `->`:
   ```rust
   // Where other two-character ops are checked:
   if c == '-' && peek == '>' {
       tokens.push(Token::Arrow);
       i += 2;
       continue;
   }
   ```
   Add `Token::Arrow` to the `Token` enum.

4. **Parser (`parse_cmp`, lines 155-192)** — After reading the initial name, loop collecting `->` segments:
   ```rust
   let mut segments = vec![first_name];
   while *pos < tokens.len() && tokens[*pos] == Token::Arrow {
       *pos += 1;
       match tokens.get(*pos) {
           Some(Token::Name(n)) => {
               segments.push(n.clone());
               *pos += 1;
           }
           _ => return Err("expected name after ->"),
       }
   }
   let path = Path { segments };
   ```

5. **Pather callback design:**
   ```rust
   /// Resolves a Ref value to a tag dict, for `->` path navigation.
   pub type Pather<'a> = dyn Fn(&str) -> Option<HashMap<String, DynValue>> + 'a;
   ```

   Add new top-level functions that accept a pather:
   ```rust
   pub fn matches_full(
       expr: &Expr,
       ch: &ChannelInfo,
       tags: Option<&HashMap<String, DynValue>>,
       pather: Option<&Pather<'_>>,
   ) -> bool { ... }
   ```
   Keep `matches()` and `matches_with_tags()` as backward-compatible wrappers calling `matches_full(..., None)`.

6. **Path evaluation logic:**
   ```rust
   fn eval_path(path: &Path, ch: &ChannelInfo, tags: Option<&HashMap<String, DynValue>>,
                pather: Option<&Pather<'_>>) -> Option<Value> {
       // Single segment: look up on current record
       if path.is_single() {
           return tag_value(path.head(), ch, tags);
       }

       // Multi-segment: follow the first segment as a ref, recurse on tail
       let first_value = tag_value(path.head(), ch, tags)?;
       let ref_str = match first_value {
           Value::Str(s) => s,  // Refs stored as strings currently
           _ => return None,
       };

       let next_tags = pather?(&ref_str)?;
       // Recurse with remaining path (build a synthetic ChannelInfo from next_tags?)
       // Or refactor to be tag-dict-first throughout
   }
   ```

   **Design decision:** For simplicity in v1, if the pather returns a tag dict, we evaluate the remaining path against that dict only (not against any channel-level fields). This is correct Haystack semantics — path navigation is between records, not within channel metadata.

7. **Handler integration (`handlers.rs:108-145`)** — Provide a pather closure:
   ```rust
   // A pather that looks up a ref by checking if any other channel has a
   // matching ref tag. For v1, we can return None (no ref resolution in
   // the channel store) and the filter will simply fail to match path
   // comparisons. This is acceptable because the production system doesn't
   // currently have inter-channel refs.
   let pather: Option<&Pather<'_>> = None;
   let matches = filter::matches_full(&expr, ch, tags, pather);
   ```

   **Future:** Once the DynSlotStore gains a by-ref index, wire a real pather.

### A.4 Phase A3: Tests — ✅ COMPLETE (2026-04-13, written inline with A1+A2)

New tests in `filter.rs`:

```rust
// Parser tests
#[test] fn test_parse_path_double() {
    assert_eq!(parse("siteRef->area==\"Z1\"").unwrap(), ...);
}
#[test] fn test_parse_path_triple() { ... }
#[test] fn test_parse_path_error_dangling_arrow() {
    assert!(parse("siteRef->").is_err());
}

// not compound tests
#[test] fn test_parse_not_compound() {
    let expr = parse("not (enabled and point)").unwrap();
    assert!(matches!(expr, Expr::Not(_)));
}
#[test] fn test_eval_not_compound() { ... }

// Evaluation tests with mock pather
#[test] fn test_eval_path_dereference() {
    let pather = |r: &str| {
        if r == "@site1" {
            let mut t = HashMap::new();
            t.insert("area".to_string(), DynValue::Str("Z1".to_string()));
            Some(t)
        } else {
            None
        }
    };
    // Evaluate filter with pather, assert match
}
```

### A.5 Phase A4: Commit & deploy — ✅ COMPLETE (2026-04-14)

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

Deploy to 1-11 via the same pipeline we used for the previous fixes (cross-compile → scp → dpkg -i → verify).

---

## Project B: BACnet/IP Read-Only Driver

**Estimated effort:** 4-6 weeks (multi-session)
**Risk:** Medium
**Outcome:** Sandstar can discover, read, and subscribe to real BACnet/IP devices

### B.1 Scope

**In scope (MVP = Phase B1-B5):**
- UDP socket on port 47808
- BVLL framing (Original-Unicast-NPDU 0x0A, Original-Broadcast-NPDU 0x0B)
- NPDU version 1 with control=0 (local network only)
- APDU services: Who-Is (8), I-Am (0), ReadProperty (12)
- Object types: Device (8), Analog Input (0), Analog Output (1), Analog Value (2), Binary Input (3), Binary Output (4), Binary Value (5)
- Properties: present-value (85), object-name (77), units (117), object-list (76)
- Value types: Real (0x44), Unsigned (0x22), Signed (0x32), Boolean (0x10), Enumerated (0x91), Character String (0x74)
- `AsyncDriver` trait implementation matching Modbus pattern
- Integration tests with mock UDP server (localhost loopback)

**Out of scope initially:**
- WriteProperty (Phase B6 — a later extension)
- SubscribeCOV (Phase B6)
- ReadPropertyMultiple (optimization, not required)
- Segmentation (most devices don't need it for small reads)
- MS/TP serial transport
- BACnet router support (DNET/DADR)
- Alarms, trend logs, schedules

### B.2 File layout

BACnet will grow to ~1,700-2,000 lines. Split into submodules to keep files manageable:

```
crates/sandstar-server/src/drivers/bacnet/
├── mod.rs              # Public interface, BacnetDriver struct, AsyncDriver impl
├── frame.rs            # BVLL + NPDU + APDU encoding/decoding
├── object.rs           # Object types, property IDs, BacnetObject config
├── value.rs            # BACnet value encoding (Real, Unsigned, etc.)
├── transaction.rs      # Invoke ID management, pending requests
└── discovery.rs        # Who-Is/I-Am state machine
```

The existing single-file `bacnet.rs` (120 lines) becomes the entry point `mod.rs`.

### B.3 Phase B1: Frame encoding & unit tests — ✅ COMPLETE (2026-04-14)

**Goal:** Build the byte-level frame encoder/decoder. No network I/O yet.

**Deliverables:**

1. **`bacnet/frame.rs`** — ~500 lines
   - `fn encode_who_is(low_limit: Option<u32>, high_limit: Option<u32>) -> Vec<u8>`
   - `fn encode_read_property(invoke_id: u8, device_id: u32, obj_type: u8, instance: u32, property: u32) -> Vec<u8>`
   - `fn decode_bvll_header(data: &[u8]) -> Result<BvllHeader, BacnetError>`
   - `fn decode_npdu(data: &[u8]) -> Result<(NpduHeader, usize), BacnetError>` — returns header + APDU offset
   - `fn decode_apdu(data: &[u8]) -> Result<Apdu, BacnetError>`
   - Apdu enum: `IAm { device_id, max_apdu, segmentation, vendor_id }`, `ReadPropertyAck { object_id, property, value }`, `Error { class, code }`, etc.

2. **`bacnet/value.rs`** — ~300 lines
   - `fn encode_object_id(obj_type: u8, instance: u32) -> [u8; 6]` (context tag 0)
   - `fn encode_property_id(property: u32) -> Vec<u8>` (context tag 1)
   - `fn decode_application_tag(data: &[u8]) -> Result<(BacnetValue, usize), BacnetError>`
   - `BacnetValue` enum: `Real(f32)`, `Unsigned(u32)`, `Signed(i32)`, `Boolean(bool)`, `Enumerated(u32)`, `CharString(String)`, `ObjectId(u8, u32)`
   - `fn bacnet_value_to_f64(v: &BacnetValue) -> f64` — conversion for sync_cur

3. **`bacnet/object.rs`** — ~150 lines
   - `ObjectType` enum with discriminants matching BACnet spec (AnalogInput=0, AnalogOutput=1, ...)
   - `PropertyId` enum (PresentValue=85, ObjectName=77, Units=117, ObjectList=76, ...)
   - `BacnetObject` struct used by BacnetDriver for configured points

4. **Unit tests** (~40-50 tests in these modules):
   - Who-Is encoding produces `0x81 0x0B 0x00 0x08 0x01 0x20 0xFF 0xFF 0x00 0xFF 0x10 0x08` (or similar well-known bytes)
   - ReadProperty encoding matches a known-good capture
   - I-Am decoding extracts device_id correctly from a captured packet
   - Round-trip: encode → decode → assert equal
   - Malformed frame detection (too short, bad magic, bad tag)

**Verification:**
- `cargo test -p sandstar-server drivers::bacnet` passes
- Byte output matches captured wireshark frames (or ASHRAE 135 spec examples)

**Checkpoint:** Can generate and parse bytes, but nothing talks to the network yet.

### B.4 Phase B2: Discovery state machine (Week 2, ~8 hours) — ✅ COMPLETE (2026-04-14)

**Goal:** `open()` sends Who-Is, collects I-Am responses into a device table.

**Deliverables:**

1. **`bacnet/discovery.rs`** — ~250 lines
   - `DeviceInfo { instance: u32, address: SocketAddr, max_apdu: u16, vendor_id: u16 }`
   - `DeviceRegistry { devices: HashMap<u32, DeviceInfo> }`
   - `async fn send_who_is(socket: &UdpSocket, broadcast: SocketAddr) -> io::Result<()>`
   - `async fn collect_i_am(socket: &UdpSocket, timeout: Duration) -> Vec<DeviceInfo>`

2. **`bacnet/mod.rs` — `BacnetDriver::open()` implementation:**
   ```rust
   async fn open(&mut self) -> Result<DriverMeta, DriverError> {
       // 1. Bind UDP socket to 0.0.0.0:47808 (or configured port)
       let socket = UdpSocket::bind(("0.0.0.0", self.port)).await
           .map_err(|e| DriverError::CommFault(format!("bind: {e}")))?;
       socket.set_broadcast(true)?;

       // 2. Send Who-Is broadcast
       let broadcast: SocketAddr = format!("{}:{}", self.broadcast_addr, self.port).parse()?;
       socket.send_to(&frame::encode_who_is(None, None), broadcast).await?;

       // 3. Collect I-Am responses for 2 seconds
       let devices = discovery::collect_i_am(&socket, Duration::from_secs(2)).await;

       // 4. Store results, set status
       self.socket = Some(socket);
       self.device_registry.bulk_insert(devices);
       self.status = if self.device_registry.is_empty() {
           DriverStatus::Fault("no BACnet devices found".into())
       } else {
           DriverStatus::Ok
       };

       Ok(DriverMeta {
           model: Some(format!("BACnet/IP {} devices", self.device_registry.len())),
           ..Default::default()
       })
   }
   ```

3. **Integration test** (`tests/bacnet_discovery.rs`):
   - Spawn a mock I-Am responder on localhost:47808 (use a different port in tests to avoid conflicts)
   - Call `driver.open()`
   - Assert that the mock device appears in `device_registry`

**Verification:**
- `cargo test bacnet_discovery` passes
- Tracing shows Who-Is and I-Am traffic at debug level

**Checkpoint:** Can discover devices on a real or mock network.

### B.5 Phase B3: ReadProperty request/response (Week 3, ~10 hours) — ✅ COMPLETE (2026-04-14)

**Goal:** `sync_cur()` reads present-value from configured points.

**Deliverables:**

1. **`bacnet/transaction.rs`** — ~200 lines
   - `TransactionTable` — maps invoke_id to `oneshot::Sender<Result<Apdu, BacnetError>>`
   - `async fn transact(&mut self, device: &DeviceInfo, apdu: &[u8], timeout: Duration) -> Result<Apdu, BacnetError>`
   - Timeout handling with retries (configurable, default 3 retries / 5s each)
   - Concurrent request support (multiple in-flight invoke IDs)

2. **`BacnetDriver::sync_cur()` implementation:**
   ```rust
   async fn sync_cur(&mut self, points: &[DriverPointRef])
       -> Vec<(u32, Result<f64, DriverError>)> {
       let mut results = Vec::with_capacity(points.len());
       for pt in points {
           let obj = match self.objects.get(&pt.point_id) {
               Some(o) => o.clone(),
               None => {
                   results.push((pt.point_id, Err(DriverError::ConfigFault(
                       format!("no BACnet object for point {}", pt.point_id)
                   ))));
                   continue;
               }
           };
           let device = match self.device_registry.get(&obj.device_id) {
               Some(d) => d,
               None => {
                   results.push((pt.point_id, Err(DriverError::CommFault(
                       format!("device {} not in registry", obj.device_id)
                   ))));
                   continue;
               }
           };
           match self.read_present_value(device, &obj).await {
               Ok(v) => results.push((pt.point_id, Ok(v))),
               Err(e) => results.push((pt.point_id, Err(e))),
           }
       }
       results
   }
   ```

3. **Helper method `read_present_value()`:**
   - Build ReadProperty frame
   - Send via `transact()`
   - Parse Complex-ACK response
   - Extract present-value from application-tagged value
   - Apply optional scale/offset
   - Return `f64`

4. **Unit tests for transact:**
   - Invoke ID allocation and wrap-around
   - Timeout handling
   - Concurrent requests don't mix up responses
   - Error responses (Error PDU) propagate as `DriverError::RemoteStatus`

**Verification:**
- With a running BACnet simulator (YABE, bacnet-stack demo, or a hand-rolled mock), `driver.sync_cur()` returns real values
- Unit tests cover frame round-trips and error paths

**Checkpoint:** Can read one property from one object. Sandstar can now be a BACnet client.

### B.6 Phase B4: `learn()` — Object enumeration (Week 4, ~6 hours) — ✅ COMPLETE (2026-04-14)

**Goal:** `GET /api/drivers/<id>/learn` returns all readable points from all discovered devices.

**Deliverables:**

1. **`BacnetDriver::learn()` implementation:**
   ```rust
   async fn learn(&mut self, _path: Option<&str>) -> Result<LearnGrid, DriverError> {
       let mut grid = Vec::new();
       // Clone the device list to avoid holding a borrow while we iterate
       let devices: Vec<DeviceInfo> = self.device_registry.all().into_iter().cloned().collect();

       for device in devices {
           // 1. Read Device.object-list property (returns array of object IDs)
           let object_list = self.read_object_list(&device).await?;

           for (obj_type, instance) in object_list {
               // 2. For each object, read its name
               let name = self.read_object_name(&device, obj_type, instance).await
                   .unwrap_or_else(|_| format!("{}-{}", obj_type, instance));

               // 3. Determine if it's readable (skip Device itself, skip write-only)
               let kind = match obj_type {
                   0 | 1 | 2 => "Number".to_string(),  // AI, AO, AV
                   3 | 4 | 5 => "Bool".to_string(),    // BI, BO, BV
                   _ => continue,  // Skip unknown types
               };

               let mut tags = HashMap::new();
               tags.insert("deviceId".to_string(), device.instance.to_string());
               tags.insert("objectType".to_string(), obj_type.to_string());
               tags.insert("instance".to_string(), instance.to_string());

               grid.push(LearnPoint {
                   name: format!("{}-{}-{}", device.instance, obj_type, instance),
                   address: format!("{}:{}:{}", device.instance, obj_type, instance),
                   kind,
                   unit: None,  // TODO Phase B5: also read units property
                   tags,
               });
           }
       }
       Ok(grid)
   }
   ```

2. **Helper methods:**
   - `read_object_list(device) -> Result<Vec<(u8, u32)>, BacnetError>` — Reads Device.object-list property, which returns an array of object identifiers
   - `read_object_name(device, obj_type, instance) -> Result<String, BacnetError>`

**Verification:**
- Against a mock device with 3 analog inputs + 2 binary outputs, `learn()` returns 5 points
- Real device test: point a BACnet simulator at us and verify the learn output

**Checkpoint:** Point discovery works. Operators can now see what a BACnet device exposes.

### B.7 Phase B5: Wire into main.rs + end-to-end test (Week 5, ~4 hours) — ✅ COMPLETE (2026-04-14)

**Goal:** The server can create a BacnetDriver via config/CLI and it actually works in production mode.

**Deliverables:**

1. **Config struct** in `bacnet/mod.rs`:
   ```rust
   #[derive(Debug, Clone, serde::Deserialize)]
   pub struct BacnetConfig {
       pub id: String,
       pub port: Option<u16>,        // default 47808
       pub broadcast: Option<String>, // default "255.255.255.255"
       pub objects: Vec<BacnetObjectConfig>,
   }

   #[derive(Debug, Clone, serde::Deserialize)]
   pub struct BacnetObjectConfig {
       pub point_id: u32,
       pub device_id: u32,
       pub object_type: u8,
       pub instance: u32,
       pub unit: Option<String>,
       pub scale: Option<f64>,
       pub offset: Option<f64>,
   }
   ```

2. **Factory function:**
   ```rust
   pub fn from_config(config: BacnetConfig) -> Result<BacnetDriver, DriverError> {
       let mut driver = BacnetDriver::new(
           config.id,
           config.broadcast.unwrap_or_else(|| "255.255.255.255".into()),
           config.port.unwrap_or(47808),
       );
       for obj in config.objects {
           driver.add_object(obj.point_id, BacnetObject {
               device_id: obj.device_id,
               object_type: obj.object_type,
               instance: obj.instance,
               unit: obj.unit,
               scale: obj.scale.unwrap_or(1.0),
               offset: obj.offset.unwrap_or(0.0),
           });
       }
       Ok(driver)
   }
   ```

3. **Wire into main.rs** — add BACnet driver registration when config specifies it.

4. **End-to-end integration test** (tests/bacnet_e2e.rs):
   - Spawn a mock BACnet device on a random port
   - Configure a BacnetDriver to talk to it
   - Register with DriverManager
   - Call open → learn → sync_cur → close
   - Assert all values flow correctly

**Verification:**
- Can configure a BACnet driver via YAML/TOML
- End-to-end test passes
- CI green (cargo fmt, clippy, test)

**Checkpoint:** MVP is complete. Ready for production testing.

### B.8 Phase B6: Production deployment + extensions (Week 6+)

**Goal:** Deploy to 1-11 and iterate based on real-world feedback.

**Deliverables:**

1. **Cross-compile for ARM, deploy .deb to 1-11**
2. **Run in production alongside existing channels** for a day (no BACnet devices yet, just verify it doesn't crash)
3. **Test against a real BACnet device** (if you have one available) or continue with the simulator

**Optional extensions (not required for MVP):**
- **WriteProperty** — ✅ Phase B7 complete (2026-04-15) — adds write capability via `AsyncDriver::write()`.
  Delivered: `frame::encode_write_property` + `encode_simple_ack`, `Apdu::WritePropertyRequest` +
  `Apdu::SimpleAck` decoder variants, `BacnetDriver::write_property()` with retry loop,
  `AsyncDriver::write()` impl that inverts the `sync_cur` scale/offset and dispatches Real (AI/AO/AV)
  or Enumerated (BI/BO/BV) values at priority 16. End-to-end tests
  `e2e_write_succeeds_with_simple_ack` and `e2e_write_error_pdu_returns_remote_status` cover both the
  SimpleAck and Error PDU paths via UDP loopback.
- **SubscribeCOV (wire-level)** — ✅ Phase B8 complete (2026-04-16) — `BacnetDriver::subscribe_cov` +
  `unsubscribe_cov` + `on_watch`/`on_unwatch` implementations. Frame codec: `SubscribeCovRequest`
  encoder/decoder, `UnconfirmedCovNotification` / `ConfirmedCovNotification` decoders,
  `CovNotification` + `CovPropertyValue` structs. Subscription state tracked in `cov_subscriptions`
  HashMap keyed by `subscriberProcessIdentifier`. Default lifetime 300s. End-to-end tests
  `e2e_on_watch_subscribes_cov` and `e2e_on_unwatch_sends_cancel` validate the wire-level
  subscribe/cancel path via UDP loopback. Python simulator `tools/bacnet_sim.py` gained
  `parse_subscribe_cov` + SubscribeCOV Simple-ACK handler. Notification receiver task for pushing
  updates to watchers is reserved for Phase B8.1 (requires I/O refactor).
- **COV Notification Reception** — ✅ Phase B8.1 complete (2026-04-16) — CovCache + inline notification handling in all recv loops. sync_cur returns cached values for COV-subscribed points (max_age=600s). No background receiver task needed — notifications processed as side-effects of existing I/O.
- **COV Subscription Renewal** — ✅ Phase B8.2 complete (2026-04-16) — `CovSubscription.subscribed_at` + `BacnetDriver.renewal_interval` (default 240s = 80% of 300s lifetime). `sync_cur()` calls `renew_due_subscriptions()` which re-issues SubscribeCOV for entries past the threshold. No new tokio tasks — piggybacks on the existing poll loop. Failed renewals log a warning and retry next cycle. End-to-end test `e2e_cov_renewal_sends_second_subscribe` drives the full flow through the `DriverHandle` actor with a 1ms renewal_interval and asserts at least 2 SubscribeCOV requests arrive at the mock (initial + renewal).
- **ReadPropertyMultiple** — ✅ Phase B9 complete (2026-04-16) — `sync_cur()` now batches points
  by device into a single RPM request and falls back to individual `ReadProperty` on transport
  error or when the device replies with an Error PDU. Delivered: `frame::RpmRequestSpec` +
  `frame::RpmResult`, `encode_read_property_multiple` + `encode_read_property_multiple_ack`,
  matching `Apdu::ReadPropertyMultipleRequest` + `Apdu::ReadPropertyMultipleAck` decoder variants,
  and `BacnetDriver::read_properties_multiple()` with a retry loop mirroring the single-RP path.
  End-to-end test `e2e_sync_cur_uses_rpm` in `e2e_test.rs` drives a 2-point batch through the
  `DriverHandle` actor, asserts both values flow back correctly, and asserts the mock responder
  observed exactly ONE inbound service-0x0E request (proving the batched path — 2 individual
  reads would fail the test). The Python simulator `tools/bacnet_sim.py` gained `parse_rpm_request`
  + `encode_rpm_ack` helpers wired into the main dispatch loop so that running
  `py tools/bacnet_sim.py` now answers RPM as well as ReadProperty/WriteProperty.
- **Router/BBMD support** — ✅ Phase B10 complete (2026-04-16) — Register-Foreign-Device BVLL encoder/decoder, Distribute-Broadcast-To-Network, BVLL-Result decoder, Forwarded-NPDU decode. BacnetConfig.bbmd + BacnetDriver.bbmd_addr config. Auto-registration in open() with 300s TTL, non-fatal on failure. Who-Is sent via both local broadcast AND BBMD distribute.

### B.9 Risks and mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Byte layout mismatch with real devices | High | Medium | Start with bacnet-stack demo tool as reference, capture real packets early |
| UDP broadcast doesn't work on BeagleBone | Medium | Medium | Use device IP directly as first fallback; add unicast Who-Is targeting specific devices |
| Invoke ID collisions under load | Low | High | TransactionTable with explicit lifetime tracking, not just next++ |
| Segmentation required for large object lists | Medium | Medium | Phase 1 reads device object-list without segmentation; if it fails, document limitation and add segmentation in Phase B9 |
| ASN.1 BER encoding edge cases | High | Low | Build an exhaustive test suite from captured frames; lean on ASHRAE 135 Annex G examples |
| Memory allocation on embedded target | Low | Low | Use `Vec::with_capacity` aggressively; avoid nested allocations in hot path |

---

## Cross-cutting Concerns

### Testing philosophy

**For both projects:**

1. **Unit tests first** — Frame encoding/decoding, parser logic, AST manipulation. No I/O.
2. **Integration tests with mocks** — Tokio mpsc for filter pather, UDP loopback for BACnet.
3. **Real hardware smoke test** — Deploy to 1-11 and observe.

**Test count targets:**
- Filter improvements: +15-20 new tests (existing 698 lines of tests stay)
- BACnet: +50-80 new tests (frame encoding/decoding is test-heavy)

### Code style

Follow existing Sandstar patterns:
- `snake_case` for functions and variables
- `PascalCase` for types
- `tracing::debug!` / `tracing::info!` for logging
- `thiserror` for custom error types
- Batch operations — don't process one item at a time when the protocol supports batching

### Documentation

- Each new public API gets rustdoc
- BACnet frame formats documented in module doc comments with the byte layout
- HowToUse guide for BACnet driver once MVP lands

### CI green requirement

Every commit must keep CI green:
```bash
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

No exceptions. Fix lint warnings immediately; don't accumulate technical debt.

---

## Verification Strategy

### Filter improvements (Project A)

| Check | Command | Expected |
|-------|---------|----------|
| Parser accepts `not (a and b)` | Unit test | Returns `Expr::Not(Box::new(And(...)))` |
| Parser accepts `a->b==c` | Unit test | Returns `Cmp(Path{segments:[a,b]}, Eq, Str(c))` |
| Evaluation handles double-not | Unit test | `not not point` matches points |
| Existing 698 lines of tests | `cargo test -p sandstar-server filter` | All pass |
| Clippy clean | `cargo clippy --workspace -- -D warnings` | 0 warnings |
| Production smoke test | Deploy + hit `/api/read?filter=...` | Returns expected filtered set |

### BACnet driver (Project B) — per-phase gates

**Phase B1 gate (Frame encoding):**
- 40+ unit tests pass
- Byte output matches ASHRAE 135 Annex G examples
- Round-trip encoding → decoding → equality

**Phase B2 gate (Discovery):**
- Mock I-Am responder test passes
- `driver.open()` populates device registry
- Tracing shows Who-Is and I-Am frames

**Phase B3 gate (ReadProperty):**
- `sync_cur()` returns real values from mock device
- Invoke ID management handles concurrent requests
- Timeout and retry work correctly

**Phase B4 gate (Learn):**
- `learn()` returns a populated LearnGrid
- All object types categorized correctly (Number vs Bool)

**Phase B5 gate (Integration):**
- End-to-end test passes
- Config deserialization works
- CI green

**Phase B6 gate (Production):**
- Deployed to 1-11 without crashing
- No new errors in production logs
- Existing channels still healthy

---

## Appendix A: Files to Create

### Project A (Filter)

| File | Lines Added | Purpose |
|------|-------------|---------|
| `rest/filter.rs` | +100 (modify existing) | New Path type, Not variant, path parsing, path evaluation, pather callback |
| `rest/handlers.rs` | +5 | Pass None pather to matches_full |

### Project B (BACnet)

| File | Lines | Purpose |
|------|-------|---------|
| `drivers/bacnet/mod.rs` | ~400 | BacnetDriver struct, AsyncDriver impl |
| `drivers/bacnet/frame.rs` | ~500 | BVLL/NPDU/APDU encoding/decoding |
| `drivers/bacnet/value.rs` | ~300 | BACnet value types and BER encoding |
| `drivers/bacnet/object.rs` | ~150 | ObjectType, PropertyId enums, BacnetObject config |
| `drivers/bacnet/transaction.rs` | ~200 | Invoke ID management, pending requests |
| `drivers/bacnet/discovery.rs` | ~250 | Who-Is/I-Am state machine |
| `tests/bacnet_e2e.rs` | ~200 | End-to-end integration test |
| `tests/bacnet_discovery.rs` | ~100 | Discovery integration test |
| **Total** | **~2,100** | |

The existing `drivers/bacnet.rs` (120 lines) gets replaced by the new `drivers/bacnet/mod.rs`.

---

## Appendix B: Testing with a BACnet Simulator

### Option 1: YABE (Yet Another BACnet Explorer)

- **URL:** https://sourceforge.net/projects/yabe/
- **Platform:** Windows GUI
- **Setup:** Runs a virtual device that responds to Who-Is, supports ReadProperty
- **Use:** Development smoke test on the same LAN as the BeagleBone

### Option 2: bacnet-stack demo applications

- **URL:** https://github.com/bacnet-stack/bacnet-stack
- **Platform:** Linux command-line
- **Setup:** `./server` runs a virtual BACnet device with configurable objects
- **Use:** Run on the dev machine, point Sandstar driver at `127.0.0.1:47808`

### Option 3: Hand-rolled mock in Rust tests

- **Platform:** Rust integration tests
- **Setup:** Tokio task binds a UDP socket, responds to known frames
- **Use:** Unit-test integration without any external tooling

**Recommendation:** Start with option 3 for automated tests, use option 2 (bacnet-stack) for smoke testing before committing each phase.

---

## Appendix C: Timeline

| Week | Work | Deliverable |
|------|------|-------------|
| **Day 1** | Filter Phase A1 + A2 + A3 + A4 | `not` and `->` shipped to 1-11 |
| **Week 1** | BACnet Phase B1 — frame encoding | 40+ unit tests, byte-level correctness |
| **Week 2** | BACnet Phase B2 — discovery | Mock-tested Who-Is/I-Am |
| **Week 3** | BACnet Phase B3 — ReadProperty | sync_cur returns real values |
| **Week 4** | BACnet Phase B4 — learn | Object enumeration works |
| **Week 5** | BACnet Phase B5 — integration | End-to-end test + config |
| **Week 6** | BACnet Phase B6 — production | Deployed to 1-11 |
| **Week 7** | BACnet Phase B7 — WriteProperty ✅ 2026-04-15 | `encode_write_property`, `SimpleAck`, `write_property()`, `AsyncDriver::write()`, 2 E2E tests |
| **Week 8** | BACnet Phase B9 — ReadPropertyMultiple ✅ 2026-04-16 | `RpmRequestSpec` + `RpmResult`, `encode_read_property_multiple` + ACK, `read_properties_multiple()`, batched `sync_cur()` with fallback, `e2e_sync_cur_uses_rpm` E2E test, Python sim RPM handler |
| **Week 9** | BACnet Phase B8 — SubscribeCOV (wire-level) ✅ 2026-04-16 | `subscribe_cov` + `unsubscribe_cov`, `on_watch`/`on_unwatch`, `CovNotification` structs, 2 E2E tests, Python sim handler |
| **Week 10** | BACnet Phase B8.1 — COV notification reception ✅ 2026-04-16 | CovCache, inline notification handling, sim COV sender |
| **Week 10** | BACnet Phase B10 — BBMD/Router support ✅ 2026-04-16 | Register-Foreign-Device, BVLL-Result, BacnetConfig.bbmd, E2E test, sim BBMD handler |

Each phase ends with a verification gate and a git commit. No long-running branches — commit and push at every checkpoint.

---

*Plan generated from 3-agent discovery analysis of current code state, BACnet protocol requirements, and driver framework patterns. Ready to execute Phase A1 on demand.*
