# REST API Reference

Base URL: `http://<host>:8085`

## Public Endpoints (No Auth)

### Pages
```bash
# Dashboard
curl http://localhost:8085/

# Visual DDC Editor
curl http://localhost:8085/editor

# Health check
curl http://localhost:8085/health
```

### Haystack Operations
```bash
# Server info
curl http://localhost:8085/api/about

# Available operations
curl http://localhost:8085/api/ops

# Supported formats
curl http://localhost:8085/api/formats

# Read channels (with optional filter)
curl "http://localhost:8085/api/read?filter=point&limit=100"
curl "http://localhost:8085/api/read?id=1113"

# Engine status
curl http://localhost:8085/api/status

# List all channels
curl http://localhost:8085/api/channels

# List polled channels
curl http://localhost:8085/api/polls

# List lookup tables
curl http://localhost:8085/api/tables

# Channel history
curl "http://localhost:8085/api/history/1113?duration=1h&limit=1000"

# Metrics
curl http://localhost:8085/api/metrics

# Diagnostics
curl http://localhost:8085/api/diagnostics
```

### SOX Component Tree (Editor API)
```bash
# Full component tree
curl http://localhost:8085/api/sox/tree

# Single component with slots and links
curl http://localhost:8085/api/sox/comp/200

# Available component types for palette
curl http://localhost:8085/api/sox/palette
```

### Dynamic Tags
```bash
# List all tagged components
curl http://localhost:8085/api/tags

# Get tags for a component
curl http://localhost:8085/api/tags/200
```

### Cluster Status
```bash
curl http://localhost:8085/api/cluster/status
```

## Protected Endpoints (Auth Required)

### Write Operations
```bash
# Write value to channel at priority level
curl -X POST http://localhost:8085/api/pointWrite \
  -H 'Content-Type: application/json' \
  -d '{"channel": 360, "value": 1.0, "level": 17}'

# Write with level and duration
curl -X POST http://localhost:8085/api/pointWrite \
  -H 'Content-Type: application/json' \
  -d '{"channel": 360, "value": 90.0, "level": 8, "who": "operator", "duration": 3600}'

# Relinquish a priority level (set value to null)
curl -X POST http://localhost:8085/api/pointWrite \
  -H 'Content-Type: application/json' \
  -d '{"channel": 360, "value": null, "level": 8}'

# Read priority array for a channel
curl "http://localhost:8085/api/pointWrite?channel=360"
```

### SOX Component CRUD
```bash
# Add component (under control folder, parentId=6)
curl -X POST http://localhost:8085/api/sox/comp \
  -H 'Content-Type: application/json' \
  -d '{"parentId": 6, "kitId": 2, "typeId": 14, "name": "myConst"}'

# Delete component
curl -X DELETE http://localhost:8085/api/sox/comp/200

# Rename component (max 31 chars, Sedona compatible)
curl -X PUT http://localhost:8085/api/sox/comp/200/name \
  -H 'Content-Type: application/json' \
  -d '{"name": "newName"}'

# Write slot value
curl -X PUT http://localhost:8085/api/sox/comp/200/slot/1 \
  -H 'Content-Type: application/json' \
  -d '{"value": 42.5}'

# Invoke action slot
curl -X POST http://localhost:8085/api/sox/comp/200/invoke/2

# Add link (wire)
curl -X POST http://localhost:8085/api/sox/link \
  -H 'Content-Type: application/json' \
  -d '{"fromComp": 200, "fromSlot": 1, "toComp": 201, "toSlot": 2}'

# Delete link
curl -X DELETE http://localhost:8085/api/sox/link \
  -H 'Content-Type: application/json' \
  -d '{"fromComp": 200, "fromSlot": 1, "toComp": 201, "toSlot": 2}'

# Update canvas position
curl -X PUT http://localhost:8085/api/sox/comp/200/pos \
  -H 'Content-Type: application/json' \
  -d '{"x": 50, "y": 30}'
```

### Dynamic Tags CRUD
```bash
# Set/merge tags
curl -X PUT http://localhost:8085/api/tags/200 \
  -H 'Content-Type: application/json' \
  -d '{"modbusAddr": 40001, "bacnetObj": "AI:1"}'

# Delete a tag
curl -X DELETE http://localhost:8085/api/tags/200/modbusAddr
```

### Engine Control
```bash
# Trigger immediate poll
curl -X POST http://localhost:8085/api/pollNow

# Reload configuration
curl -X POST http://localhost:8085/api/reload
```

### Cluster Query
```bash
# Distributed query across cluster
curl -X POST http://localhost:8085/api/cluster/query \
  -H 'Content-Type: application/json' \
  -d '{"filter": "point and channel > 1000", "limit": 50}'
```

## WebSocket Endpoints

### Haystack Watch (channel values)
```
ws://localhost:8085/api/ws
```
See [05_ROWS_PROTOCOL.md](05_ROWS_PROTOCOL.md) for details.

### RoWS (component tree operations)
```
ws://localhost:8085/api/rows
```
See [05_ROWS_PROTOCOL.md](05_ROWS_PROTOCOL.md) for details.

### roxWarp (cluster gossip)
```
ws://localhost:7443/roxwarp
ws://localhost:7443/roxwarp?debug=trio
```
See [06_ROXWARP_CLUSTER.md](06_ROXWARP_CLUSTER.md) for details.
