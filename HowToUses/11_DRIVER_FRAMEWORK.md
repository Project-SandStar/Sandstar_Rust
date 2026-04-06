# 11. Driver Framework

The Driver Framework v2 provides a unified abstraction for hardware and protocol drivers. Each driver implements the `Driver` trait and is managed by the `DriverManager`.

## Driver Trait

Every driver implements these lifecycle callbacks:

| Method | Purpose |
|--------|---------|
| `open()` | Initialize hardware or establish connection. Returns `DriverMeta`. |
| `close()` | Release resources, shut down cleanly. |
| `ping()` | Periodic health check. Returns `DriverMeta` or error. |
| `learn(path)` | Discover available points (auto-discovery). |
| `sync_cur(points)` | Read current values for a batch of points. |
| `write(writes)` | Write values to output points. |
| `poll_mode()` | How the driver wants to be polled (`Buckets` or `Manual`). |
| `driver_type()` | Type identifier string (e.g. `"localIo"`, `"modbus"`). |
| `id()` | Unique instance identifier. |
| `status()` | Current operational status. |

## Driver Lifecycle

```
Pending ──open()──> Ok ──close()──> Down
   │                 │
   │                 ├──ping()──> Ok (healthy)
   │                 └──ping()──> Fault (error)
   │
   └──open() fails──> Fault
```

## DriverStatus Cascade

Driver status cascades to child points:

| DriverStatus | Meaning |
|-------------|---------|
| `Pending` | Not yet initialized |
| `Ok` | Running normally |
| `Stale` | No recent data within timeout |
| `Fault(msg)` | Hardware/communication error with details |
| `Disabled` | Disabled by configuration |
| `Down` | Shut down |
| `Syncing` | Initial data load in progress |

When a driver enters `Fault` or `Down`, all its points inherit that status unless they have an explicit override via `PointStatus::Own(...)`.

## Available Drivers

### LocalIoDriver (Active)

Bridges BeagleBone hardware I/O (GPIO, ADC, I2C, PWM) through the existing HAL.

```rust
use sandstar_server::drivers::local_io::{LocalIoDriver, LocalIoChannel};

let mut driver = LocalIoDriver::new("local-1");
driver.configure_channels(vec![
    LocalIoChannel::new(1100, "AI1 RTD", "AI", "analog", "AIN0"),
    LocalIoChannel::new(5000, "DO1 Relay", "DO", "digital", "GPIO60"),
]);
driver.open()?;
```

Channel directions: `AI` (analog input), `DI` (digital input), `AO` (analog output), `DO` (digital output), `PWM`.

Channel types: `analog`, `digital`, `pwm`.

The `add_point(id, address, kind)` method infers direction from kind (`"Number"` -> `"AI"`, `"Bool"` -> `"DI"`).

### ModbusDriver (Active)

Modbus TCP driver with built-in frame encoder/decoder. Supports function codes 01-06 and 16 for coils and registers. No external dependencies (uses raw TCP).

```rust
use sandstar_server::drivers::modbus::{ModbusDriver, ModbusRegister, RegisterType};

let mut driver = ModbusDriver::new("mb-1", "192.168.1.100", 502, 1);

// Map point IDs to Modbus registers
driver.add_register(100, ModbusRegister::new(40001, RegisterType::HoldingRegister));
driver.add_register(200, ModbusRegister::with_scaling(40002, RegisterType::HoldingRegister, 0.1, 0.0));
driver.add_register(300, ModbusRegister::new(0, RegisterType::Coil));
driver.add_register(400, ModbusRegister::new(30001, RegisterType::InputRegister));

driver.open()?; // Attempts TCP connection
```

#### Register Types

| Type | Read FC | Write FC | Value Kind |
|------|---------|----------|------------|
| `HoldingRegister` | FC 03 | FC 06 / FC 16 | Number (u16) |
| `InputRegister` | FC 04 | Read-only | Number (u16) |
| `Coil` | FC 01 | FC 05 | Bool (on/off) |
| `DiscreteInput` | FC 02 | Read-only | Bool (on/off) |

#### Register Scaling

Each register has `scale` and `offset` fields for engineering unit conversion:

```
engineering_value = raw_value * scale + offset
raw_value = (engineering_value - offset) / scale
```

Default: `scale=1.0, offset=0.0` (pass-through).

Example: Temperature sensor that reports in tenths of a degree:
```rust
// Raw value 250 -> 250 * 0.1 + 0 = 25.0 degrees
ModbusRegister::with_scaling(40010, RegisterType::HoldingRegister, 0.1, 0.0)
```

#### Modbus TCP Frame Format

The driver implements the Modbus TCP Application Protocol (MBAP) framing:

```
Transaction ID (2B) | Protocol ID (2B, 0x0000) | Length (2B) | Unit ID (1B) | FC (1B) | Data
```

Use `ModbusFrame` directly for advanced use cases:

```rust
use sandstar_server::drivers::modbus::{ModbusFrame, fc};

// Build a read request
let frame = ModbusFrame::read_holding_registers(1, 1, 40001, 10);
let bytes = frame.encode(); // ready to send over TCP

// Parse a response
let response = ModbusFrame::decode(&received_bytes)?;
let values = response.parse_register_response()?; // Vec<u16>
```

#### Connection Behavior

- `open()` attempts a TCP connection. If the device is offline, the driver still initializes but enters `Fault` status.
- `sync_cur()` and `write()` return `CommFault` errors when not connected.
- `ping()` attempts reconnection and can transition back to `Ok`.
- The driver uses 5-second TCP connect and read timeouts.

### BacnetDriver (Stub)

Placeholder for BACnet/IP with ReadProperty, WriteProperty, COV.

```rust
use sandstar_server::drivers::bacnet::BacnetDriver;

let driver = BacnetDriver::new("bac-1");
```

### MqttDriver (Stub)

Placeholder for MQTT pub/sub. Uses `Manual` poll mode (event-driven).

```rust
use sandstar_server::drivers::mqtt::MqttDriver;

let driver = MqttDriver::new("mq-1", "mqtt://broker:1883");
```

## DriverManager

The `DriverManager` orchestrates all driver instances:

```rust
use sandstar_server::drivers::{DriverManager, local_io::LocalIoDriver};

let mut mgr = DriverManager::new();
mgr.register(Box::new(LocalIoDriver::new("local-1")))?;
mgr.open_all();        // Initialize all drivers
mgr.sync_all(&points); // Poll all drivers
mgr.close_all();       // Shut down all drivers
```

Key operations:
- `register(driver)` -- Add a driver instance (rejects duplicate IDs)
- `remove(id)` -- Remove and close a driver
- `open_all()` / `close_all()` -- Lifecycle management
- `sync_all(point_map)` -- Batch read across all drivers
- `write(driver_id, writes)` -- Write to a specific driver
- `learn(driver_id, path)` -- Discover points from a driver
- `driver_summaries()` -- Status overview for REST API

## PollMode

| Mode | Behavior |
|------|----------|
| `Buckets` | Automatic polling by the manager at configured intervals (default) |
| `Manual` | Driver controls its own timing (e.g. MQTT event-driven) |

## Point Status Inheritance

Points inherit their driver's status by default. Override with `PointStatus::Own(status)`:

```rust
mgr.register_point(100, "mb-1");  // point 100 belongs to driver mb-1
mgr.set_point_status(200, PointStatus::Own(DriverStatus::Fault("sensor fault".into())));
let effective = mgr.effective_point_status(100); // inherits from mb-1
```

## REST Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/drivers` | GET | List all drivers with status |
| `/api/drivers/{id}/status` | GET | Get driver status with point statuses |
| `/api/drivers/{id}/learn` | GET | Discover points from driver |
| `/api/drivers/{id}/write` | POST | Write values to driver points |

### GET /api/drivers

List all registered drivers:

```json
[
  {
    "id": "local-1",
    "driverType": "localIo",
    "status": "Ok",
    "pollMode": "Buckets",
    "pollBuckets": 2,
    "pollPoints": 140
  },
  {
    "id": "mb-1",
    "driverType": "modbus",
    "status": "Ok",
    "pollMode": "Buckets",
    "pollBuckets": 1,
    "pollPoints": 10
  }
]
```

### GET /api/drivers/{id}/status

```json
{
  "id": "mb-1",
  "status": "Ok",
  "points": [
    {"pointId": 100, "status": "Ok"},
    {"pointId": 200, "status": {"Fault": "sensor fault"}}
  ]
}
```

### GET /api/drivers/{id}/learn

```json
{
  "driverId": "mb-1",
  "points": [
    {"name": "hr_40001", "address": "HR:40001", "kind": "Number", "unit": null, "tags": {"writable": "true"}},
    {"name": "co_0", "address": "CO:0", "kind": "Bool", "unit": null, "tags": {"writable": "true"}}
  ]
}
```

### POST /api/drivers/{id}/write

Request body:
```json
{
  "writes": [[100, 72.5], [200, 1.0]]
}
```

Response:
```json
{
  "driverId": "mb-1",
  "results": [
    {"pointId": 100, "ok": true},
    {"pointId": 200, "ok": false, "error": "register type IR at address 30001 is read-only"}
  ]
}
```

## Error Types

| Error | Meaning |
|-------|---------|
| `ConfigFault(msg)` | Bad configuration (won't recover without fix) |
| `CommFault(msg)` | Communication error (may recover on retry) |
| `NotSupported(feature)` | Feature not implemented by this driver |
| `RemoteStatus(msg)` | Remote device reported an error |
| `Timeout(msg)` | Communication timeout |
| `Internal(msg)` | Logic bug or unexpected state |
| `HardwareNotFound(msg)` | Device not found or inaccessible |

## Adding a New Driver

1. Create a new file in `crates/sandstar-server/src/drivers/` (e.g. `my_protocol.rs`)
2. Implement the `Driver` trait for your struct
3. Add `pub mod my_protocol;` to `drivers/mod.rs`
4. Register with the `DriverManager` in server startup code

Minimum implementation requires: `driver_type()`, `id()`, `status()`, `open()`, `close()`, `ping()`, `sync_cur()`, `write()`. The `learn()` and `poll_mode()` methods have defaults.
