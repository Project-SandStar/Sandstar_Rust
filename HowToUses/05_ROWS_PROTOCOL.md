# RoWS Protocol (ROX over WebSocket)

## Overview
RoWS provides real-time bidirectional component tree operations over WebSocket. It's the WebSocket equivalent of the SOX REST API, with added COV (Change of Value) push events.

## Connect
```
ws://<host>:8085/api/rows
```

## Client Commands (JSON)

### Read Operations
```json
// Full tree
{"op": "readTree", "id": "r1"}

// Single component detail
{"op": "readComp", "id": "r2", "compId": 200}

// Component palette
{"op": "palette", "id": "r3"}
```

### Write Operations
```json
// Write slot value
{"op": "writeSlot", "id": "w1", "compId": 200, "slotIdx": 1, "value": 42.5}

// Add component
{"op": "addComp", "id": "w2", "parentId": 6, "kitId": 2, "typeId": 14, "name": "myConst"}

// Delete component
{"op": "deleteComp", "id": "w3", "compId": 200}

// Rename (max 31 chars)
{"op": "rename", "id": "w4", "compId": 200, "name": "newName"}

// Update canvas position
{"op": "updatePos", "id": "w5", "compId": 200, "x": 50, "y": 30}

// Invoke action
{"op": "invoke", "id": "w6", "compId": 200, "slotIdx": 2}

// Add wire
{"op": "addLink", "id": "w7", "fromComp": 200, "fromSlot": 1, "toComp": 201, "toSlot": 2}

// Delete wire
{"op": "deleteLink", "id": "w8", "fromComp": 200, "fromSlot": 1, "toComp": 201, "toSlot": 2}
```

### Subscriptions
```json
// Subscribe to COV events
{"op": "subscribe", "id": "s1", "compIds": [200, 201, 100]}

// Unsubscribe
{"op": "unsubscribe", "id": "s2", "compIds": [200]}
```

### Keepalive
```json
{"op": "ping", "id": "p1"}
```

## Server Responses

### Success
```json
{"op": "result", "id": "r1", "ok": true, "data": {...}}
```

### Error
```json
{"op": "error", "id": "w3", "code": "NOT_FOUND", "message": "component 999 not found"}
```

## Server Push Events (no "id")

### COV — Slot Values Changed
```json
{"op": "cov", "compId": 200, "slots": [
  {"index": 1, "name": "in1", "value": 72.5},
  {"index": 3, "name": "out", "value": 75.5}
]}
```

### Tree Changed
```json
{"op": "treeChanged", "action": "add", "compId": 250, "parentId": 6, "name": "new1", "typeName": "control::Add2"}
{"op": "treeChanged", "action": "delete", "compId": 250}
{"op": "treeChanged", "action": "rename", "compId": 250, "name": "renamedComp"}
```

### Link Changed
```json
{"op": "linkChanged", "action": "add", "fromComp": 200, "fromSlot": 1, "toComp": 201, "toSlot": 2}
{"op": "linkChanged", "action": "delete", "fromComp": 200, "fromSlot": 1, "toComp": 201, "toSlot": 2}
```

## Connection Limits
- Max 16 concurrent RoWS connections
- 120 second inactivity timeout
- COV polling interval: 1 second

## Haystack WebSocket (Channel Values)

Separate endpoint for channel-level value subscriptions:
```
ws://<host>:8085/api/ws
```

```json
// Subscribe to channels
{"op": "subscribe", "ids": [1713, 1100, 360], "pollInterval": 1000}

// Server pushes updates
{"op": "update", "watchId": "w-1", "ts": "2026-04-06T12:00:00Z",
 "rows": [{"channel": 1713, "status": "ok", "cur": 121.5}]}
```
