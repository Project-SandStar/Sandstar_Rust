//! Modbus TCP driver with minimal frame encoder/decoder.
//!
//! Implements Modbus TCP Application Protocol (MBAP) framing and
//! function codes 01-04, 05-06, 16 for coils and registers.
//! No external `tokio-modbus` dependency — uses raw TCP via `tokio::net`.

use std::collections::HashMap;

use async_trait::async_trait;

use super::async_driver::AsyncDriver;
use super::{DriverError, DriverMeta, DriverPointRef, DriverStatus, LearnGrid, LearnPoint, PollMode};

// ── Modbus TCP Frame ───────────────────────────────────────

/// Modbus TCP MBAP frame builder/parser.
///
/// Wire format:
/// ```text
/// Transaction ID (2B) | Protocol ID (2B, always 0) | Length (2B) | Unit ID (1B) | FC (1B) | Data
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ModbusFrame {
    pub transaction_id: u16,
    pub unit_id: u8,
    pub function_code: u8,
    pub data: Vec<u8>,
}

/// Modbus function codes.
pub mod fc {
    pub const READ_COILS: u8 = 0x01;
    pub const READ_DISCRETE_INPUTS: u8 = 0x02;
    pub const READ_HOLDING_REGISTERS: u8 = 0x03;
    pub const READ_INPUT_REGISTERS: u8 = 0x04;
    pub const WRITE_SINGLE_COIL: u8 = 0x05;
    pub const WRITE_SINGLE_REGISTER: u8 = 0x06;
    pub const WRITE_MULTIPLE_REGISTERS: u8 = 0x10; // FC 16
}

impl ModbusFrame {
    /// Encode the frame into Modbus TCP wire format (MBAP header + PDU).
    pub fn encode(&self) -> Vec<u8> {
        let length = 2 + self.data.len(); // unit_id(1) + fc(1) + data
        let mut frame = Vec::with_capacity(7 + self.data.len());
        frame.extend_from_slice(&self.transaction_id.to_be_bytes());
        frame.extend_from_slice(&0u16.to_be_bytes()); // protocol ID always 0
        frame.extend_from_slice(&(length as u16).to_be_bytes());
        frame.push(self.unit_id);
        frame.push(self.function_code);
        frame.extend_from_slice(&self.data);
        frame
    }

    /// Decode a Modbus TCP frame from raw bytes.
    ///
    /// Requires at least 9 bytes (7-byte MBAP header + 1 unit + 1 FC).
    /// Validates protocol ID is 0 and length field matches actual data.
    pub fn decode(bytes: &[u8]) -> Result<Self, DriverError> {
        if bytes.len() < 9 {
            return Err(DriverError::CommFault(format!(
                "modbus frame too short: {} bytes (need at least 9)",
                bytes.len()
            )));
        }
        let protocol_id = u16::from_be_bytes([bytes[2], bytes[3]]);
        if protocol_id != 0 {
            return Err(DriverError::CommFault(format!(
                "invalid modbus protocol ID: {protocol_id} (expected 0)"
            )));
        }
        let length = u16::from_be_bytes([bytes[4], bytes[5]]) as usize;
        if bytes.len() < 6 + length {
            return Err(DriverError::CommFault(format!(
                "modbus frame truncated: have {} bytes after header, need {length}",
                bytes.len() - 6
            )));
        }
        let function_code = bytes[7];
        // Check for exception response (FC has bit 7 set)
        if function_code & 0x80 != 0 {
            let exception_code = if bytes.len() > 8 { bytes[8] } else { 0 };
            return Err(DriverError::RemoteStatus(format!(
                "modbus exception: FC 0x{:02X}, code {}",
                function_code & 0x7F,
                exception_code
            )));
        }
        Ok(Self {
            transaction_id: u16::from_be_bytes([bytes[0], bytes[1]]),
            unit_id: bytes[6],
            function_code,
            data: bytes[8..6 + length].to_vec(),
        })
    }

    /// Build a Read Coils request (FC 01).
    pub fn read_coils(txn: u16, unit: u8, start: u16, count: u16) -> Self {
        let mut data = Vec::with_capacity(4);
        data.extend_from_slice(&start.to_be_bytes());
        data.extend_from_slice(&count.to_be_bytes());
        Self { transaction_id: txn, unit_id: unit, function_code: fc::READ_COILS, data }
    }

    /// Build a Read Discrete Inputs request (FC 02).
    pub fn read_discrete_inputs(txn: u16, unit: u8, start: u16, count: u16) -> Self {
        let mut data = Vec::with_capacity(4);
        data.extend_from_slice(&start.to_be_bytes());
        data.extend_from_slice(&count.to_be_bytes());
        Self { transaction_id: txn, unit_id: unit, function_code: fc::READ_DISCRETE_INPUTS, data }
    }

    /// Build a Read Holding Registers request (FC 03).
    pub fn read_holding_registers(txn: u16, unit: u8, start: u16, count: u16) -> Self {
        let mut data = Vec::with_capacity(4);
        data.extend_from_slice(&start.to_be_bytes());
        data.extend_from_slice(&count.to_be_bytes());
        Self { transaction_id: txn, unit_id: unit, function_code: fc::READ_HOLDING_REGISTERS, data }
    }

    /// Build a Read Input Registers request (FC 04).
    pub fn read_input_registers(txn: u16, unit: u8, start: u16, count: u16) -> Self {
        let mut data = Vec::with_capacity(4);
        data.extend_from_slice(&start.to_be_bytes());
        data.extend_from_slice(&count.to_be_bytes());
        Self { transaction_id: txn, unit_id: unit, function_code: fc::READ_INPUT_REGISTERS, data }
    }

    /// Build a Write Single Coil request (FC 05).
    ///
    /// Value: `true` = 0xFF00, `false` = 0x0000 per Modbus spec.
    pub fn write_single_coil(txn: u16, unit: u8, address: u16, value: bool) -> Self {
        let mut data = Vec::with_capacity(4);
        data.extend_from_slice(&address.to_be_bytes());
        let coil_val: u16 = if value { 0xFF00 } else { 0x0000 };
        data.extend_from_slice(&coil_val.to_be_bytes());
        Self { transaction_id: txn, unit_id: unit, function_code: fc::WRITE_SINGLE_COIL, data }
    }

    /// Build a Write Single Register request (FC 06).
    pub fn write_single_register(txn: u16, unit: u8, address: u16, value: u16) -> Self {
        let mut data = Vec::with_capacity(4);
        data.extend_from_slice(&address.to_be_bytes());
        data.extend_from_slice(&value.to_be_bytes());
        Self { transaction_id: txn, unit_id: unit, function_code: fc::WRITE_SINGLE_REGISTER, data }
    }

    /// Build a Write Multiple Registers request (FC 16).
    pub fn write_multiple_registers(txn: u16, unit: u8, start: u16, values: &[u16]) -> Self {
        let count = values.len() as u16;
        let byte_count = (values.len() * 2) as u8;
        let mut data = Vec::with_capacity(5 + values.len() * 2);
        data.extend_from_slice(&start.to_be_bytes());
        data.extend_from_slice(&count.to_be_bytes());
        data.push(byte_count);
        for v in values {
            data.extend_from_slice(&v.to_be_bytes());
        }
        Self { transaction_id: txn, unit_id: unit, function_code: fc::WRITE_MULTIPLE_REGISTERS, data }
    }

    /// Parse register values from a read response (FC 03/04).
    ///
    /// Response data format: `byte_count(1) + register_values(N*2)`.
    pub fn parse_register_response(&self) -> Result<Vec<u16>, DriverError> {
        if self.data.is_empty() {
            return Err(DriverError::CommFault("empty register response".into()));
        }
        let byte_count = self.data[0] as usize;
        if self.data.len() < 1 + byte_count {
            return Err(DriverError::CommFault(format!(
                "register response truncated: expected {} data bytes, got {}",
                byte_count,
                self.data.len() - 1
            )));
        }
        let mut registers = Vec::with_capacity(byte_count / 2);
        for i in (1..1 + byte_count).step_by(2) {
            if i + 1 < self.data.len() {
                registers.push(u16::from_be_bytes([self.data[i], self.data[i + 1]]));
            }
        }
        Ok(registers)
    }

    /// Parse coil/discrete input values from a read response (FC 01/02).
    ///
    /// Response data format: `byte_count(1) + coil_bytes(N)`.
    /// Returns individual bit values as booleans.
    pub fn parse_coil_response(&self, count: u16) -> Result<Vec<bool>, DriverError> {
        if self.data.is_empty() {
            return Err(DriverError::CommFault("empty coil response".into()));
        }
        let byte_count = self.data[0] as usize;
        if self.data.len() < 1 + byte_count {
            return Err(DriverError::CommFault("coil response truncated".into()));
        }
        let mut coils = Vec::with_capacity(count as usize);
        for bit_idx in 0..count as usize {
            let byte_idx = bit_idx / 8;
            let bit_pos = bit_idx % 8;
            if 1 + byte_idx < self.data.len() {
                coils.push((self.data[1 + byte_idx] >> bit_pos) & 1 != 0);
            }
        }
        Ok(coils)
    }
}

// ── Register Type ──────────────────────────────────────────

/// Modbus register type determines which function code to use for read/write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegisterType {
    /// Holding Register — FC 03 read, FC 06/16 write.
    HoldingRegister,
    /// Input Register — FC 04 read, read-only.
    InputRegister,
    /// Coil — FC 01 read, FC 05 write.
    Coil,
    /// Discrete Input — FC 02 read, read-only.
    DiscreteInput,
}

impl RegisterType {
    /// Function code for reading this register type.
    pub fn read_fc(&self) -> u8 {
        match self {
            Self::Coil => fc::READ_COILS,
            Self::DiscreteInput => fc::READ_DISCRETE_INPUTS,
            Self::HoldingRegister => fc::READ_HOLDING_REGISTERS,
            Self::InputRegister => fc::READ_INPUT_REGISTERS,
        }
    }

    /// Whether this register type is writable.
    pub fn is_writable(&self) -> bool {
        matches!(self, Self::HoldingRegister | Self::Coil)
    }

    /// Short label for display / learn grid.
    pub fn label(&self) -> &'static str {
        match self {
            Self::HoldingRegister => "HR",
            Self::InputRegister => "IR",
            Self::Coil => "CO",
            Self::DiscreteInput => "DI",
        }
    }
}

impl std::fmt::Display for RegisterType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

// ── ModbusRegister ─────────────────────────────────────────

/// A single Modbus register mapping for a point.
#[derive(Debug, Clone)]
pub struct ModbusRegister {
    /// Register address (0-based).
    pub address: u16,
    /// Register type (determines FC used for read/write).
    pub register_type: RegisterType,
    /// Scale factor: `engineering_value = raw * scale + offset`.
    pub scale: f64,
    /// Offset: `engineering_value = raw * scale + offset`.
    pub offset: f64,
}

impl ModbusRegister {
    /// Create a new register mapping with default scale (1.0) and offset (0.0).
    pub fn new(address: u16, register_type: RegisterType) -> Self {
        Self {
            address,
            register_type,
            scale: 1.0,
            offset: 0.0,
        }
    }

    /// Create with explicit scale and offset.
    pub fn with_scaling(address: u16, register_type: RegisterType, scale: f64, offset: f64) -> Self {
        Self { address, register_type, scale, offset }
    }

    /// Convert raw register value to engineering value.
    pub fn raw_to_eng(&self, raw: f64) -> f64 {
        raw * self.scale + self.offset
    }

    /// Convert engineering value to raw register value.
    pub fn eng_to_raw(&self, eng: f64) -> f64 {
        if self.scale == 0.0 {
            0.0
        } else {
            (eng - self.offset) / self.scale
        }
    }
}

// ── ModbusDriver ───────────────────────────────────────────

/// Modbus TCP driver with register mapping and frame-level I/O.
///
/// Maintains a TCP connection to a Modbus slave device and maps
/// point IDs to register addresses. Supports coils (FC 01/05),
/// discrete inputs (FC 02), holding registers (FC 03/06/16),
/// and input registers (FC 04).
///
/// ## Connection Lifecycle
///
/// ```text
/// new() → Pending
/// open() → connects TCP → Ok
/// sync_cur() → sends read frames over TCP → values
/// write() → sends write frames over TCP
/// close() → drops TCP connection → Down
/// ```
///
/// When TCP connection is lost, `sync_cur` and `write` return
/// `CommFault` errors and the driver status transitions to `Fault`.
pub struct ModbusDriver {
    id: String,
    status: DriverStatus,
    /// Target host address (e.g. "192.168.1.100").
    host: String,
    /// TCP port (default 502).
    port: u16,
    /// Modbus slave/unit ID (1-247).
    slave_id: u8,
    /// Modbus register mappings: point_id -> register config.
    register_map: HashMap<u32, ModbusRegister>,
    /// Whether we have an active TCP connection.
    ///
    /// In production, this would hold a `tokio::net::TcpStream`.
    /// For the sync Driver trait, we track connection state as a bool
    /// and use std::net::TcpStream for blocking I/O.
    connected: bool,
    /// Monotonically increasing transaction ID for MBAP framing.
    transaction_counter: u16,
}

impl ModbusDriver {
    /// Create a new Modbus TCP driver.
    pub fn new(id: impl Into<String>, host: impl Into<String>, port: u16, slave_id: u8) -> Self {
        Self {
            id: id.into(),
            status: DriverStatus::Pending,
            host: host.into(),
            port,
            slave_id,
            register_map: HashMap::new(),
            connected: false,
            transaction_counter: 0,
        }
    }

    /// Add a register mapping for a point.
    pub fn add_register(&mut self, point_id: u32, register: ModbusRegister) {
        self.register_map.insert(point_id, register);
    }

    /// Get the number of mapped registers.
    pub fn register_count(&self) -> usize {
        self.register_map.len()
    }

    /// Get a register mapping by point ID.
    pub fn get_register(&self, point_id: u32) -> Option<&ModbusRegister> {
        self.register_map.get(&point_id)
    }

    /// Get the configured host address.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Get the configured TCP port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Get the configured slave ID.
    pub fn slave_id(&self) -> u8 {
        self.slave_id
    }

    /// Whether there is an active TCP connection.
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Allocate the next transaction ID (wraps at u16::MAX).
    fn next_txn(&mut self) -> u16 {
        let txn = self.transaction_counter;
        self.transaction_counter = self.transaction_counter.wrapping_add(1);
        txn
    }

    /// Build a read request frame for the given register.
    fn build_read_frame(&mut self, reg: &ModbusRegister) -> ModbusFrame {
        let txn = self.next_txn();
        match reg.register_type {
            RegisterType::Coil => ModbusFrame::read_coils(txn, self.slave_id, reg.address, 1),
            RegisterType::DiscreteInput => ModbusFrame::read_discrete_inputs(txn, self.slave_id, reg.address, 1),
            RegisterType::HoldingRegister => ModbusFrame::read_holding_registers(txn, self.slave_id, reg.address, 1),
            RegisterType::InputRegister => ModbusFrame::read_input_registers(txn, self.slave_id, reg.address, 1),
        }
    }

    /// Build a write request frame for the given register and engineering value.
    fn build_write_frame(&mut self, reg: &ModbusRegister, value: f64) -> Result<ModbusFrame, DriverError> {
        let txn = self.next_txn();
        match reg.register_type {
            RegisterType::Coil => {
                Ok(ModbusFrame::write_single_coil(txn, self.slave_id, reg.address, value != 0.0))
            }
            RegisterType::HoldingRegister => {
                let raw = reg.eng_to_raw(value);
                let raw_u16 = raw.round().clamp(0.0, u16::MAX as f64) as u16;
                Ok(ModbusFrame::write_single_register(txn, self.slave_id, reg.address, raw_u16))
            }
            RegisterType::InputRegister | RegisterType::DiscreteInput => {
                Err(DriverError::ConfigFault(format!(
                    "register type {} at address {} is read-only",
                    reg.register_type, reg.address
                )))
            }
        }
    }

    /// Attempt to establish a TCP connection to the Modbus slave.
    ///
    /// Uses std::net::TcpStream with a 5-second connect timeout.
    fn try_connect(&mut self) -> Result<(), DriverError> {
        use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
        use std::time::Duration;

        let addr_str = format!("{}:{}", self.host, self.port);
        let addr: SocketAddr = addr_str
            .to_socket_addrs()
            .map_err(|e| DriverError::ConfigFault(format!("invalid address '{addr_str}': {e}")))?
            .next()
            .ok_or_else(|| DriverError::ConfigFault(format!("no addresses for '{addr_str}'")))?;

        match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
            Ok(_stream) => {
                // In production, we would store the stream for reuse.
                // For the sync Driver trait, we confirm connectivity.
                self.connected = true;
                self.status = DriverStatus::Ok;
                Ok(())
            }
            Err(e) => {
                self.connected = false;
                self.status = DriverStatus::Fault(format!("connect failed: {e}"));
                Err(DriverError::CommFault(format!(
                    "failed to connect to {addr_str}: {e}"
                )))
            }
        }
    }

    /// Send a frame and receive the response over a fresh TCP connection.
    ///
    /// Opens a new connection per request (simple but not optimal).
    /// A production implementation would reuse a persistent connection.
    fn transact(&mut self, frame: &ModbusFrame) -> Result<ModbusFrame, DriverError> {
        use std::io::{Read, Write};
        use std::net::{TcpStream, ToSocketAddrs};
        use std::time::Duration;

        let addr_str = format!("{}:{}", self.host, self.port);
        let addr = addr_str
            .to_socket_addrs()
            .map_err(|e| DriverError::ConfigFault(format!("invalid address '{addr_str}': {e}")))?
            .next()
            .ok_or_else(|| DriverError::ConfigFault(format!("no addresses for '{addr_str}'")))?;

        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .map_err(|e| {
                self.connected = false;
                self.status = DriverStatus::Fault(format!("connect failed: {e}"));
                DriverError::CommFault(format!("connect to {addr_str}: {e}"))
            })?;

        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| DriverError::Internal(format!("set read timeout: {e}")))?;

        let request_bytes = frame.encode();
        stream.write_all(&request_bytes).map_err(|e| {
            self.connected = false;
            self.status = DriverStatus::Fault(format!("write failed: {e}"));
            DriverError::CommFault(format!("send to {addr_str}: {e}"))
        })?;

        let mut buf = [0u8; 260]; // max Modbus TCP frame is 260 bytes
        let n = stream.read(&mut buf).map_err(|e| {
            self.connected = false;
            self.status = DriverStatus::Fault(format!("read failed: {e}"));
            DriverError::Timeout(format!("read from {addr_str}: {e}"))
        })?;

        if n == 0 {
            self.connected = false;
            return Err(DriverError::CommFault("connection closed by remote".into()));
        }

        ModbusFrame::decode(&buf[..n])
    }
}

#[async_trait]
impl AsyncDriver for ModbusDriver {
    fn driver_type(&self) -> &'static str {
        "modbus"
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn status(&self) -> &DriverStatus {
        &self.status
    }

    async fn open(&mut self) -> Result<DriverMeta, DriverError> {
        // Attempt TCP connection to validate configuration.
        match self.try_connect() {
            Ok(()) => {
                self.status = DriverStatus::Ok;
                Ok(DriverMeta {
                    model: Some(format!(
                        "Modbus TCP {}:{} unit {} ({} registers)",
                        self.host, self.port, self.slave_id, self.register_map.len()
                    )),
                    ..Default::default()
                })
            }
            Err(_) => {
                // Connection failed, but driver is still usable (will retry on sync).
                // Status already set to Fault by try_connect.
                Ok(DriverMeta {
                    model: Some(format!(
                        "Modbus TCP {}:{} unit {} (offline)",
                        self.host, self.port, self.slave_id
                    )),
                    ..Default::default()
                })
            }
        }
    }

    async fn close(&mut self) {
        self.connected = false;
        self.status = DriverStatus::Down;
    }

    async fn ping(&mut self) -> Result<DriverMeta, DriverError> {
        // Try to connect (or reconnect) as a health check.
        self.try_connect()?;
        Ok(DriverMeta {
            model: Some(format!("Modbus TCP {}:{} unit {}", self.host, self.port, self.slave_id)),
            ..Default::default()
        })
    }

    async fn learn(&mut self, _path: Option<&str>) -> Result<LearnGrid, DriverError> {
        let mut points: Vec<LearnPoint> = self
            .register_map
            .iter()
            .map(|(&id, reg)| {
                let kind = match reg.register_type {
                    RegisterType::Coil | RegisterType::DiscreteInput => "Bool".to_string(),
                    RegisterType::HoldingRegister | RegisterType::InputRegister => "Number".to_string(),
                };
                let mut tags = HashMap::new();
                tags.insert("pointId".to_string(), id.to_string());
                if reg.scale != 1.0 {
                    tags.insert("scale".to_string(), reg.scale.to_string());
                }
                if reg.offset != 0.0 {
                    tags.insert("offset".to_string(), reg.offset.to_string());
                }
                if reg.register_type.is_writable() {
                    tags.insert("writable".to_string(), "true".to_string());
                }
                LearnPoint {
                    name: format!("{}_{}", reg.register_type.label().to_lowercase(), reg.address),
                    address: format!("{}:{}", reg.register_type.label(), reg.address),
                    kind,
                    unit: None,
                    tags,
                }
            })
            .collect();
        // Sort by address for stable output.
        points.sort_by_key(|p| p.address.clone());
        Ok(points)
    }

    async fn sync_cur(&mut self, points: &[DriverPointRef]) -> Vec<(u32, Result<f64, DriverError>)> {
        if !self.connected {
            // All reads fail when not connected.
            return points
                .iter()
                .map(|p| {
                    (
                        p.point_id,
                        Err(DriverError::CommFault("not connected".into())),
                    )
                })
                .collect();
        }

        let mut results = Vec::with_capacity(points.len());
        for pt in points {
            let reg = match self.register_map.get(&pt.point_id) {
                Some(r) => r.clone(),
                None => {
                    results.push((pt.point_id, Err(DriverError::ConfigFault(
                        format!("no register mapping for point {}", pt.point_id),
                    ))));
                    continue;
                }
            };

            let frame = self.build_read_frame(&reg);
            match self.transact(&frame) {
                Ok(resp) => {
                    let value = match reg.register_type {
                        RegisterType::Coil | RegisterType::DiscreteInput => {
                            resp.parse_coil_response(1)
                                .map(|bits| if bits.first().copied().unwrap_or(false) { 1.0 } else { 0.0 })
                        }
                        RegisterType::HoldingRegister | RegisterType::InputRegister => {
                            resp.parse_register_response()
                                .map(|regs| reg.raw_to_eng(regs.first().copied().unwrap_or(0) as f64))
                        }
                    };
                    results.push((pt.point_id, value));
                }
                Err(e) => {
                    results.push((pt.point_id, Err(e)));
                }
            }
        }
        results
    }

    async fn write(&mut self, writes: &[(u32, f64)]) -> Vec<(u32, Result<(), DriverError>)> {
        if !self.connected {
            return writes
                .iter()
                .map(|(id, _)| (*id, Err(DriverError::CommFault("not connected".into()))))
                .collect();
        }

        let mut results = Vec::with_capacity(writes.len());
        for &(point_id, value) in writes {
            let reg = match self.register_map.get(&point_id) {
                Some(r) => r.clone(),
                None => {
                    results.push((point_id, Err(DriverError::ConfigFault(
                        format!("no register mapping for point {}", point_id),
                    ))));
                    continue;
                }
            };

            match self.build_write_frame(&reg, value) {
                Ok(frame) => {
                    match self.transact(&frame) {
                        Ok(_resp) => results.push((point_id, Ok(()))),
                        Err(e) => results.push((point_id, Err(e))),
                    }
                }
                Err(e) => results.push((point_id, Err(e))),
            }
        }
        results
    }

    fn poll_mode(&self) -> PollMode {
        PollMode::Buckets
    }
}

// ── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ModbusFrame encode/decode ──────────────────────────

    #[test]
    fn frame_encode_decode_roundtrip() {
        let frame = ModbusFrame {
            transaction_id: 42,
            unit_id: 1,
            function_code: fc::READ_HOLDING_REGISTERS,
            data: vec![0x00, 0x0A, 0x00, 0x02], // start=10, count=2
        };
        let bytes = frame.encode();
        assert_eq!(bytes.len(), 12); // 7 header + 1 unit + 1 fc + 4 data (wait, 7+1 fc already in header count)

        // Verify MBAP header
        assert_eq!(u16::from_be_bytes([bytes[0], bytes[1]]), 42); // txn ID
        assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]), 0);  // protocol ID
        assert_eq!(u16::from_be_bytes([bytes[4], bytes[5]]), 6);  // length = 1+1+4
        assert_eq!(bytes[6], 1);  // unit ID
        assert_eq!(bytes[7], fc::READ_HOLDING_REGISTERS); // FC

        let decoded = ModbusFrame::decode(&bytes).unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn frame_decode_too_short() {
        let result = ModbusFrame::decode(&[0; 5]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too short"));
    }

    #[test]
    fn frame_decode_bad_protocol_id() {
        let mut bytes = ModbusFrame {
            transaction_id: 0,
            unit_id: 1,
            function_code: 3,
            data: vec![0, 0, 0, 1],
        }
        .encode();
        // Corrupt protocol ID
        bytes[2] = 0x00;
        bytes[3] = 0x01;
        let result = ModbusFrame::decode(&bytes);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("protocol ID"));
    }

    #[test]
    fn frame_decode_exception_response() {
        // Simulate an exception response: FC 0x83 (read holding regs exception), code 2
        let bytes = [
            0x00, 0x01, // txn
            0x00, 0x00, // protocol
            0x00, 0x03, // length = 3 (unit + fc + exception code)
            0x01,       // unit
            0x83,       // FC 03 + 0x80 = exception
            0x02,       // exception code 2 = illegal data address
        ];
        let result = ModbusFrame::decode(&bytes);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("exception"));
    }

    #[test]
    fn frame_decode_truncated() {
        // Length field says 10 bytes but we only have 3 after header
        let bytes = [
            0x00, 0x01, // txn
            0x00, 0x00, // protocol
            0x00, 0x0A, // length = 10
            0x01,       // unit
            0x03,       // FC
            0x00,       // only 1 data byte, need 8 more
        ];
        let result = ModbusFrame::decode(&bytes);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("truncated"));
    }

    // ── Frame builder helpers ──────────────────────────────

    #[test]
    fn frame_read_coils() {
        let f = ModbusFrame::read_coils(1, 5, 100, 8);
        assert_eq!(f.function_code, fc::READ_COILS);
        assert_eq!(f.unit_id, 5);
        assert_eq!(f.data, vec![0x00, 0x64, 0x00, 0x08]);
    }

    #[test]
    fn frame_read_discrete_inputs() {
        let f = ModbusFrame::read_discrete_inputs(2, 1, 0, 16);
        assert_eq!(f.function_code, fc::READ_DISCRETE_INPUTS);
        assert_eq!(f.data, vec![0x00, 0x00, 0x00, 0x10]);
    }

    #[test]
    fn frame_read_holding_registers() {
        let f = ModbusFrame::read_holding_registers(3, 1, 40001, 2);
        assert_eq!(f.function_code, fc::READ_HOLDING_REGISTERS);
        // 40001 = 0x9C41
        assert_eq!(f.data, vec![0x9C, 0x41, 0x00, 0x02]);
    }

    #[test]
    fn frame_read_input_registers() {
        let f = ModbusFrame::read_input_registers(4, 1, 30001, 1);
        assert_eq!(f.function_code, fc::READ_INPUT_REGISTERS);
    }

    #[test]
    fn frame_write_single_coil_on() {
        let f = ModbusFrame::write_single_coil(5, 1, 50, true);
        assert_eq!(f.function_code, fc::WRITE_SINGLE_COIL);
        assert_eq!(f.data, vec![0x00, 0x32, 0xFF, 0x00]); // addr=50, value=0xFF00
    }

    #[test]
    fn frame_write_single_coil_off() {
        let f = ModbusFrame::write_single_coil(6, 1, 50, false);
        assert_eq!(f.data, vec![0x00, 0x32, 0x00, 0x00]); // addr=50, value=0x0000
    }

    #[test]
    fn frame_write_single_register() {
        let f = ModbusFrame::write_single_register(7, 1, 100, 1234);
        assert_eq!(f.function_code, fc::WRITE_SINGLE_REGISTER);
        // addr=100 (0x0064), value=1234 (0x04D2)
        assert_eq!(f.data, vec![0x00, 0x64, 0x04, 0xD2]);
    }

    #[test]
    fn frame_write_multiple_registers() {
        let f = ModbusFrame::write_multiple_registers(8, 1, 200, &[100, 200]);
        assert_eq!(f.function_code, fc::WRITE_MULTIPLE_REGISTERS);
        // start=200, count=2, byte_count=4, values
        assert_eq!(
            f.data,
            vec![
                0x00, 0xC8, // start addr 200
                0x00, 0x02, // count 2
                0x04,       // byte count 4
                0x00, 0x64, // value 100
                0x00, 0xC8, // value 200
            ]
        );
    }

    // ── Response parsing ───────────────────────────────────

    #[test]
    fn parse_register_response_two_regs() {
        let resp = ModbusFrame {
            transaction_id: 1,
            unit_id: 1,
            function_code: fc::READ_HOLDING_REGISTERS,
            data: vec![0x04, 0x00, 0x64, 0x01, 0xF4], // byte_count=4, reg[0]=100, reg[1]=500
        };
        let regs = resp.parse_register_response().unwrap();
        assert_eq!(regs, vec![100, 500]);
    }

    #[test]
    fn parse_register_response_empty() {
        let resp = ModbusFrame {
            transaction_id: 1,
            unit_id: 1,
            function_code: fc::READ_HOLDING_REGISTERS,
            data: vec![],
        };
        assert!(resp.parse_register_response().is_err());
    }

    #[test]
    fn parse_coil_response_8_coils() {
        // 8 coils: bits 0,1,4 are ON = 0b00010011 = 0x13
        let resp = ModbusFrame {
            transaction_id: 1,
            unit_id: 1,
            function_code: fc::READ_COILS,
            data: vec![0x01, 0x13], // byte_count=1, data=0x13
        };
        let coils = resp.parse_coil_response(8).unwrap();
        assert_eq!(coils.len(), 8);
        assert!(coils[0]);  // bit 0
        assert!(coils[1]);  // bit 1
        assert!(!coils[2]); // bit 2
        assert!(!coils[3]); // bit 3
        assert!(coils[4]);  // bit 4
        assert!(!coils[5]); // bit 5
        assert!(!coils[6]); // bit 6
        assert!(!coils[7]); // bit 7
    }

    #[test]
    fn parse_coil_response_empty() {
        let resp = ModbusFrame {
            transaction_id: 1,
            unit_id: 1,
            function_code: fc::READ_COILS,
            data: vec![],
        };
        assert!(resp.parse_coil_response(1).is_err());
    }

    // ── RegisterType ───────────────────────────────────────

    #[test]
    fn register_type_read_fc() {
        assert_eq!(RegisterType::Coil.read_fc(), fc::READ_COILS);
        assert_eq!(RegisterType::DiscreteInput.read_fc(), fc::READ_DISCRETE_INPUTS);
        assert_eq!(RegisterType::HoldingRegister.read_fc(), fc::READ_HOLDING_REGISTERS);
        assert_eq!(RegisterType::InputRegister.read_fc(), fc::READ_INPUT_REGISTERS);
    }

    #[test]
    fn register_type_writable() {
        assert!(RegisterType::Coil.is_writable());
        assert!(RegisterType::HoldingRegister.is_writable());
        assert!(!RegisterType::DiscreteInput.is_writable());
        assert!(!RegisterType::InputRegister.is_writable());
    }

    #[test]
    fn register_type_label() {
        assert_eq!(RegisterType::HoldingRegister.label(), "HR");
        assert_eq!(RegisterType::InputRegister.label(), "IR");
        assert_eq!(RegisterType::Coil.label(), "CO");
        assert_eq!(RegisterType::DiscreteInput.label(), "DI");
    }

    #[test]
    fn register_type_display() {
        assert_eq!(format!("{}", RegisterType::HoldingRegister), "HR");
    }

    // ── ModbusRegister scaling ─────────────────────────────

    #[test]
    fn register_scaling_identity() {
        let reg = ModbusRegister::new(100, RegisterType::HoldingRegister);
        assert_eq!(reg.raw_to_eng(42.0), 42.0);
        assert_eq!(reg.eng_to_raw(42.0), 42.0);
    }

    #[test]
    fn register_scaling_factor() {
        let reg = ModbusRegister::with_scaling(100, RegisterType::HoldingRegister, 0.1, -10.0);
        // raw=250 -> 250*0.1 + (-10) = 15.0
        assert!((reg.raw_to_eng(250.0) - 15.0).abs() < f64::EPSILON);
        // eng=15 -> (15 - (-10)) / 0.1 = 250.0
        assert!((reg.eng_to_raw(15.0) - 250.0).abs() < f64::EPSILON);
    }

    #[test]
    fn register_scaling_zero_scale() {
        let reg = ModbusRegister::with_scaling(0, RegisterType::HoldingRegister, 0.0, 5.0);
        assert_eq!(reg.eng_to_raw(100.0), 0.0); // division by zero handled
    }

    // ── ModbusDriver lifecycle ─────────────────────────────

    #[test]
    fn modbus_driver_new() {
        let d = ModbusDriver::new("mb-1", "192.168.1.100", 502, 1);
        assert_eq!(d.id(), "mb-1");
        assert_eq!(d.host(), "192.168.1.100");
        assert_eq!(d.port(), 502);
        assert_eq!(d.slave_id(), 1);
        assert_eq!(d.driver_type(), "modbus");
        assert_eq!(*d.status(), DriverStatus::Pending);
        assert!(!d.is_connected());
        assert_eq!(d.register_count(), 0);
    }

    #[test]
    fn modbus_driver_register_map() {
        let mut d = ModbusDriver::new("mb-2", "x", 502, 1);
        d.add_register(100, ModbusRegister::new(40001, RegisterType::HoldingRegister));
        d.add_register(200, ModbusRegister::new(0, RegisterType::Coil));
        assert_eq!(d.register_count(), 2);

        let reg = d.get_register(100).unwrap();
        assert_eq!(reg.address, 40001);
        assert_eq!(reg.register_type, RegisterType::HoldingRegister);

        assert!(d.get_register(999).is_none());
    }

    #[tokio::test]
    async fn modbus_lifecycle_no_server() {
        // open() should handle connection failure gracefully.
        let mut d = ModbusDriver::new("mb-3", "127.0.0.1", 59999, 1);
        let meta = d.open().await.unwrap(); // open doesn't fail, just marks as fault
        assert!(meta.model.unwrap().contains("offline"));
        assert!(matches!(d.status(), DriverStatus::Fault(_)));

        d.close().await;
        assert_eq!(*d.status(), DriverStatus::Down);
        assert!(!d.is_connected());
    }

    #[tokio::test]
    async fn modbus_learn_empty_map() {
        let mut d = ModbusDriver::new("mb-4", "x", 502, 1);
        let grid = d.learn(None).await.unwrap();
        assert!(grid.is_empty());
    }

    #[tokio::test]
    async fn modbus_learn_with_registers() {
        let mut d = ModbusDriver::new("mb-5", "x", 502, 1);
        d.add_register(100, ModbusRegister::new(40001, RegisterType::HoldingRegister));
        d.add_register(200, ModbusRegister::with_scaling(0, RegisterType::Coil, 1.0, 0.0));
        d.add_register(300, ModbusRegister::new(30001, RegisterType::InputRegister));

        let grid = d.learn(None).await.unwrap();
        assert_eq!(grid.len(), 3);

        // Verify sorted by address
        // CO:0, HR:40001, IR:30001 -> sorted: CO:0, HR:40001, IR:30001
        // Actually sorted lexicographically: "CO:0" < "HR:40001" < "IR:30001"
        assert!(grid[0].address.starts_with("CO:"));
        assert!(grid[1].address.starts_with("HR:"));
        assert!(grid[2].address.starts_with("IR:"));

        // Bool kind for coils
        assert_eq!(grid[0].kind, "Bool");
        // Number kind for registers
        assert_eq!(grid[1].kind, "Number");
        // Writable tag on HR
        assert_eq!(grid[1].tags.get("writable"), Some(&"true".to_string()));
    }

    #[tokio::test]
    async fn modbus_sync_cur_not_connected() {
        let mut d = ModbusDriver::new("mb-6", "x", 502, 1);
        d.add_register(100, ModbusRegister::new(0, RegisterType::HoldingRegister));

        let refs = vec![DriverPointRef {
            point_id: 100,
            address: "HR:0".into(),
        }];
        let results = d.sync_cur(&refs).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_err());
        assert!(results[0].1.as_ref().unwrap_err().to_string().contains("not connected"));
    }

    #[tokio::test]
    async fn modbus_sync_cur_unknown_point() {
        let mut d = ModbusDriver::new("mb-7", "x", 502, 1);
        d.connected = true; // pretend connected

        let refs = vec![DriverPointRef {
            point_id: 999,
            address: "HR:0".into(),
        }];
        let results = d.sync_cur(&refs).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_err());
        assert!(results[0].1.as_ref().unwrap_err().to_string().contains("no register mapping"));
    }

    #[tokio::test]
    async fn modbus_write_not_connected() {
        let mut d = ModbusDriver::new("mb-8", "x", 502, 1);
        d.add_register(100, ModbusRegister::new(0, RegisterType::HoldingRegister));

        let results = d.write(&[(100, 42.0)]).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_err());
        assert!(results[0].1.as_ref().unwrap_err().to_string().contains("not connected"));
    }

    #[test]
    fn modbus_write_readonly_register() {
        let mut d = ModbusDriver::new("mb-9", "x", 502, 1);
        d.add_register(100, ModbusRegister::new(0, RegisterType::InputRegister));
        d.connected = true; // pretend connected

        // build_write_frame should fail for InputRegister
        let reg = d.get_register(100).unwrap().clone();
        let result = d.build_write_frame(&reg, 42.0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("read-only"));
    }

    #[test]
    fn modbus_poll_mode() {
        let d = ModbusDriver::new("mb-10", "x", 502, 1);
        assert_eq!(d.poll_mode(), PollMode::Buckets);
    }

    #[test]
    fn modbus_transaction_counter_wraps() {
        let mut d = ModbusDriver::new("mb-11", "x", 502, 1);
        d.transaction_counter = u16::MAX;
        assert_eq!(d.next_txn(), u16::MAX);
        assert_eq!(d.next_txn(), 0); // wrapped
        assert_eq!(d.next_txn(), 1);
    }

    #[test]
    fn modbus_build_read_frame_types() {
        let mut d = ModbusDriver::new("mb-12", "x", 502, 3);

        let reg_hr = ModbusRegister::new(100, RegisterType::HoldingRegister);
        let f = d.build_read_frame(&reg_hr);
        assert_eq!(f.function_code, fc::READ_HOLDING_REGISTERS);
        assert_eq!(f.unit_id, 3);

        let reg_ir = ModbusRegister::new(200, RegisterType::InputRegister);
        let f = d.build_read_frame(&reg_ir);
        assert_eq!(f.function_code, fc::READ_INPUT_REGISTERS);

        let reg_co = ModbusRegister::new(50, RegisterType::Coil);
        let f = d.build_read_frame(&reg_co);
        assert_eq!(f.function_code, fc::READ_COILS);

        let reg_di = ModbusRegister::new(60, RegisterType::DiscreteInput);
        let f = d.build_read_frame(&reg_di);
        assert_eq!(f.function_code, fc::READ_DISCRETE_INPUTS);
    }

    #[test]
    fn modbus_build_write_frame_coil() {
        let mut d = ModbusDriver::new("mb-13", "x", 502, 1);
        let reg = ModbusRegister::new(10, RegisterType::Coil);

        let f = d.build_write_frame(&reg, 1.0).unwrap();
        assert_eq!(f.function_code, fc::WRITE_SINGLE_COIL);
        // value=true -> 0xFF00
        assert_eq!(f.data[2..4], [0xFF, 0x00]);

        let f = d.build_write_frame(&reg, 0.0).unwrap();
        assert_eq!(f.data[2..4], [0x00, 0x00]);
    }

    #[test]
    fn modbus_build_write_frame_holding_register() {
        let mut d = ModbusDriver::new("mb-14", "x", 502, 1);
        let reg = ModbusRegister::with_scaling(100, RegisterType::HoldingRegister, 0.1, 0.0);

        // eng=25.0 -> raw = 25.0 / 0.1 = 250
        let f = d.build_write_frame(&reg, 25.0).unwrap();
        assert_eq!(f.function_code, fc::WRITE_SINGLE_REGISTER);
        // address=100 (0x0064), value=250 (0x00FA)
        assert_eq!(f.data, vec![0x00, 0x64, 0x00, 0xFA]);
    }

    #[tokio::test]
    async fn modbus_sync_cur_empty() {
        let mut d = ModbusDriver::new("mb-15", "x", 502, 1);
        let results = d.sync_cur(&[]).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn modbus_write_empty() {
        let mut d = ModbusDriver::new("mb-16", "x", 502, 1);
        let results = d.write(&[]).await;
        assert!(results.is_empty());
    }
}
