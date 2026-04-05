# roxWarp — Device-to-Device Clustering

## Overview
roxWarp is a gossip-based state synchronization protocol for multi-device Sandstar clusters. It enables real-time point value replication across a mesh of embedded IoT devices.

## Enable Clustering

### Basic (plain WebSocket, no TLS)
```bash
sandstar-engine-server --sox --cluster --node-id sandstar-a-001
```

### With Config File
```bash
sandstar-engine-server --sox --cluster --cluster-config /path/to/cluster.json
```

### With mTLS (production)
```bash
sandstar-engine-server --sox --cluster --node-id sandstar-a-001 \
  --cluster-port 7443 \
  --cluster-cert /etc/sandstar/certs/device.pem \
  --cluster-key /etc/sandstar/certs/device.key \
  --cluster-ca /etc/sandstar/certs/ca.pem
```

## CLI Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--cluster` | off | Enable roxWarp cluster mode |
| `--node-id <id>` | auto (hostname) | Unique node identifier |
| `--cluster-port <port>` | `7443` | roxWarp listener port |
| `--cluster-config <path>` | none | Path to cluster config JSON |
| `--cluster-cert <path>` | none | Device certificate (PEM) |
| `--cluster-key <path>` | none | Device private key (PEM) |
| `--cluster-ca <path>` | none | CA certificate for peer verification |

## Cluster Config File

```json
{
  "nodeId": "sandstar-a-001",
  "port": 7443,
  "heartbeatIntervalSecs": 5,
  "antiEntropyIntervalSecs": 60,
  "certPath": "/etc/sandstar/certs/device.pem",
  "keyPath": "/etc/sandstar/certs/device.key",
  "caPath": "/etc/sandstar/certs/ca.pem",
  "peers": [
    { "nodeId": "sandstar-b-002", "address": "192.168.1.4:7443", "enabled": true },
    { "nodeId": "sandstar-c-003", "address": "192.168.1.5:7443", "enabled": true }
  ]
}
```

## REST API

### Cluster Status
```bash
curl http://localhost:8085/api/cluster/status
```

Response (when enabled):
```json
{
  "nodeId": "sandstar-a-001",
  "version": 1542,
  "pointCount": 140,
  "versionVector": {"sandstar-a-001": 1542, "sandstar-b-002": 1200}
}
```

Response (when disabled):
```json
{
  "enabled": false,
  "message": "Clustering not enabled. Start with --cluster flag to enable roxWarp.",
  "requirements": {...}
}
```

### Distributed Query
```bash
# Query all nodes for matching points
curl -X POST http://localhost:8085/api/cluster/query \
  -H 'Content-Type: application/json' \
  -d '{"filter": "point", "limit": 100}'

# Filter by channel range
curl -X POST http://localhost:8085/api/cluster/query \
  -H 'Content-Type: application/json' \
  -d '{"filter": "channel > 1000 and channel < 2000"}'

# Filter by status
curl -X POST http://localhost:8085/api/cluster/query \
  -H 'Content-Type: application/json' \
  -d '{"filter": "status == fault"}'
```

Response:
```json
{
  "results": [
    {"channel": 1113, "value": 72.5, "unit": "degF", "status": "ok", "nodeId": "sandstar-a-001"},
    {"channel": 1713, "value": 121.5, "unit": "degF", "status": "ok", "nodeId": "sandstar-b-002"}
  ],
  "nodeCount": 2,
  "totalResults": 2
}
```

## roxWarp WebSocket Protocol

### Endpoint
```
ws://<host>:7443/roxwarp           # Binary (MessagePack) frames
ws://<host>:7443/roxwarp?debug=trio  # JSON text frames for debugging
```

### Message Types

| Message | Direction | Description |
|---------|-----------|-------------|
| `warp:hello` | Client->Server | Handshake with version vector |
| `warp:welcome` | Server->Client | Handshake accepted |
| `warp:delta` | Both | Incremental state changes |
| `warp:full` | Response | Full state dump |
| `warp:fullReq` | Request | Request full state |
| `warp:heartbeat` | Both | Keep-alive with load metrics |
| `warp:versions` | Both | Anti-entropy version exchange |
| `warp:deltaReq` | Request | Request delta from version |
| `warp:query` | Request | Distributed filter query |
| `warp:queryResult` | Response | Query results |
| `warp:join` | Broadcast | New node announcement |
| `warp:leave` | Broadcast | Graceful departure |
| `warp:ack` | Response | Acknowledgment |

### Delta Sync Flow
```
Node A                              Node B
  |-- warp:hello (versions) -------> |
  |<- warp:welcome (versions) ------ |
  |-- warp:delta (A's changes) ----> |
  |<- warp:delta (B's changes) ----- |
  |   === ACTIVE (gossip) ===        |
  |<-> warp:heartbeat (every 5s) <-> |
  |<-> warp:versions (every 60s) <-> |
  |<-> warp:delta (on change) <----> |
```

### Conflict Resolution
Last-writer-wins (LWW) based on timestamp. Tie-break on node_id (lexicographic).

## Generating Certificates for mTLS

```bash
# Generate CA key and cert
openssl req -x509 -newkey rsa:2048 -keyout ca.key -out ca.pem -days 3650 \
  -nodes -subj "/CN=Sandstar CA"

# Generate device key and CSR
openssl req -newkey rsa:2048 -keyout device.key -out device.csr \
  -nodes -subj "/CN=sandstar-a-001"

# Sign device cert with CA
openssl x509 -req -in device.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
  -out device.pem -days 365

# Copy to each device:
# /etc/sandstar/certs/device.pem  (device cert)
# /etc/sandstar/certs/device.key  (device private key)
# /etc/sandstar/certs/ca.pem      (CA cert — same on all devices)
```
