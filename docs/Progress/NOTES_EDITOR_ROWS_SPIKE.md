# G-1 Spike Findings: Editor dialect ↔ `/api/rows` compatibility

> **Status (2026-11-18): Completed spike.** Its recommendation was implemented in G-2a/2b/2c (`369d84f`, `f260189`); see [`IMPLEMENTATION_PLAN_NATIVE_EDITOR_BACKEND.md`](IMPLEMENTATION_PLAN_NATIVE_EDITOR_BACKEND.md).
> - **Done differently:** 5 new ops shipped, not 4 (`versionExt` added); dispatch lives in `sox_ops::apply_op`. `/live` has its own event loop. `w` needed a dedicated binary decoder; the semantics were not identical.
> - **Open:** `r` (read property) has no `/live` handler; `whatMask` is ignored.

**Date:** 2026-04-23
**Author:** Phase 14.0G G-1
**Purpose:** Determine whether the existing `/api/rows` (RoWS) WebSocket endpoint can be re-used to serve the Vue web editor, eliminating some of G-2's implementation cost. Go/no-go for "alias `/live` → `/api/rows`" vs. "ship independent native `/live`."

---

## TL;DR

**Recommendation:** **Do not alias `/live` onto `/api/rows` at the wire layer.** Implement G-2's native `/live` handler as planned, but extract the command-dispatch logic into a shared module so both endpoints share business logic. This costs no more than a direct port of RoWS and avoids shipping a lossy JSON-envelope shim that would need constant maintenance as the editor evolves.

The good news: the two endpoints differ only in **JSON envelope**, not **semantics**. 10 of 19 editor msgTypes map 1:1 to existing RoWS ops. The underlying state (`SharedComponentTree`, `ManifestDb`, `DynSlotStore`) is already shared, so G-2 reuses 100% of RoWS business logic — just wrapped in the editor's expected envelope.

---

## Wire-format comparison

### Fantom pod → Editor dialect (`/live`)

```json
// Request
{"msgType": "c", "compId": 1, "what": "t", "replyNum": 0}
// Response
{"msgType": "C", "compId": 1, "what": "t", "compTree": {...}, "replyNum": 0}
// Error
{"msgType": "!", "cause": "...", "replyNum": 0}
// Event (server push)
{"msgType": "e", "what": "c", "compId": 1, "compProps": {...}}
```

Single-character msgType codes, numeric `replyNum` (0-255 ring) for correlation, event frames share the same channel.

### Sandstar native RoWS dialect (`/api/rows`)

```json
// Request
{"op": "readComp", "compId": 1, "id": "c1"}
// Response
{"op": "readComp", "id": "c1", "result": {...}}
// Error
{"op": "error", "id": "c1", "code": "NOT_FOUND", "message": "..."}
// Event (server push)
{"op": "cov", "compId": 1, "slots": [{"index": 0, "name": "out", "value": 72.5}]}
{"op": "treeChanged", "action": "childrenChanged", "compId": 1, "children": [2,3,4]}
{"op": "tagsChanged", "compId": 1, "tags": {...}}
```

Word-style `op` names, string `id` for correlation (not numeric ring), three distinct event ops instead of one multiplexed `e`.

---

## Op-to-op mapping (19 Fantom msgTypes × 19 RoWS ops)

| Fantom msg | Meaning                         | RoWS equivalent                    | Fit  | Notes |
|:----------:|---------------------------------|------------------------------------|:----:|-------|
| `v`        | Version / kit list              | *(none)*                           | ❌   | RoWS has no version handshake. Editor expects `{kits: [{name, checksum}]}`. Must be added. |
| `c`        | Read component (t/c/r/l)        | `readComp`                         | 🟡   | RoWS returns full component in one object; Fantom returns tree/config/runtime/links separately per `what`. Shim: slice RoWS response by `what` param. |
| `r`        | Read property                   | subset of `readComp`               | 🟡   | RoWS has no single-slot read; client must ignore unused slots. |
| `w`        | Write slot                      | `writeSlot`                        | ✅   | Identical semantics. |
| `a`        | Add component                   | `addComp`                          | ✅   | Same four params (parent, kit, type, name). |
| `d`        | Delete component                | `deleteComp`                       | ✅   | — |
| `s`        | Subscribe                       | `subscribe`                        | 🟡   | Fantom has `whatMask` (tree/config/runtime/links bits); RoWS always pushes all changes. Shim: mask on client before render. |
| `u`        | Unsubscribe                     | `unsubscribe`                      | ✅   | — |
| `l`        | Link add/delete (flag-based)    | `addLink` / `deleteLink`           | ✅   | Split into two ops; shim branches on Fantom flag. |
| `n`        | Rename                          | `rename`                           | ✅   | — |
| `o`        | Reorder children                | *(none)*                           | ❌   | Only exposed as REST (`PUT /api/sox/comp/{id}/reorder`). Must be added to RoWS / live. |
| `q`        | Query                           | *(none)*                           | ❌   | Fantom queries are component filter/search. RoWS equivalent needed. |
| `i`        | Invoke action                   | `invoke`                           | ✅   | — |
| `f`        | File open                       | *(none — separate `/file/*` path)* | ❌   | Out of RoWS scope; handled by G-4. |
| `z`        | File close                      | *(none)*                           | ❌   | G-4. |
| `b`        | File rename                     | *(none)*                           | ❌   | G-4. |
| `y`        | Version extended                | *(none)*                           | ❌   | Like `v`; adds build info. Must be added. |
| `p`        | Ping                            | `ping`                             | ✅   | Identical. |
| `x`        | Close watch (release all subs)  | `unsubscribe` (all)                | 🟡   | Semantics close but not identical — Fantom also releases server-side watch resources tied to a watchId. |

**Summary:** ✅ 8 clean · 🟡 4 adaptable · ❌ 7 gaps (of which 3 are G-4's file ops, so only 4 gaps inside the WebSocket protocol proper).

---

## Event-frame mapping

Fantom sends one event opcode `e` with a `what` discriminator and inline payload of whichever facet changed. RoWS splits into three distinct ops (`cov`, `treeChanged`, `tagsChanged`) with dedicated payload shapes. The editor's `event.worker.ts` already normalises this into its own event model, so either source is adaptable — but a shim would have to fan-in (three RoWS ops → one Fantom frame) or the native `/live` endpoint would have to fan-out (internal event stream → three frames). Fan-out is simpler because the source data arrives pre-classified in RoWS' `build_cov_messages_with_tags`.

---

## Why not an envelope shim?

A thin translator (rewrite `{"op":"readComp",...}` ↔ `{"msgType":"c",...}` in `sox.worker.ts`) would appear to be a one-day job. Three reasons it is worse than implementing native `/live`:

1. **Four-to-seven gap msgTypes still need server work.** `v`, `y`, `o`, `q`, `x` have no RoWS op. A shim would end up shipping a patchwork of "for this msgType, do three RoWS calls and synthesise a response" workarounds. Each workaround is a corner case that drifts as the editor evolves.
2. **ReplyNum ring vs. string id.** Fantom uses a 0–255 numeric ring that wraps; RoWS uses arbitrary strings. A shim has to maintain a pending-request map that survives ring wrap bugs the real Fantom pod has already debugged. Not worth re-deriving.
3. **Editor already assumes Fantom event shape.** `WorkerCoordinator` and `event.worker.ts` route `e` frames with `what` discriminators. Translating three RoWS event ops back into one `e` frame requires knowing which facet each RoWS event corresponds to — which means reimplementing the classifier that Sandstar already has on the server side. Better to produce the `e` frame at the source.

---

## Revised G-2 shape (light refactor + native handler)

G-2 is **not** a ground-up reimplementation. Its actual content:

1. **Refactor `rows.rs::handle_client_msg`** into a pure function of the form `apply_op(state, op: SoxOp) -> SoxOpResult`, where `SoxOp` and `SoxOpResult` are enums over the 19 RoWS ops. Move it to a new module `sox_ops.rs`. ~1 day, zero behaviour change; RoWS keeps working.
2. **Add 4 missing ops** — `version`, `reorder`, `query`, `closeWatch` — to `SoxOp` + `apply_op`. ~1 day.
3. **Write `rest/live.rs`** — Axum WebSocket handler that parses Fantom envelopes, calls `apply_op`, and encodes Fantom envelopes back. Events: subscribe to the same COV source `build_cov_messages_with_tags` feeds and translate once. ~2 days.

Total G-2: 3–4 days (matches the original estimate). No extra parser work, no throwaway code. Both `/api/rows` and `/live` remain first-class.

---

## Impact on the main roadmap

- **G-1 complete.** Answer: light refactor of RoWS into shared business logic, then G-2 implements `/live` as a second envelope over the same core. `/api/rows` keeps working.
- **G-2 scope confirmed at 3–4 days, not reduced.** Shim approach rejected on maintainability grounds; refactor approach adds a small amount of work but keeps both endpoints clean.
- **4 new ops surfaced** (`version`, `reorder`, `query`, `closeWatch`) that RoWS also lacks. These are part of G-2's work but also improve RoWS coverage for any future non-editor client. Net win.
- **Event translation is simpler than expected.** The 3-way event split in RoWS is information-preserving; re-combining into `e` frames is bijective.

---

## Appendix: RoWS op inventory (for quick reference during G-2)

From `crates/sandstar-server/src/rest/rows.rs::handle_client_msg`:

`ping` · `readTree` · `readComp` · `writeSlot` · `addComp` · `deleteComp` · `rename` · `addLink` · `deleteLink` · `updatePos` · `invoke` · `nameTable` · `palette` · `subscribe` · `unsubscribe` · `readTags` · `setTags` · `deleteTag` · `setFormat`

Server-push ops: `cov` · `treeChanged` · `tagsChanged` · `error`

Shared state: `SharedComponentTree`, `Arc<ManifestDb>`, `Option<DynSlotStoreHandle>`, session-local `HashSet<u16>` (subscriptions), `HashMap<u16, Vec<JsonValue>>` (cov_cache), `HashMap<u16, Vec<u16>>` (tree_cache), `HashMap<u16, JsonValue>` (tags_cache), `bool` (compact_cov).
