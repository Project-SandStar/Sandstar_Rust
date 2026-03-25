//! SOX binary protocol message parser and builder.
//!
//! Implements the Sedona Object eXchange (SOX) wire protocol used for
//! device communication over DASP/UDP. Every SOX message has a 2-byte header:
//!
//! ```text
//! Byte 0: Command code (ASCII lowercase = request, uppercase = response)
//! Byte 1: Request ID   (0-254 for correlation, 0xFF = none)
//! Bytes 2+: Command-specific payload
//! ```
//!
//! Multi-byte integers are **big-endian** on the wire.
//! Strings are length-prefixed: 1 byte length then UTF-8 bytes.
//! Component IDs are u16, slot IDs are u8.

// ---------------------------------------------------------------------------
// Command codes
// ---------------------------------------------------------------------------

/// SOX command codes as defined in the Sedona specification.
///
/// Lowercase ASCII letters are used for requests; the server responds with the
/// corresponding uppercase letter on success, or `!` on error.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SoxCmd {
    // -- Queries --
    /// Read schema: kit names + checksums ('v' request, 'V' response).
    ReadSchema = b'v',
    /// Read version: platform ID + kit versions ('y' request, 'Y' response).
    ReadVersion = b'y',
    /// Read component tree + slot values.
    ReadComp = b'c',
    /// Read single property ('r' request, 'R' response).
    ReadProp = b'r',
    /// Read link.
    ReadLink = b'l',

    // -- Subscriptions --
    /// Register for COV (change-of-value) events.
    Subscribe = b's',
    /// Unregister COV.
    Unsubscribe = b'u',

    // -- Mutations --
    /// Write slot value.
    Write = b'w',
    /// Invoke action ('i' from CC editor, 'k' from standard Sedona).
    Invoke = b'i',
    /// Add component.
    Add = b'a',
    /// Delete component.
    Delete = b'd',
    /// Rename component ('n' request, 'N' response).
    Rename = b'n',
    /// Reorder children.
    Reorder = b'o',

    // -- File transfer --
    /// Open file for transfer.
    FileOpen = b'f',
    /// Read file chunk.
    FileRead = b'g',
    /// Write file chunk.
    FileWrite = b'h',
    /// Close file transfer.
    FileClose = b'q',
    /// Rename file.
    FileRename = b'x',

    // -- Events (server -> client) --
    /// Changed-value notification (server push, no reply expected).
    Event = b'e',
}

impl SoxCmd {
    /// Try to convert a raw byte into a `SoxCmd`.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            b'v' => Some(Self::ReadSchema),
            b'y' => Some(Self::ReadVersion),
            b'c' => Some(Self::ReadComp),
            b'r' => Some(Self::ReadProp),
            b'l' => Some(Self::ReadLink),
            b's' => Some(Self::Subscribe),
            b'u' => Some(Self::Unsubscribe),
            b'w' => Some(Self::Write),
            b'k' | b'i' => Some(Self::Invoke), // 'i' used by CC editor, 'k' by standard Sedona
            b'a' => Some(Self::Add),
            b'd' => Some(Self::Delete),
            b'n' => Some(Self::Rename),
            b'o' => Some(Self::Reorder),
            b'f' => Some(Self::FileOpen),
            b'g' => Some(Self::FileRead),
            b'h' => Some(Self::FileWrite),
            b'q' => Some(Self::FileClose),
            b'x' => Some(Self::FileRename),
            b'e' => Some(Self::Event),
            _ => None,
        }
    }

    /// Return the uppercase response byte for this command (success response).
    pub fn response_byte(self) -> u8 {
        (self as u8).to_ascii_uppercase()
    }

    /// Return the uppercase error-response byte for this command.
    ///
    /// In SOX, an error response uses the `!` command byte.
    pub fn error_byte() -> u8 {
        b'!'
    }
}

// ---------------------------------------------------------------------------
// Value types
// ---------------------------------------------------------------------------

/// Sedona value types for SOX wire encoding.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoxValueType {
    Void = 0,
    Bool = 1,
    Byte = 2,
    Short = 3,
    Int = 4,
    Long = 5,
    Float = 6,
    Double = 7,
    Buf = 8,
}

impl SoxValueType {
    /// Try to convert a raw byte into a `SoxValueType`.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Void),
            1 => Some(Self::Bool),
            2 => Some(Self::Byte),
            3 => Some(Self::Short),
            4 => Some(Self::Int),
            5 => Some(Self::Long),
            6 => Some(Self::Float),
            7 => Some(Self::Double),
            8 => Some(Self::Buf),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// A parsed SOX request.
#[derive(Debug, Clone, PartialEq)]
pub struct SoxRequest {
    /// The command code.
    pub cmd: SoxCmd,
    /// Correlation ID (0-254, 0xFF = unsolicited).
    pub req_id: u8,
    /// Command-specific payload bytes (may be empty).
    pub payload: Vec<u8>,
}

impl SoxRequest {
    /// Parse a SOX request from raw bytes.
    ///
    /// Returns `None` if the data is too short (< 2 bytes) or contains an
    /// unrecognised command code.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 2 {
            return None;
        }
        let cmd = SoxCmd::from_byte(data[0])?;
        let req_id = data[1];
        let payload = data[2..].to_vec();
        Some(Self {
            cmd,
            req_id,
            payload,
        })
    }
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

/// SOX response builder.
///
/// Accumulates a payload via typed `write_*` helpers and serialises to bytes
/// with [`to_bytes`](Self::to_bytes).
#[derive(Debug, Clone)]
pub struct SoxResponse {
    /// The command/status byte (uppercase = success, `!` = error).
    pub cmd: u8,
    /// Correlation ID matching the request.
    pub req_id: u8,
    /// Accumulated payload.
    pub payload: Vec<u8>,
}

impl SoxResponse {
    /// Create a **success** response for the given command and request ID.
    pub fn success(cmd: SoxCmd, req_id: u8) -> Self {
        Self {
            cmd: cmd.response_byte(),
            req_id,
            payload: Vec::new(),
        }
    }

    /// Create an **error** response for the given request ID.
    pub fn error(cmd: SoxCmd, req_id: u8) -> Self {
        let _ = cmd; // included for API symmetry
        Self {
            cmd: SoxCmd::error_byte(),
            req_id,
            payload: Vec::new(),
        }
    }

    // -- Payload writers (big-endian) --

    /// Append a `u8` to the payload.
    pub fn write_u8(&mut self, val: u8) -> &mut Self {
        self.payload.push(val);
        self
    }

    /// Append a `u16` (big-endian) to the payload.
    pub fn write_u16(&mut self, val: u16) -> &mut Self {
        self.payload.extend_from_slice(&val.to_be_bytes());
        self
    }

    /// Append a `u32` (big-endian) to the payload.
    pub fn write_u32(&mut self, val: u32) -> &mut Self {
        self.payload.extend_from_slice(&val.to_be_bytes());
        self
    }

    /// Append an `i32` (big-endian) to the payload.
    pub fn write_i32(&mut self, val: i32) -> &mut Self {
        self.payload.extend_from_slice(&val.to_be_bytes());
        self
    }

    /// Append an `f32` (big-endian IEEE 754) to the payload.
    pub fn write_f32(&mut self, val: f32) -> &mut Self {
        self.payload.extend_from_slice(&val.to_be_bytes());
        self
    }

    /// Append an `f64` (big-endian IEEE 754) to the payload.
    pub fn write_f64(&mut self, val: f64) -> &mut Self {
        self.payload.extend_from_slice(&val.to_be_bytes());
        self
    }

    /// Append a null-terminated string (UTF-8 bytes + 0x00).
    ///
    /// This matches the Sedona SOX wire format where `str()` reads until NUL.
    pub fn write_str(&mut self, s: &str) -> &mut Self {
        self.payload.extend_from_slice(s.as_bytes());
        self.payload.push(0x00);
        self
    }

    /// Append raw bytes to the payload (no length prefix).
    pub fn write_bytes(&mut self, data: &[u8]) -> &mut Self {
        self.payload.extend_from_slice(data);
        self
    }

    /// Serialise the response to a byte vector suitable for the wire.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.payload.len());
        out.push(self.cmd);
        out.push(self.req_id);
        out.extend_from_slice(&self.payload);
        out
    }
}

// ---------------------------------------------------------------------------
// Payload reader
// ---------------------------------------------------------------------------

/// Cursor-style reader for parsing SOX request payloads.
///
/// All multi-byte reads are **big-endian** per the SOX wire format.
#[derive(Debug)]
pub struct SoxReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> SoxReader<'a> {
    /// Create a new reader over the given byte slice.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Number of bytes remaining.
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    /// Read a single `u8`.
    pub fn read_u8(&mut self) -> Option<u8> {
        if self.pos < self.data.len() {
            let v = self.data[self.pos];
            self.pos += 1;
            Some(v)
        } else {
            None
        }
    }

    /// Read a big-endian `u16`.
    pub fn read_u16(&mut self) -> Option<u16> {
        if self.pos + 2 <= self.data.len() {
            let v = u16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
            self.pos += 2;
            Some(v)
        } else {
            None
        }
    }

    /// Read a big-endian `u32`.
    pub fn read_u32(&mut self) -> Option<u32> {
        if self.pos + 4 <= self.data.len() {
            let v = u32::from_be_bytes([
                self.data[self.pos],
                self.data[self.pos + 1],
                self.data[self.pos + 2],
                self.data[self.pos + 3],
            ]);
            self.pos += 4;
            Some(v)
        } else {
            None
        }
    }

    /// Read a big-endian `i32`.
    pub fn read_i32(&mut self) -> Option<i32> {
        if self.pos + 4 <= self.data.len() {
            let v = i32::from_be_bytes([
                self.data[self.pos],
                self.data[self.pos + 1],
                self.data[self.pos + 2],
                self.data[self.pos + 3],
            ]);
            self.pos += 4;
            Some(v)
        } else {
            None
        }
    }

    /// Read a big-endian `f32` (IEEE 754).
    pub fn read_f32(&mut self) -> Option<f32> {
        self.read_u32().map(f32::from_bits)
    }

    /// Read a big-endian `f64` (IEEE 754).
    pub fn read_f64(&mut self) -> Option<f64> {
        if self.pos + 8 <= self.data.len() {
            let v = u64::from_be_bytes([
                self.data[self.pos],
                self.data[self.pos + 1],
                self.data[self.pos + 2],
                self.data[self.pos + 3],
                self.data[self.pos + 4],
                self.data[self.pos + 5],
                self.data[self.pos + 6],
                self.data[self.pos + 7],
            ]);
            self.pos += 8;
            Some(f64::from_bits(v))
        } else {
            None
        }
    }

    /// Read a length-prefixed string (u8 length + UTF-8 bytes).
    /// Read a null-terminated string from the payload.
    ///
    /// Matches the Sedona SOX wire format where strings end with 0x00.
    pub fn read_str(&mut self) -> Option<String> {
        let start = self.pos;
        while self.pos < self.data.len() {
            if self.data[self.pos] == 0x00 {
                let s = String::from_utf8_lossy(&self.data[start..self.pos]).into_owned();
                self.pos += 1; // skip NUL
                return Some(s);
            }
            self.pos += 1;
        }
        None // no NUL terminator found
    }

    /// Read exactly `len` raw bytes.
    pub fn read_bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.pos + len <= self.data.len() {
            let slice = &self.data[self.pos..self.pos + len];
            self.pos += len;
            Some(slice)
        } else {
            None
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // SoxCmd
    // -----------------------------------------------------------------------

    #[test]
    fn cmd_from_byte_all_known() {
        let cases: &[(u8, SoxCmd)] = &[
            (b'v', SoxCmd::ReadSchema),
            (b'y', SoxCmd::ReadVersion),
            (b'n', SoxCmd::Rename),
            (b'c', SoxCmd::ReadComp),
            (b'r', SoxCmd::ReadProp),
            (b'l', SoxCmd::ReadLink),
            (b's', SoxCmd::Subscribe),
            (b'u', SoxCmd::Unsubscribe),
            (b'w', SoxCmd::Write),
            (b'k', SoxCmd::Invoke),
            (b'a', SoxCmd::Add),
            (b'd', SoxCmd::Delete),
            (b'n', SoxCmd::Rename),
            (b'o', SoxCmd::Reorder),
            (b'f', SoxCmd::FileOpen),
            (b'g', SoxCmd::FileRead),
            (b'h', SoxCmd::FileWrite),
            (b'q', SoxCmd::FileClose),
            (b'x', SoxCmd::FileRename),
            (b'e', SoxCmd::Event),
        ];
        for &(byte, expected) in cases {
            assert_eq!(
                SoxCmd::from_byte(byte),
                Some(expected),
                "failed for byte 0x{byte:02x}"
            );
        }
    }

    #[test]
    fn cmd_from_byte_unknown_returns_none() {
        assert_eq!(SoxCmd::from_byte(b'z'), None);
        assert_eq!(SoxCmd::from_byte(0x00), None);
        assert_eq!(SoxCmd::from_byte(0xFF), None);
    }

    #[test]
    fn cmd_response_byte_is_uppercase() {
        assert_eq!(SoxCmd::ReadComp.response_byte(), b'C');
        assert_eq!(SoxCmd::Write.response_byte(), b'W');
        assert_eq!(SoxCmd::Subscribe.response_byte(), b'S');
        assert_eq!(SoxCmd::ReadVersion.response_byte(), b'Y');
    }

    #[test]
    fn cmd_error_byte_is_bang() {
        assert_eq!(SoxCmd::error_byte(), b'!');
    }

    #[test]
    fn cmd_repr_values_match_sedona_spec() {
        assert_eq!(SoxCmd::ReadSchema as u8, 0x76);
        assert_eq!(SoxCmd::ReadVersion as u8, 0x79);
        assert_eq!(SoxCmd::Rename as u8, 0x6E);
        assert_eq!(SoxCmd::ReadComp as u8, 0x63);
        assert_eq!(SoxCmd::Subscribe as u8, 0x73);
        assert_eq!(SoxCmd::Unsubscribe as u8, 0x75);
        assert_eq!(SoxCmd::Write as u8, 0x77);
        assert_eq!(SoxCmd::Invoke as u8, 0x69); // 'i' — CC editor invoke byte
        assert_eq!(SoxCmd::Add as u8, 0x61);
        assert_eq!(SoxCmd::Delete as u8, 0x64);
        assert_eq!(SoxCmd::Rename as u8, 0x6E);
        assert_eq!(SoxCmd::ReadProp as u8, 0x72);
        assert_eq!(SoxCmd::Reorder as u8, 0x6F);
        assert_eq!(SoxCmd::FileOpen as u8, 0x66);
        assert_eq!(SoxCmd::FileRead as u8, 0x67);
        assert_eq!(SoxCmd::FileWrite as u8, 0x68);
        assert_eq!(SoxCmd::FileClose as u8, 0x71);
        assert_eq!(SoxCmd::FileRename as u8, 0x78);
        assert_eq!(SoxCmd::Event as u8, 0x65);
    }

    // -----------------------------------------------------------------------
    // SoxValueType
    // -----------------------------------------------------------------------

    #[test]
    fn value_type_from_byte_all() {
        assert_eq!(SoxValueType::from_byte(0), Some(SoxValueType::Void));
        assert_eq!(SoxValueType::from_byte(1), Some(SoxValueType::Bool));
        assert_eq!(SoxValueType::from_byte(2), Some(SoxValueType::Byte));
        assert_eq!(SoxValueType::from_byte(3), Some(SoxValueType::Short));
        assert_eq!(SoxValueType::from_byte(4), Some(SoxValueType::Int));
        assert_eq!(SoxValueType::from_byte(5), Some(SoxValueType::Long));
        assert_eq!(SoxValueType::from_byte(6), Some(SoxValueType::Float));
        assert_eq!(SoxValueType::from_byte(7), Some(SoxValueType::Double));
        assert_eq!(SoxValueType::from_byte(8), Some(SoxValueType::Buf));
        assert_eq!(SoxValueType::from_byte(9), None);
        assert_eq!(SoxValueType::from_byte(255), None);
    }

    // -----------------------------------------------------------------------
    // SoxRequest::parse
    // -----------------------------------------------------------------------

    #[test]
    fn parse_valid_read_comp() {
        // cmd=c, reqId=7, payload = compId 0x0042 + what 't'
        let data = [b'c', 7, 0x00, 0x42, b't'];
        let req = SoxRequest::parse(&data).unwrap();
        assert_eq!(req.cmd, SoxCmd::ReadComp);
        assert_eq!(req.req_id, 7);
        assert_eq!(req.payload, vec![0x00, 0x42, b't']);
    }

    #[test]
    fn parse_valid_write() {
        // cmd=w, reqId=1, payload = compId(0x0010) slotId(3) + value byte
        let data = [b'w', 1, 0x00, 0x10, 3, 0x42];
        let req = SoxRequest::parse(&data).unwrap();
        assert_eq!(req.cmd, SoxCmd::Write);
        assert_eq!(req.req_id, 1);
        assert_eq!(req.payload, vec![0x00, 0x10, 3, 0x42]);
    }

    #[test]
    fn parse_empty_payload() {
        // readVersion has no payload
        let data = [b'y', 0];
        let req = SoxRequest::parse(&data).unwrap();
        assert_eq!(req.cmd, SoxCmd::ReadVersion);
        assert_eq!(req.req_id, 0);
        assert!(req.payload.is_empty());
    }

    #[test]
    fn parse_subscribe() {
        let data = [b's', 12, 0x00, 0x05, 0x07];
        let req = SoxRequest::parse(&data).unwrap();
        assert_eq!(req.cmd, SoxCmd::Subscribe);
        assert_eq!(req.req_id, 12);
        assert_eq!(req.payload, vec![0x00, 0x05, 0x07]);
    }

    #[test]
    fn parse_truncated_returns_none() {
        assert!(SoxRequest::parse(&[]).is_none());
        assert!(SoxRequest::parse(&[b'c']).is_none());
    }

    #[test]
    fn parse_unknown_command_returns_none() {
        let data = [b'Z', 0, 0x00];
        assert!(SoxRequest::parse(&data).is_none());
    }

    // -----------------------------------------------------------------------
    // SoxResponse
    // -----------------------------------------------------------------------

    #[test]
    fn response_success_header() {
        let resp = SoxResponse::success(SoxCmd::ReadComp, 5);
        assert_eq!(resp.cmd, b'C');
        assert_eq!(resp.req_id, 5);
        assert!(resp.payload.is_empty());
    }

    #[test]
    fn response_error_header() {
        let resp = SoxResponse::error(SoxCmd::Write, 3);
        assert_eq!(resp.cmd, b'!');
        assert_eq!(resp.req_id, 3);
    }

    #[test]
    fn response_write_u8() {
        let mut resp = SoxResponse::success(SoxCmd::ReadVersion, 0);
        resp.write_u8(0xAB);
        assert_eq!(resp.payload, vec![0xAB]);
    }

    #[test]
    fn response_write_u16_big_endian() {
        let mut resp = SoxResponse::success(SoxCmd::ReadComp, 0);
        resp.write_u16(0x1234);
        assert_eq!(resp.payload, vec![0x12, 0x34]);
    }

    #[test]
    fn response_write_u32_big_endian() {
        let mut resp = SoxResponse::success(SoxCmd::ReadComp, 0);
        resp.write_u32(0xDEADBEEF);
        assert_eq!(resp.payload, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn response_write_i32_big_endian() {
        let mut resp = SoxResponse::success(SoxCmd::ReadComp, 0);
        resp.write_i32(-1);
        assert_eq!(resp.payload, vec![0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn response_write_f32() {
        let mut resp = SoxResponse::success(SoxCmd::ReadComp, 0);
        resp.write_f32(1.0_f32);
        // IEEE 754: 1.0f = 0x3F800000
        assert_eq!(resp.payload, vec![0x3F, 0x80, 0x00, 0x00]);
    }

    #[test]
    fn response_write_f64() {
        let mut resp = SoxResponse::success(SoxCmd::ReadComp, 0);
        resp.write_f64(1.0_f64);
        // IEEE 754: 1.0d = 0x3FF0000000000000
        assert_eq!(
            resp.payload,
            vec![0x3F, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn response_write_str_null_terminated() {
        let mut resp = SoxResponse::success(SoxCmd::ReadComp, 0);
        resp.write_str("hello");
        assert_eq!(resp.payload, vec![b'h', b'e', b'l', b'l', b'o', 0x00]);
    }

    #[test]
    fn response_write_str_empty() {
        let mut resp = SoxResponse::success(SoxCmd::ReadComp, 0);
        resp.write_str("");
        assert_eq!(resp.payload, vec![0x00]);
    }

    #[test]
    fn response_write_bytes() {
        let mut resp = SoxResponse::success(SoxCmd::ReadComp, 0);
        resp.write_bytes(&[0xCA, 0xFE]);
        assert_eq!(resp.payload, vec![0xCA, 0xFE]);
    }

    #[test]
    fn response_to_bytes_roundtrip() {
        let mut resp = SoxResponse::success(SoxCmd::Write, 42);
        resp.write_u16(0x0010).write_u8(3);
        let bytes = resp.to_bytes();
        assert_eq!(bytes[0], b'W'); // uppercase response
        assert_eq!(bytes[1], 42); // req_id
        assert_eq!(&bytes[2..], &[0x00, 0x10, 3]); // payload
    }

    #[test]
    fn response_chained_writes() {
        let mut resp = SoxResponse::success(SoxCmd::ReadComp, 1);
        resp.write_u16(0x0042)
            .write_u8(b't')
            .write_str("App")
            .write_f32(72.5);
        let bytes = resp.to_bytes();
        assert_eq!(bytes[0], b'C');
        assert_eq!(bytes[1], 1);
        // u16(0x0042) + u8('t') + str("App") + f32(72.5)
        assert_eq!(bytes[2], 0x00);
        assert_eq!(bytes[3], 0x42);
        assert_eq!(bytes[4], b't');
        assert_eq!(&bytes[5..8], b"App");
        assert_eq!(bytes[8], 0x00); // NUL terminator
        // 72.5f = 0x42910000
        assert_eq!(&bytes[9..13], &[0x42, 0x91, 0x00, 0x00]);
    }

    // -----------------------------------------------------------------------
    // SoxReader
    // -----------------------------------------------------------------------

    #[test]
    fn reader_read_u8() {
        let mut r = SoxReader::new(&[0xAB, 0xCD]);
        assert_eq!(r.read_u8(), Some(0xAB));
        assert_eq!(r.read_u8(), Some(0xCD));
        assert_eq!(r.read_u8(), None);
    }

    #[test]
    fn reader_read_u16() {
        let mut r = SoxReader::new(&[0x12, 0x34]);
        assert_eq!(r.read_u16(), Some(0x1234));
        assert_eq!(r.read_u16(), None);
    }

    #[test]
    fn reader_read_u32() {
        let mut r = SoxReader::new(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(r.read_u32(), Some(0xDEADBEEF));
    }

    #[test]
    fn reader_read_i32() {
        let mut r = SoxReader::new(&[0xFF, 0xFF, 0xFF, 0xFE]);
        assert_eq!(r.read_i32(), Some(-2));
    }

    #[test]
    fn reader_read_f32() {
        let mut r = SoxReader::new(&[0x3F, 0x80, 0x00, 0x00]);
        let v = r.read_f32().unwrap();
        assert!((v - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn reader_read_f64() {
        let mut r = SoxReader::new(&[0x3F, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        let v = r.read_f64().unwrap();
        assert!((v - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn reader_read_str() {
        // len=5 + "hello"
        let data = [b'h', b'e', b'l', b'l', b'o', 0x00];
        let mut r = SoxReader::new(&data);
        assert_eq!(r.read_str(), Some("hello".to_string()));
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn reader_read_str_empty() {
        let data = [0x00];
        let mut r = SoxReader::new(&data);
        assert_eq!(r.read_str(), Some(String::new()));
    }

    #[test]
    fn reader_read_str_no_nul_returns_none() {
        // no null terminator
        let data = [b'a', b'b', b'c'];
        let mut r = SoxReader::new(&data);
        assert_eq!(r.read_str(), None);
    }

    #[test]
    fn reader_read_bytes() {
        let data = [0xCA, 0xFE, 0xBA, 0xBE];
        let mut r = SoxReader::new(&data);
        assert_eq!(r.read_bytes(2), Some(&[0xCA, 0xFE][..]));
        assert_eq!(r.read_bytes(2), Some(&[0xBA, 0xBE][..]));
        assert_eq!(r.read_bytes(1), None);
    }

    #[test]
    fn reader_remaining_tracks_position() {
        let data = [1, 2, 3, 4, 5];
        let mut r = SoxReader::new(&data);
        assert_eq!(r.remaining(), 5);
        r.read_u8();
        assert_eq!(r.remaining(), 4);
        r.read_u16();
        assert_eq!(r.remaining(), 2);
        r.read_u16();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn reader_exhaustion_returns_none_all_types() {
        let mut r = SoxReader::new(&[]);
        assert_eq!(r.read_u8(), None);
        assert_eq!(r.read_u16(), None);
        assert_eq!(r.read_u32(), None);
        assert_eq!(r.read_i32(), None);
        assert_eq!(r.read_f32(), None);
        assert_eq!(r.read_f64(), None);
        assert_eq!(r.read_str(), None);
        assert_eq!(r.read_bytes(1), None);
    }

    #[test]
    fn reader_partial_u16_returns_none() {
        let mut r = SoxReader::new(&[0x12]);
        assert_eq!(r.read_u16(), None);
    }

    #[test]
    fn reader_partial_u32_returns_none() {
        let mut r = SoxReader::new(&[0x12, 0x34, 0x56]);
        assert_eq!(r.read_u32(), None);
    }

    #[test]
    fn reader_partial_f64_returns_none() {
        let mut r = SoxReader::new(&[0; 7]);
        assert_eq!(r.read_f64(), None);
    }

    // -----------------------------------------------------------------------
    // Roundtrip: Response builder → Reader
    // -----------------------------------------------------------------------

    #[test]
    fn roundtrip_response_to_reader() {
        let mut resp = SoxResponse::success(SoxCmd::ReadComp, 10);
        resp.write_u16(0x0042)
            .write_u8(5)
            .write_str("Zone1")
            .write_f32(72.5)
            .write_u32(0xAABBCCDD);

        let bytes = resp.to_bytes();
        assert_eq!(bytes[0], b'C');
        assert_eq!(bytes[1], 10);

        // Parse back the payload
        let mut r = SoxReader::new(&bytes[2..]);
        assert_eq!(r.read_u16(), Some(0x0042));
        assert_eq!(r.read_u8(), Some(5));
        assert_eq!(r.read_str(), Some("Zone1".to_string()));
        let f = r.read_f32().unwrap();
        assert!((f - 72.5).abs() < f32::EPSILON);
        assert_eq!(r.read_u32(), Some(0xAABBCCDD));
        assert_eq!(r.remaining(), 0);
    }
}
