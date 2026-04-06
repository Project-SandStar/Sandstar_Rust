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
| `Fault(msg)` | Hardware/communication error with details |
| `Disabled` | Disabled by configuration |
| `Down` | Shut down |

When a driver enters `Fault` or `Down`, all its points inherit that status.

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

### ModbusDriver (Stub)

Placeholder for Modbus TCP/RTU. Function codes 01-06, 15-16.

```rust
use sandstar_server::drivers::modbus::ModbusDriver;

let driver = ModbusDriver::new("mb-1", "192.168.1.100", 502, 1);
```

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

## REST Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/drivers` | GET | List all drivers with status |
| `/api/drivers/{id}/status` | GET | Get specific driver status |
| `/api/drivers/{id}/learn` | GET | Discover points from driver |

Example response from `GET /api/drivers`:
```json
[
  {
    "id": "local-1",
    "driverType": "localIo",
    "status": "Ok",
    "pollMode": "Buckets"
  }
]
```

## Adding a New Driver

1. Create a new file in `crates/sandstar-server/src/drivers/` (e.g. `my_protocol.rs`)
2. Implement the `Driver` trait for your struct
3. Add `pub mod my_protocol;` to `drivers/mod.rs`
4. Register with the `DriverManager` in server startup code

Minimum implementation requires: `driver_type()`, `id()`, `status()`, `open()`, `close()`, `ping()`, `sync_cur()`, `write()`. The `learn()` and `poll_mode()` methods have defaults.

## Error Types

| Error | Meaning |
|-------|---------|
| `ConfigFault(msg)` | Bad configuration (won't recover without fix) |
| `CommFault(msg)` | Communication error (may recover on retry) |
| `NotSupported(feature)` | Feature not implemented by this driver |
