# SOX/DASP Protocol (Sedona Application Editor)

## Overview
SOX (Sedona Object eXchange) over DASP (Datagram Authenticated Session Protocol) enables the Java-based Sedona Application Editor to connect to Sandstar for visual DDC programming.

## Enable SOX
```bash
cargo run -p sandstar-server -- --sox
# or with custom port:
cargo run -p sandstar-server -- --sox --sox-port 1876
```

## Connect Sedona Editor
1. Open the Sedona Application Editor (SAE.exe)
2. Connect to: `<device-ip>:1876` (UDP)
3. The editor discovers the component tree automatically

## Supported Commands (20/20)

| Command | Code | Description |
|---------|------|-------------|
| readSchema | `v` | Kit names and checksums |
| readVersion | `y` | Platform ID and kit versions |
| readComp | `c` | Tree structure, config, runtime, links |
| readProp | `r` | Single property value |
| write | `w` | Write slot value |
| invoke | `i` | Invoke action slot |
| subscribe | `s` | Subscribe to COV events |
| unsubscribe | `u` | Unsubscribe from COV |
| add | `a` | Add component |
| delete | `d` | Delete component |
| rename | `n` | Rename component |
| link | `l` | Add/delete link |
| reorder | `o` | Reorder children |
| fileOpen | `f` | Open file for read/write |
| fileRead | `g` | Read file chunk |
| fileWrite | `h` | Write file chunk |
| fileClose | `z` | Close file transfer |
| fileRename | `b` | Rename file |
| event | `e` | COV push event (server->client) |

## Wire Format
- All integers: big-endian
- Strings: null-terminated (NOT length-prefixed)
- Sedona Str encoding: `u2(size_including_null) + chars + 0x00`
- DASP ACK piggybacking on response datagrams

## Component Tree Structure
```
App (compId=0)
+-- service (1)
|   +-- sox (2)
|   +-- users (3)
|   +-- plat (4)
+-- io (5)
|   +-- ch_1113 (100)
|   +-- ch_1713 (101)
|   +-- ...
+-- control (6)
    +-- (user-added components)
```

## Debugging SOX
```bash
# Capture Sedona editor output for debugging
SAE.exe > log.log 2>&1
```
