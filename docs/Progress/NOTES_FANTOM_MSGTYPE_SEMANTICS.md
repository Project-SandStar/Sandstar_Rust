# Fantom Pod msgType Semantics — Reference Spec for G-2b

**Date:** 2026-04-23
**Source:** `C:\Coding\swe\bassgSoxWebSocket\fan\SoxSession.fan`, `SoxWebSocketMod.fan`, `WatchManager.fan` (Fantom source)
**Cross-reference:** `C:\Coding\swe\sedonaWebEditor\src\services\sox\SoxClient.ts`, `SoxMessages.ts`, `workers\coordinator.ts` (TypeScript client)

**Purpose:** Exact protocol specification for the 4 Fantom msgTypes that Sandstar's current RoWS does not cover. This document freezes the wire contract that G-2b implements so we have one source of truth while G-2a / G-2b / G-2c are in flight.

---

## 1. `v` · Version — `V` Version response

**Request:**
```json
{"msgType": "v", "replyNum": N}
```
No additional fields.

**Response:**
```json
{
  "msgType": "V",
  "replyNum": N,
  "kitCount": K,
  "kits": [
    {"name": "sys", "checksum": 3014671298},
    {"name": "sox", "checksum": 2491085312},
    ...
  ]
}
```
- `checksum` is u32 unsigned (not hex string).
- Read-only; no side effects.

**Editor trigger:** sent immediately on WebSocket open.
**Sandstar implementation note:** `ManifestDb` already stores kit metadata (15 kits / 185 types per Phase 8). Checksum field needs to be populated from the parsed kit manifest XML. If checksums aren't currently stored, compute deterministic ones from the kit name + version at load time so they remain stable across restarts.

---

## 2. `y` · Version extended — `Y` response

**Request:**
```json
{"msgType": "y", "replyNum": N}
```

**Response:**
```json
{
  "msgType": "Y",
  "replyNum": N,
  "platformId": "",
  "scodeFlags": 0,
  "kitVersions": [{"name": "...", "checksum": ...}, ...],
  "pairs": []
}
```

**Notes from Fantom source:** `SoxSession.fan:2443-2484` — `platformId` is empty (TODO in pod), `scodeFlags` hardcoded to 0, `pairs` is always empty. Only meaningful field beyond `v`'s output is that `kits` is renamed to `kitVersions`.

**Editor usage:** Agent inventory reports "currently unused" by the Vue client. Implement as a minimal stub that returns empty `platformId`/`scodeFlags=0`/`pairs=[]` and reuses the `v` kit list. Do not invest parser time matching undocumented extended fields until a real caller surfaces.

---

## 3. `o` · Reorder children — `O` response

**Request:**
```json
{
  "msgType": "o",
  "replyNum": N,
  "compId": 17,
  "childrenIds": [42, 11, 9]
}
```

**Response:**
```json
{"msgType": "O", "replyNum": N}
```
Empty body on success; error envelope `!` on failure.

**Side effect:** Fantom emits an `e` event with `what='o'` to subscribers of `compId`, payload `{kidIds: [42, 11, 9]}`. See `SoxSession.fan:464-465` / `parseReorderEventData()` at `:616`.

**Error cases:**
- Component not found → `"Component not found: $compId"` error envelope.
- No validation that the reorder list is complete or that every listed child actually belongs to the parent. Fantom defers this to the Java SOX client downstream. Sandstar should match: validate `compId` exists, accept any u16 array.

**Editor trigger:** user drags a child within the tree view.

**Sandstar implementation note:** REST endpoint `PUT /api/sox/comp/{id}/reorder` already exists (Phase 14.0A); ports the same tree mutation. New work in G-2b:
- Surface the op through `SoxOp::Reorder { comp_id, children }` → `SoxResponse::Reorder`
- Add a `treeChanged` COV broadcast with `action: "reordered"` **and** let the `/live` envelope translate that into Fantom's `e` / `what='o'` frame in G-2c. No need for a separate event variant in the shared core.

---

## 4. `q` · Query — `Q` response

**Request:**
```json
{
  "msgType": "q",
  "replyNum": N,
  "queryType": "s",
  "kitId": 3,
  "typeId": 12
}
```

**Response:**
```json
{
  "msgType": "Q",
  "replyNum": N,
  "compIds": [7, 41, 88]
}
```

**Key semantics (from `SoxSession.fan:2332-2400`):**
- Only `queryType='s'` is implemented (service query). Any other char → `"Unknown query type: <char>"` error.
- Find all components whose type implements the service type `(kitId, typeId)`.
- Returns **compIds only** — no inlined components. Client does a follow-up `readComp` per ID if details needed.
- Read-only; no mutation, no subscription change.

**Editor trigger:** service-interface pickers (e.g., "pick an IoService component for this slot"). Also useful for kit-type filtering in the palette.

**Sandstar implementation note (the hardest of the four):**

"Implements service X" ≠ "is of type X". Fantom's `client.queryService(serviceType)` walks the manifest for each component's type, checks the interface list, and returns components whose type declares the given `(kitId, typeId)` as an implemented service. To match this:

- Read each component's `type_id` + `kit_id` from `ComponentTree`.
- Look up that type in `ManifestDb` to get its service interface list.
- Include the component if `(kitId, typeId)` is in that list.

**Minimum viable for G-2b:** start with the narrower semantic **"type matches exactly"** — return components whose own `(kit_id, type_id)` equals the query. This covers the common palette-filter case. Document the simplification; widen to proper service-interface walk only if the editor's service picker misbehaves in testing.

**Error cases to preserve:**
- `"Type not found: kitId=$kitId typeId=$typeId"` when the manifest lookup misses.
- `"Unknown query type: $c"` for anything other than `'s'`.

---

## 5. `x` · Close watch — `X` response

**Request:**
```json
{"msgType": "x", "replyNum": N, "watchId": "abc123"}
```

**Response:**
```json
{
  "msgType": "X",
  "replyNum": N,
  "watchId": "abc123",
  "unsubscribed": 5
}
```
`unsubscribed` is the count of compIds released.

**Semantics (from `WatchManager.fan:123-150`):**
- Scoped cleanup: only the specified `watchId`'s subscriptions are removed.
- One session can hold multiple watches (keyed by watchId string).
- Does **not** close the DASP device session; other watches on the same device stay alive.

**Difference from `u` (unsubscribe):**
- `u` — remove *specific compIds* from a watch; takes `compIds: [...]`.
- `x` — close the *entire watch* atomically; takes `watchId`.

**Editor trigger:** tab/page close (navigate away from a device editor tab).

**Sandstar implementation note — scope decision:**

RoWS currently has a single implicit subscription set per session (`HashSet<u16>` on the session state). It has no watchId concept. Two options:

- **(a) Keep it implicit.** Treat `x` as "clear all subs on this session regardless of watchId". The response returns the count cleared. Simpler; works as long as the editor uses one watchId per WebSocket.
- **(b) Add watchId indirection.** `HashMap<String, HashSet<u16>>`, and `s` takes an optional `watchId` (defaults to `""`), `u` takes `{watchId, compIds}`, `x` removes a keyed watch. Closer to Fantom fidelity.

**Check before committing either way:** `coordinator.ts` tracks one `connections` map keyed by `ip`, one WebSocket per device. `SubscriptionWorker` batches by IP. There's no evidence the editor opens multiple logical watches on the same WebSocket. **Going with (a)** for G-2b; if a real caller surfaces, promote to (b) in a future minor release without breaking the wire (existing clients that don't pass watchId still work).

---

## G-2b scope locked

Given the notes above, the 4 ops needed for G-2b are:

| Op           | Request shape                                 | Response shape                                              | Complexity  |
|--------------|-----------------------------------------------|-------------------------------------------------------------|-------------|
| `version`    | `{}`                                          | `{kits: [{name, checksum}]}`                                | Trivial     |
| `versionExt` | `{}`                                          | `{platformId:"", scodeFlags:0, kitVersions:[...], pairs:[]}` | Trivial    |
| `reorder`    | `{compId, children:[u16]}`                    | `{ok:true}` + `treeChanged` event with `action:"reordered"` | Small       |
| `query`      | `{queryType:"s", kitId, typeId}`              | `{compIds:[u16]}`                                           | Small (type-match variant; service-walk later) |
| `closeWatch` | `{watchId}`                                   | `{watchId, unsubscribed:N}` — clears all subs on session   | Trivial     |

All five fit into `SoxOp` + `apply_op` cleanly. Estimated size: ~200 lines incl. unit tests. G-2c will render them into Fantom envelopes.

**Open question for later:** do we store kit checksums in `ManifestDb`? Needs a quick check before `version` can return useful values. Will resolve at the top of G-2a when the refactor lands in the same file neighbourhood.
