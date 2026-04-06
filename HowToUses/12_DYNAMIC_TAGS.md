# Dynamic Tags

Dynamic tags allow attaching runtime key-value metadata to SOX components without modifying the compiled Sedona slot model. This is the "side-car" pattern: a separate `DynSlotStore` maps component IDs to dictionaries of typed values.

## Why Dynamic Tags Exist

Sedona's slot architecture is frozen at compile time. Every property name, type, and byte offset is baked into the scode binary. There is no mechanism to add metadata at runtime.

Dynamic tags solve this by providing a separate store that can hold arbitrary key-value pairs per component. Use cases include:

- **Modbus**: register address, function code, data type, byte order
- **BACnet**: object type, object instance, COV increment
- **LoRaWAN**: devEUI, appEUI, RSSI, SNR, data rate, gateway ID
- **MQTT**: topic path, QoS, payload format, JSON path
- **Haystack**: arbitrary project tags (dis, siteRef, equipRef)

## DynValue Types

Tags are typed using the `DynValue` enum:

| Type | JSON Representation | Example |
|------|-------------------|---------|
| Null | `{"type":"Null"}` | Absent/unknown value |
| Marker | `{"type":"Marker"}` | Presence tag (e.g., `point`, `sensor`) |
| Bool | `{"type":"Bool","val":true}` | Boolean flag |
| Int | `{"type":"Int","val":40001}` | Integer (i64) |
| Float | `{"type":"Float","val":72.4}` | Floating point (f64) |
| Str | `{"type":"Str","val":"hello"}` | String value |
| Ref | `{"type":"Ref","val":"@p:demo:r:abc"}` | Haystack reference |

The PUT endpoint also accepts plain JSON values which are auto-converted:
- `"hello"` becomes `Str("hello")`
- `42` becomes `Int(42)`
- `1.5` becomes `Float(1.5)`
- `true` becomes `Bool(true)`
- `null` becomes `Null`

## REST API Endpoints

All endpoints are under `/api/tags`. Default server port is 8085.

### GET /api/tags -- List All Tagged Components

Returns a summary of all components that have dynamic tags.

```bash
curl -s http://localhost:8085/api/tags
```

Response:
```json
{
  "totalTags": 5,
  "components": [
    { "compId": 100, "tagCount": 3 },
    { "compId": 200, "tagCount": 2 }
  ]
}
```

### GET /api/tags/{comp_id} -- Get Tags for a Component

Returns all dynamic tags for the specified component.

```bash
curl -s http://localhost:8085/api/tags/100
```

Response (with tags):
```json
{
  "compId": 100,
  "tags": {
    "modbusAddr": { "type": "Int", "val": 40001 },
    "dis": { "type": "Str", "val": "Zone Temperature" },
    "point": { "type": "Marker" }
  }
}
```

Response (no tags):
```json
{
  "compId": 100,
  "tags": {}
}
```

### PUT /api/tags/{comp_id} -- Set/Merge Tags

Sets one or more tags on a component. By default, existing tags not in the request body are preserved (merge mode).

Merge mode (default):
```bash
curl -s -X PUT http://localhost:8085/api/tags/100 \
  -H 'Content-Type: application/json' \
  -d '{"modbusAddr": 40001, "dis": "Zone Temperature"}'
```

Replace mode (removes tags not in the request):
```bash
curl -s -X PUT http://localhost:8085/api/tags/100 \
  -H 'Content-Type: application/json' \
  -d '{"_replace": true, "modbusAddr": 40001}'
```

Response:
```json
{
  "ok": true,
  "compId": 100,
  "tagCount": 2
}
```

You can use either plain JSON values or the typed DynValue format:
```bash
# Plain JSON (auto-converted)
curl -s -X PUT http://localhost:8085/api/tags/100 \
  -H 'Content-Type: application/json' \
  -d '{"address": 40001, "name": "Zone Temp", "active": true}'

# Typed DynValue format
curl -s -X PUT http://localhost:8085/api/tags/100 \
  -H 'Content-Type: application/json' \
  -d '{"address": {"type":"Int","val":40001}, "point": {"type":"Marker"}}'
```

### DELETE /api/tags/{comp_id}/{key} -- Delete a Single Tag

Removes a specific tag from a component.

```bash
curl -s -X DELETE http://localhost:8085/api/tags/100/modbusAddr
```

Response:
```json
{
  "ok": true,
  "compId": 100,
  "key": "modbusAddr",
  "removed": true
}
```

If the tag did not exist:
```json
{
  "ok": true,
  "compId": 100,
  "key": "modbusAddr",
  "removed": false
}
```

## Memory Limits

The dynamic tag store enforces two limits to prevent unbounded growth on embedded devices:

| Limit | Default | Purpose |
|-------|---------|---------|
| Max tags per component | 64 | Prevents any single component from consuming too much memory |
| Max total tags | 10,000 | Global cap across all components |

Exceeding either limit returns HTTP 400:
```json
{
  "err": "component 100 has 64 tags (max 64)"
}
```

### Memory Footprint Estimates

| Deployment | Components | Tags Each | Total Memory |
|-----------|-----------|-----------|-------------|
| Small (50 LoRaWAN devices) | 50 | 15 | ~76 KB |
| Medium (200 mixed devices) | 200 | 20 | ~400 KB |
| Large (1000 devices) | 1000 | 25 | ~2.4 MB |

All well within the 512 MB available on BeagleBone.

## Persistence

Dynamic tags are automatically persisted to disk as JSON.

- **File location**: `{SANDSTAR_CONFIG_DIR}/dyn_slots.json` (default: `/home/eacio/sandstar/etc/config/dyn_slots.json`)
- **Auto-save**: Every 5 seconds when dirty (same timer as SOX component persistence)
- **Atomic writes**: Written to `.tmp` file first, then renamed (prevents corruption on power loss)
- **Load on startup**: Tags are restored automatically when the SOX server starts
- **Corrupt file handling**: If the persistence file is corrupt, the server logs a warning and starts with an empty tag store
- **Version tag**: The JSON file includes a `version` field for forward compatibility

Example persisted file:
```json
{
  "version": 1,
  "slots": {
    "100": {
      "modbusAddr": { "type": "Int", "val": 40001 },
      "dis": { "type": "Str", "val": "Zone Temperature" }
    },
    "200": {
      "devEUI": { "type": "Str", "val": "A81758FFFE0312AB" },
      "rssi": { "type": "Int", "val": -72 }
    }
  }
}
```

## Use Cases

### Modbus Register Addresses

Attach register metadata to Sandstar point components after Modbus device discovery:

```bash
curl -s -X PUT http://localhost:8085/api/tags/100 \
  -H 'Content-Type: application/json' \
  -d '{
    "modbusAddr": 40001,
    "functionCode": 3,
    "dataType": "float32",
    "byteOrder": "ABCD",
    "scaleFactor": 0.1,
    "unit": "kWh",
    "pollGroup": "fast"
  }'
```

### BACnet Object IDs

Store BACnet object identity on discovered points:

```bash
curl -s -X PUT http://localhost:8085/api/tags/200 \
  -H 'Content-Type: application/json' \
  -d '{
    "bacnetObjectType": "analog-input",
    "bacnetObjectInstance": 1,
    "bacnetObjectName": "Zone Temperature",
    "covIncrement": 0.5
  }'
```

### LoRaWAN Device Metadata

Tag components with LoRaWAN device information:

```bash
curl -s -X PUT http://localhost:8085/api/tags/300 \
  -H 'Content-Type: application/json' \
  -d '{
    "devEUI": "A81758FFFE0312AB",
    "appEUI": "70B3D57ED0041234",
    "deviceClass": "C",
    "rssi": -72,
    "snr": 8.5,
    "dataRate": "DR3"
  }'
```

## Integration with Driver Framework v2

When Driver Framework v2 (Phase 12.0) performs device discovery via `on_learn()`, the returned `LearnGrid` metadata is stored as dynamic tags on the created point components. This connects the discovery results to persistent component metadata without requiring any changes to the Sedona slot model.

Flow:
1. Driver discovers devices/points via `on_learn()`
2. User selects points to add from the learn grid
3. Sandstar creates SOX components for selected points
4. Discovery metadata is stored as dynamic tags via `DynSlotStore`
5. Tags persist across restarts and are accessible via REST API

## Component Cleanup

When a SOX component is deleted (via `SoxDelete` command), its dynamic tags are automatically cleaned up. This prevents orphaned tag entries from accumulating.
