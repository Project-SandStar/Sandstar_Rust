# Implementation Plan: Phase 14.0G — Native Editor Backend

**Goal:** Remove the SkySpark + `bassgSoxWebSocket` Fantom pod dependency of the Vue-based Sedona Web Editor. When this plan completes, a freshly-provisioned BeagleBone running only the Sandstar `.deb` package serves the full visual DDC editor from `http://<device>/editor`. No Java runtime, no SkySpark, no Fantom.

**Started:** 2026-04-23
**Target release:** v2.9.0
**Estimated total:** ~18–20 engineering days (G-3 re-scoped: native tree serializer, no SAB parser)

---

## Why this phase exists

Phase 14.0B–F (the editor UI itself) was built in a separate monorepo (`C:\Coding\swe\sedonaWebEditor`) as a Vue 3 + Rete.js SPA. In its current form the frontend talks to a **SkySpark extension named `bassgSoxWebSocket`** — a Fantom pod that bridges JSON-over-WebSocket requests from the browser down to UDP DASP/SOX datagrams on a Sedona-speaking device.

This works, but it breaks Sandstar's core value proposition ("single self-contained Rust binary on a 512 MB BeagleBone"). It also couples the editor to a proprietary analytics platform that some deployments cannot license, and it hides a second failure mode behind a second process. Observed in the wild on 2026-04-23: SkySpark at `192.168.1.8:9099` was unreachable, which made the editor unreachable even though the underlying Sandstar engine was healthy.

The fix is to reimplement, natively in Rust, the handful of HTTP/WebSocket endpoints the editor actually consumes, and then serve the editor's compiled static bundle from Sandstar itself.

---

## Endpoint delta (editor needs ↔ Sandstar has)

| Editor endpoint                                        | Fantom pod | Sandstar today                  | Gap weight |
|--------------------------------------------------------|:---------:|---------------------------------|:----------:|
| `WS /live` (JSON, 19 msgTypes)                         | ✅        | ❌ (has its own `/api/rows`)    | **heavy**  |
| `GET /live/sedonaState` (bulk `app.sab` → JSON)        | ✅        | ❌                              | **heavy**  |
| `GET /file/:uri`                                       | ✅        | ❌                              | medium     |
| `PUT /file/:uri`                                       | ✅        | ❌                              | medium     |
| `POST /file/sox/{fileOpen,fileChunk,fileClose,fileRename}` | ✅    | ❌                              | medium     |
| `POST /kit` (install kit)                              | ✅        | ❌                              | medium     |
| `GET /live/soxstatus`                                  | ✅        | partial (`/health`, `/api/status`) | light   |
| `GET /library/?q=...`                                  | ❓ (SkySpark) | ❌                          | medium     |
| `GET /tags`                                            | ❓ (SkySpark) | partial (`/api/tags`)      | light      |
| `GET /api/{proj}/ext/bassgSoxWebSocket/convertSabToSax`| ✅ (Axon) | ❌                              | light      |
| `GET /api/{proj}/ext/bassgSoxWebSocket/{status,restart}` | ✅      | n/a                             | bypassable |

**What Sandstar already has that the editor does not use today** (and could leverage after small client-side changes):

- `/api/rows` — Sandstar's own RoWS WebSocket with identical SOX semantics but slightly different JSON envelope (SoxHandlers Phase 14.0A).
- `/api/sox/*` — 11 REST endpoints (tree, comp, palette, add, delete, rename, write, invoke, link, pos, reorder) from Phase 14.0A.
- SCRAM-SHA-256 auth, Bearer tokens, rate limiter, CORS — middleware is reusable.

---

## Sub-phases

Notation: each sub-phase has a **status**, a short **scope**, and **exit criteria**. Update the status inline as work progresses.

Status legend: ⬜ not started · 🟨 in progress · ✅ complete · ⏸ blocked/paused

---

### G-0 · Preflight check: SAB parser availability ✅
**Status:** ✅ complete 2026-04-23 — see "Preflight findings" below
**Scope:** Determine whether `sandstar-svm` already contains a SAB (Sedona Application Binary) decoder we can reuse. This decision swings the G-3 scope by roughly three days.
**Outcome:** No full SAB parser exists. `sab_validator.rs` does header + opcode scanning only. G-3 must ship the full parser; total plan lands at the upper end of the 18–25 day range.

---

### G-1 · Compatibility spike ✅
**Status:** ✅ complete 2026-04-23 — see `docs/Progress/NOTES_EDITOR_ROWS_SPIKE.md`
**Scope (1–2 days):** Drive the editor's `WorkerCoordinator` at `ws://<sandstar>/api/rows` through a thin dialect-translation shim. Enumerate exactly which Fantom msgTypes map 1:1 to RoWS ops, which need rewriting, and which have no equivalent.
**Outcome:** 19 msgTypes analysed vs. 19 RoWS ops. **8 clean map, 4 adaptable, 7 gap** (of which 3 are G-4's file ops). Wire-layer envelope shim rejected on maintainability grounds (replyNum ring vs. string id, missing ops, bijective-but-non-trivial event translation). **Recommended path: refactor `rows.rs::handle_client_msg` into a pure `apply_op(state, SoxOp) -> SoxOpResult` in a shared `sox_ops.rs` module, then G-2 implements `/live` as a second envelope over the same core.** 4 new ops (`version`, `reorder`, `query`, `closeWatch`) added to G-2's scope — they also improve RoWS coverage as a side benefit. No schedule impact: G-2 still 3–4 days.

---

### G-2 · Native `/live` JSON WebSocket endpoint ✅
**Status:** ✅ complete (MVP) 2026-04-23 — G-2a ✅ + G-2b ✅ + G-2c ✅

Three sub-steps, totalling 3–4 days:

**G-2a · Refactor `rows.rs` into shared `sox_ops.rs` ✅ complete 2026-04-23**
Extracted the command dispatcher from `rest/rows.rs` into a new module `crates/sandstar-server/src/sox/sox_ops.rs`. `SoxOp` is an enum over all 24 ops (19 RoWS v1 + 5 G-2b additions). `SoxResponse` is `Ok(JsonValue) | Error { code, message }`. `apply_op(state, op, session)` is the pure dispatcher. `rest/rows::handle_client_msg` now parses JSON, calls `apply_op`, and wraps the response in the RoWS envelope (via `make_result` / `make_error` / special-case `make_pong` for ping). `RowsState` is preserved as a re-export of `sox_ops::SoxState` for router compatibility. All 35 existing RoWS tests still pass; full workspace suite 1385 passed. See diff in commit "refactor(sox): extract apply_op dispatcher to sox_ops.rs (G-2a)".

**G-2b · Add 5 missing ops to `SoxOp` ✅ complete 2026-04-23**

Wire contract frozen in `docs/Progress/NOTES_FANTOM_MSGTYPE_SEMANTICS.md` (2026-04-23). The 5 ops:

| Op           | Request                              | Response                                                       |
|--------------|--------------------------------------|----------------------------------------------------------------|
| `version`    | `{}`                                 | `{kits: [{name, checksum:u32}]}`                               |
| `versionExt` | `{}`                                 | `{platformId:"", scodeFlags:0, kitVersions:[...], pairs:[]}`   |
| `reorder`    | `{compId, children:[u16]}`           | `{ok:true}` + emits `treeChanged` with `action:"reordered"`    |
| `query`      | `{queryType:"s", kitId, typeId}`     | `{compIds:[u16]}` — type-match variant for v1                  |
| `closeWatch` | `{watchId}`                          | `{watchId, unsubscribed:N}` — clears all subs on session       |

Notes:
- `version` / `versionExt`: kit list comes from `ManifestDb`; verify checksum field exists (open question flagged in the spec doc).
- `reorder`: existing REST `PUT /api/sox/comp/{id}/reorder` already has the tree mutation; wrap it. Add `"reordered"` variant to `treeChanged` event.
- `query`: v1 implements type-match (exact `(kit_id, type_id)` on component's own type). Service-interface walk deferred to a future minor release if a real caller needs it.
- `closeWatch`: implicit-subscription variant — clears the session's `HashSet<u16>`. Multi-watchId indirection deferred until a real caller requires it.

Each op got a handler in `sox_ops.rs` plus a unit test exercising the RoWS-side parse + dispatch in `rest/rows.rs::tests`. 7 new tests added (42 total in `rest::rows`, 1385 total workspace-wide). Known open question "does `ManifestDb` store kit checksums?" resolved: `sox_handlers::DEFAULT_KITS` already carries `KitInfo { name, checksum: u32, version }` for all 15 built-in kits (values like `sys=0xd3984c51`, `hvac=0x7264c67c`, …). `version` / `versionExt` handlers iterate that table directly. RoWS gains these 5 ops as a side benefit.

**G-2c · `crates/sandstar-server/src/rest/live.rs` ✅ complete 2026-04-23 (MVP).**
New 650-line module at `/live` backed by `apply_op` from G-2a. Handles
all 19 Fantom msgTypes with a single dispatcher:

* Version (`v` / `y`), subscribe / unsubscribe, write, add, delete,
  rename, link add/delete, reorder, query, invoke, closeWatch, ping →
  straight through `apply_op`, response rendered into Fantom envelope
  shape per-msgType (e.g. `subscribe → numSubscribed`, `version →
  flattened kit list`).
* ReadComp (`c`) → dedicated direct-tree-access path that encodes slot
  values with `sox_handlers::encode_slot_value_raw`, base64-wraps them,
  and returns `compProps.propValues` bit-compatible with what the
  Fantom pod produces. `what='t'` builds `compTree`, `what='l'` builds
  `compLinks`.
* Event push loop (`e` frames) running on the same 500 ms cadence as
  RoWS. Subscribed components emit tree-change frames when children
  shift and config-slot frames on value change. Binary slot payloads
  use the same encoder as ReadComp.
* File ops (`f`, `z`, `b`) return `!` errors pointing to Phase 14.0G-4
  — keeps the envelope honest so the editor can fall back to the
  HTTP `/file/*` path once G-4 lands.
* Router wired at `/live` in `rest::mod::build_app`. Route merges
  alongside `/api/rows`; both share `SoxState` via the existing SOX
  bootstrap path — zero duplicated state.

**Tests:** 12 unit tests in `rest::live::tests` covering every branch
the MVP exercises (ping echo, version, versionExt, subscribe,
closeWatch, query, unknown msgType, file-op rejection, ReadComp
not-found, invalid JSON, encode_slots_respects_what).
`cargo test -p sandstar-server --lib`: 1397 passed (was 1385 before
G-2c; +12 live tests). `cargo clippy`: no new warnings.

**Follow-ups not blocking the MVP** (tracked here; none affect editor
bring-up):

* `WriteSlot`: value is decoded as a base64 → UTF-8 string today.
  Covers string slots; numeric slots need the binary→typed-value path
  implemented once a concrete caller surfaces (the editor's typed
  writes currently go through the REST `/api/sox/.../slot` endpoint
  which is JSON-native).
* `AddComp`: `configProps` payload is ignored in the MVP. Editor's
  add-component flow does not rely on it (slots initialise from
  manifest defaults); if that changes, decode base64 and apply.
* Event push is collapsed to a single `c`-what frame per changed
  component. If the editor's state worker rejects the collapsed form
  or expects separate config+runtime frames, split in a follow-up.
* Subscription's `whatMask` (per Fantom: 0x1 tree / 0x2 config /
  0x4 runtime / 0x8 links) is currently accepted and ignored. Native
  fidelity once a caller actually varies it.

**Exit criteria (met):** Binary runs, `/live` upgrade succeeds, all 12
test branches pass, `/api/rows` tests still green, workspace-wide test
count up from 1385 to 1397. Ready for end-to-end editor validation.

---

### G-3 · `/live/sedonaState` bulk loader ✅
**Status:** ✅ complete (MVP) 2026-04-23

**Scope revision (critical).** Original plan assumed we had to parse `.sab`
like the Fantom pod does. Wrong framing: the pod reverse-engineers SAB
because it is an external proxy that can only see the file on disk. Sandstar
**is** the engine — the entire component tree is live in `ComponentTree` +
`ManifestDb` already. The bulk loader just serialises the in-memory state
into the `SedonaStateResponse` JSON shape the editor expects. No binary
parser, no reverse engineering, no risk on the file-format axis.

**G-3a · `ComponentTree → SedonaStateResponse` serializer** ✅ —
`crates/sandstar-server/src/rest/sedona_state.rs::build_snapshot`. Walks
the shared `ComponentTree`, serialises each component with per-slot
base64-encoded values keyed by slot name, `meta` int + decoded
`{x, y, groups}` position, parent/children IDs and fully-qualified kit
`type` name. Links emitted once per pair (from-side only). Reuses
`encode_slot_value_raw` so `/live`, per-slot reads, event frames, and
bulk load all round-trip through the same binary encoder.

**G-3b · HTTP handler + router wiring** ✅ — `GET /live/sedonaState`
registered in `rest::mod::build_app` on the same `Router` as `/live`,
sharing `LiveState`. Query params parsed with serde: `h`, `p`, `u`,
`pw`, `saveFirst` (last four accepted for Fantom parity; `saveFirst` is
a no-op on native because there is nothing to flush — RAM *is* the live
state).

**G-3c · Unit + protocol tests** ✅ — 6 unit tests in
`rest::sedona_state::tests` covering schema shape, component keying,
per-slot `props` dict, base64-encoded `value` lengths, position
decoding (verifies the `0x20` byte == x = 32 packing), single-emission
links, host/port echo. Live protocol test with `curl` against the dev
server (8085) round-tripped a valid `SedonaStateResponse` including
all 15 kits with checksums. `cargo test -p sandstar-server --lib`:
1 403 passed (was 1 397 before G-3; +6).

**G-3d · Cross-check against the editor** `⬜` on user return — open
the Vue editor pointed at local Sandstar, verify it renders from a
single `/live/sedonaState` call without falling back to per-component
`c` reads. Not blocking further work; if the editor rejects anything
we tweak the shape in-place.

**SAB parser status:** explicitly dropped. No SAB-aware Rust code
needed in Sandstar. Phase G-6 (`convertSabToSax`) likewise no longer
needs a SAB parser — it can regenerate SAX from the live tree directly
or be deferred until a real caller proves it's needed.

**Scope (4–5 days, biggest performance win):** New file `crates/sandstar-server/src/rest/sedona_state.rs`. HTTP GET handler that:
- If `saveFirst=true`, sends a SOX `App.save()` to the device first
- Reads `app.sab` via DASP file-get (or via in-process SVM if the device is local)
- Decodes SAB binary to the `SedonaStateResponse` shape (schema kits, components map, links list, maxId, host, port, timestamp, loadTimeMs) — byte-for-byte compatible with Fantom pod output so the Vue client needs zero changes
- Depends on G-0 outcome: reuse existing SAB decoder if available, otherwise ship a minimal one (see Appendix A)

**Exit criteria:** Network tab shows a single `GET /live/sedonaState` returning sub-1-second with a full tree identical to the Fantom pod's response for the same device. Editor opens the device without issuing the 73+ sequential SOX calls seen pre-G-3.

---

### G-4 · File transfer endpoints ✅
**Status:** ✅ complete (MVP) 2026-04-23

**Scope revision (critical).** Original plan wrapped DASP file ops into
HTTP endpoints. Same insight as G-3: Sandstar **is** the device, no need
to proxy through SOX. Direct filesystem read/write under a sandboxed root
gives bit-for-bit identical behaviour with far less code.

**Shipped:** `crates/sandstar-server/src/rest/file_ops.rs` (~230 lines) —
`GET /file/{*path}` and `PUT /file/{*path}` with path-traversal defence
(`..`, absolute, backslash, drive-letter, NUL bytes all rejected). Files
resolve against `SANDSTAR_FILE_ROOT` (env var), or `SANDSTAR_WEB_DIR/files`
as a convenient fallback for single-env setups. Returns 503 with a clear
message when no root is configured — refuses to serve from CWD, which is a
footgun. Content-type guessed from extension (`.sab → octet-stream`,
`.sax/.xml → text/xml`, `.json → application/json`, else octet-stream).

11 unit tests cover safe_join happy paths, traversal attempts, backslash
and drive-letter rejection, NUL rejection, empty path, content-type
mapping, async 503-without-root, and a full PUT→GET round-trip through
`tempfile::TempDir`.

**Deferred to a follow-up phase (none block editor bring-up):**
- `POST /file/sox/fileOpen|fileChunk|fileClose` — SOX chunked transfer.
  HTTP bodies aren't MTU-bound the way DASP UDP frames are, so the editor's
  `axios.put(blob)` already works through the direct `PUT`.
- `POST /file/sox/fileRename` — rename file on device. Low-risk follow-up
  if the editor's rename flow surfaces.
- `POST /kit` install with platform-manifest update + reboot hook. Big
  feature with device-management implications; out of scope for the
  editor-bring-up MVP.

---

### G-5 · Library + Tags compatibility ✅
**Status:** ✅ complete (MVP) 2026-04-23

**Scope revision.** Reading `LibraryApi.ts` in the editor showed these
aren't the kit-palette calls we first thought — they're SkySpark-hosted
user-library storage (save custom DDC snippets, tag + search, ZIP export)
and a Haystack-tag autocomplete picker. Both are wrapped in the editor
with `try { ... } catch { return [] }`, so returning empty lists keeps
the UI functional without implementing either feature.

**Shipped:** `crates/sandstar-server/src/rest/library.rs` (~60 lines) —
`GET /library/` → `[]`, `GET /tags` → `[]`. Two unit tests assert the
empty shape. Mutation endpoints (`POST /library/<name>`,
`PUT /library/import`, `DELETE /library/<name>`) intentionally unmounted —
the editor's catch-all 404-swallow handles them.

When Sandstar gains a real user-library store we populate this without
changing the wire contract. `/tags` can be reshaped later to aggregate
tag keys from `DynSlotStoreHandle` if the autocomplete picker is
requested.

---

### G-6 · `convertSabToSax` native ⬜
**Status:** ⬜
**Scope (2 days):**
- `crates/sandstar-svm/src/sab_to_sax.rs` — pure-Rust SAB → SAX XML converter (Fantom pod uses Axon; we must reimplement standalone)
- `GET /api/sax/convert` endpoint + alias on the old SkySpark URL so the editor can call either
**Exit criteria:** Given the same `app.sab`, the Rust converter produces SAX XML equal to the Fantom pod output after canonical whitespace/ordering normalisation.

---

### G-7 · Integration + E2E 🟨
**Status:** 🟨 skeleton complete 2026-04-23; full E2E pending user return

**Skeleton shipped:** `crates/sandstar-server/src/rest/editor_assets.rs`
(~130 lines). When `SANDSTAR_WEB_DIR` env var is set to a directory
containing the compiled Vue bundle, `/editor/*` is served via
`tower-http`'s `ServeDir` with SPA-style fallback: bare `/editor`
serves `index.html`, any other path either hits the file directly or
falls back to `index.html` for client-side routes. When the env var is
unset (or points at a non-existent path), the router falls back to the
existing placeholder HTML so a binary-only deploy remains functional.

3 unit tests cover env-var resolution (unset, empty, non-existent
path all return None). `tower-http` gained the `fs` feature in the
workspace `Cargo.toml`.

**Operator workflow:**
```bash
cd C:\Coding\swe\sedonaWebEditor
npm run build                          # produces ./dist/
SANDSTAR_WEB_DIR=$(pwd)/dist cargo run -p sandstar-server
# http://localhost:8085/editor now serves the SPA
```

**Remaining for full G-7 (post user return):**
- End-to-end browser test: open editor, add component, verify
  `/live/sedonaState` bulk-loads and `/live` WebSocket handles a
  write-link round trip
- `sedonaWebEditor/vue.config.js` default proxy target flipped to
  `http://127.0.0.1:8085` so `npm run serve` dev mode uses the
  same backend
- SkySpark-only editor flows (`convertSabToSax`, `restartSoxServer`)
  gated behind a feature flag or routed to Sandstar alias endpoints
- `.deb` packaging: install Vue bundle to `/home/eacio/sandstar/web/`
  and set `EnvironmentFile` with `SANDSTAR_WEB_DIR` — adds ~5 MB to
  the package

**Exit criteria:** On a clean BeagleBone that has only just received
the Sandstar `.deb`, opening `http://<ip>:8085/editor` in a browser
renders the editor, authenticates via SCRAM, and lets the user
add/edit/link components against the device. No external services
involved.

---

### G-8 · Tests, docs, release ⬜
**Status:** ⬜
**Scope (2 days):**
- Protocol round-trip tests for all 19 msgTypes (`tests/live_protocol.rs` — workspace integration test, not a throwaway Python script)
- `docs/HowToUses/EDITOR_SETUP.md` — operator-facing setup guide
- Update `docs/Progress/ROADMAP_v2.md` and `docs/PROGRESS_REPORT.md`
- Update `MEMORY.md` (project memory) with completion state
- Cut v2.9.0, push to GitHub, deploy to the production BeagleBone, smoke-test
**Exit criteria:** v2.9.0 deployed; editor accessible from a browser without any SkySpark instance running.

---

## Dependencies and parallelism

G-1 is read-only research; can run alongside anything.
G-2 (live WS) and G-3 (bulk loader) touch different files and can be done in parallel by one person.
G-4 (file ops) depends on DASP file-get/put wrappers — independent of G-2/G-3.
G-5 (library/tags) is independent.
G-6 (`convertSabToSax`) depends on G-3 if it shares the SAB decoder.
G-7 is the integration layer; it starts only when G-2 + G-3 + G-4 + G-5 are done.
G-8 is the final wrap.

Critical path: **G-2 → G-7 → G-8** on one track, **G-3 → G-7** on a parallel track.

---

## Risks

1. **SAB wire format is undocumented.** ~~The Fantom pod relies on the closed-source `OfflineApp` Java class.~~ **Retired 2026-04-23.** G-3 re-scoped to serialise the live `ComponentTree` directly instead of parsing `.sab`. No binary reverse engineering in the plan at all. Net impact: G-3 dropped from 4–5 days to ~1.5 days; total phase from 21–25 down to ~18–20 days.
2. **Subtle msgType semantics.** In particular `x` (close watch), `b` (file rename), and `q` (query) have behavioural edge cases that aren't fully covered by the existing RoWS handlers. The Python round-trip tests in G-2 need to exercise these specifically.
3. **Bundle size on the `.deb`.** 5 MB of JS/CSS is small relative to the 1.8 MB current binary, but large enough to double the package. Worth confirming the BeagleBone `/home` partition has headroom. Observed at last deploy: ample.
4. **Editor depends on SkySpark-URL patterns.** `coordinator.ts:1241` extracts `projectId` from `/api/{project}/...`. After G-7 the URL will be `/editor/...` with no project slug. Need a small patch so the editor gracefully handles "no SkySpark" mode.

---

## Preflight findings

### G-0 — SAB parser audit (2026-04-23, complete)

**No full SAB parser exists in `sandstar-svm`.** The crate contains only `sab_validator.rs` (1128 lines), which parses the `.scode` header (magic `0x5ED0BA07`, version 1.5, block size, data-segment size) and scans the bytecode stream for `CallNative*` opcodes to extract kit/method references. It intentionally does **not** decode:

- the kit table that follows the header
- the component tree (nodes, parent/child pointers, type refs)
- slot values
- the link table

Public API (validator only, not wired into any REST/SOX handler):
- `validate_sab(path)` — `sandstar-svm/src/sab_validator.rs:609`
- `validate_sab_bytes(path, data)` — `:615`
- `validate_sax(path)` — `:750` (also does regex-based `.sax` XML scan)

**Available building blocks that accelerate the G-3 parser:**
- `sandstar-svm/src/image_loader.rs` — `ScodeHeader` and endian/offset helpers for binary header parsing
- `sandstar-svm/src/component_store.rs` — tree storage already scales to production sizes
- `sandstar-svm/src/native_table.rs` — kit/method metadata lookup (cross-references the parsed kit IDs)
- Existing byte-scanning loop in `sab_validator.rs:656-715` — good template for the decoder's instruction-pointer walk

**Implication for the roadmap:**
- G-3 retains its full parser sub-task (~3 days). Total G-3: 4–5 days (parser 3 days + loader 1–2 days), unchanged from the original estimate upper bound.
- G-6 (`convertSabToSax`) can reuse G-3's parser as its input stage, so G-6 does not bring additional parser work. The existing regex-based `.sax` writer in `sab_validator.rs` can seed the SAX serialiser.
- The parser target shape: ~400–600 lines of pure binary I/O (no dependencies beyond what's already in the crate).

**Risk #1 (Risks section) is now resolved from "open" to "realised, mitigated by available building blocks." No schedule slip vs. the 18–25 day band — we land at the upper end.**

---

## Appendix A — SAB format cheat sheet

*(to be filled during G-0/G-3 if the format has to be documented from scratch)*

---

## Appendix B — Fantom pod endpoint catalogue (reference)

From `C:\Coding\swe\bassgSoxWebSocket\`:
- `WS /live` — SoxWebSocketMod.fan:243-293 (19 msgTypes, JSON envelope)
- `GET /live/sedonaState` — SoxWebSocketMod.fan:51-83 (bulk loader)
- `GET/PUT /file/:uri` — SoxWebSocketMod.fan:76 / :148
- `POST /file/sox/{fileOpen,fileChunk,fileClose,fileRename}` — SoxWebSocketMod.fan:117-124
- `POST /kit` — SoxWebSocketMod.fan:133-134
- `GET /live/soxstatus` — SoxWebSocketMod.fan:65-66
- `GET /api/{proj}/ext/bassgSoxWebSocket/{status,restart,convertSabToSax}` — SoxWebSocketExt.fan:64-183 (SkySpark-coupled, bypassed/replaced in G-6)

---

## Changelog

- **2026-04-23** — Plan drafted after two-agent endpoint inventory sweep (Sandstar side: 52 routes + UDP:1876; Fantom pod side: 13 endpoints). SkySpark at 192.168.1.8:9099 confirmed unreachable, reinforcing the case for this phase. Starting with G-0 SAB parser audit.
- **2026-04-23** — G-0 complete. No existing SAB parser in `sandstar-svm`; only header + opcode validator. Risk #1 downgraded from "open" to "realised, mitigated." Total estimate narrowed to 21–25 days (upper end of original band). G-3 parser sub-task retained with ~400–600 line budget and `image_loader.rs` / `ComponentStore` / `native_table.rs` / `sab_validator.rs` as building blocks. Next: G-1 compatibility spike to determine whether `/api/rows` can serve as the `/live` backing implementation.
- **2026-04-23** — G-1 complete. Full wire-format comparison in `NOTES_EDITOR_ROWS_SPIKE.md`. 19 Fantom msgTypes vs 19 RoWS ops: 8 clean, 4 adaptable, 7 gap (3 are G-4 file ops). Wire-shim approach rejected; refactor-into-shared-module approach selected. G-2 restructured into 2a (refactor), 2b (4 new ops), 2c (`/live` handler) — same 3–4 day total. `/api/rows` preserved as first-class endpoint, gains 4 new ops as side benefit. Next: G-2a — extract `rows.rs::handle_client_msg` into shared `sox_ops.rs`.
- **2026-04-23** — G-2b protocol spec locked in `NOTES_FANTOM_MSGTYPE_SEMANTICS.md`. Deep dive into Fantom `SoxSession.fan` / `WatchManager.fan` produced request/response shapes and side-effect semantics for `v`, `y`, `o`, `q`, `x`. Final scope: 5 ops (`version`, `versionExt`, `reorder`, `query`, `closeWatch`). `query` v1 uses type-match; `closeWatch` v1 uses implicit-subscription; both defer fidelity upgrades to future minor releases if real callers surface. Open question: does `ManifestDb` store kit checksums? — to resolve inside G-2a. Proceeding to G-2a.
- **2026-04-23** — G-2a + G-2b landed together (single logical refactor). New module `crates/sandstar-server/src/sox/sox_ops.rs` (~750 lines) holds `SoxState`, `SoxOp` (24 variants), `SoxResponse`, `SessionState`, and `apply_op`. `rest/rows.rs` shrank from 2 087 → ~1 720 lines after handler extraction; dispatcher now `parse_rows_op(op, msg) → SoxOp → apply_op → envelope`. 7 new unit tests cover the G-2b ops. Open question "does `ManifestDb` carry checksums?" resolved: yes — `sox_handlers::DEFAULT_KITS` has been storing `KitInfo { name, checksum: u32, version }` all along. `cargo test -p sandstar-server --lib`: 1 385 passed, 0 failed. `cargo clippy`: no new warnings. Next: G-2c, the `/live` envelope handler.
- **2026-04-23** — G-2c MVP complete. New module `crates/sandstar-server/src/rest/live.rs` (~650 lines) — Fantom-dialect WebSocket handler at `/live`. All 19 msgTypes dispatched; ReadComp + event push use `encode_slot_value_raw` + base64 to produce bit-compatible `compProps.propValues` payloads; file ops cleanly reject with `!` pointing to G-4. 12 unit tests added in `rest::live::tests`. `cargo test -p sandstar-server --lib`: 1 397 passed (was 1 385; +12). Four follow-up items documented in the G-2c section (typed write decoding, AddComp configProps, split config+runtime event frames, whatMask honouring) — none block editor bring-up. Phase 14.0G-2 closed. Next: end-to-end editor validation (user-driven), then G-3 bulk loader once hotspots in the editor surface.
- **2026-04-23** — G-3 re-scoped then landed. Critical insight: the Fantom pod parses `.sab` because it's an external proxy; Sandstar **is** the engine and already has the live `ComponentTree`. No SAB parser needed. New module `crates/sandstar-server/src/rest/sedona_state.rs` (~430 lines) walks the tree + manifest + dyn tags and emits the editor's `SedonaStateResponse` shape directly. Handler at `GET /live/sedonaState` merged into the `/live` router. 6 unit tests + live `curl` check against the dev server (15 kits returned with checksums). `cargo test -p sandstar-server --lib`: 1 403 passed (was 1 397; +6). Risk #1 retired. Total phase estimate down from 21–25 days to ~18–20. Next: G-4 (file ops) or user-return editor bring-up, whichever comes first.
- **2026-04-23** — G-4 + G-5 + G-7 skeleton landed together (single editor-readiness pass). `rest/file_ops.rs` adds `GET/PUT /file/*` with path-traversal defence and `SANDSTAR_FILE_ROOT` / `SANDSTAR_WEB_DIR/files` resolution (SOX-chunked fileOpen/Chunk/Close and `/kit` install deferred — HTTP body transfer sidesteps the DASP MTU problem anyway). `rest/library.rs` stubs `GET /library/` and `GET /tags` as empty arrays — editor's try/catch swallows this gracefully, disabling library/tag UI without breaking anything else. `rest/editor_assets.rs` adds `SANDSTAR_WEB_DIR`-guarded SPA serving via `tower-http::ServeDir` (with `fs` feature newly enabled in workspace `Cargo.toml`); falls back to the existing 101 KB placeholder when unset. Router wiring in `rest/mod.rs` merges all three new routes. `cargo test -p sandstar-server --lib`: 1 419 passed (was 1 403; +16 tests across the three new modules). `cargo clippy`: no new warnings. Live `curl` sanity: `/library/` → `[]`, `/tags` → `[]`, `/file/app.sab` → 503 (root unconfigured), `/editor` → 200 placeholder. Browser-side E2E validation awaits user return.
