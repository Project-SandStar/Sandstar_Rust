//! DASP (Datagram Authenticated Session Protocol) transport layer.
//!
//! Implements the DASP session protocol used by Sedona Application Editor
//! and other SOX clients to communicate with Sedona devices over UDP.
//!
//! Wire format (per `DaspMsg.java`):
//! ```text
//! Byte 0-1: Session ID (u16 big-endian)
//! Byte 2-3: Sequence number (u16 big-endian)
//! Byte 4:   (msgType << 4) | numHeaderFields
//! [Header fields: id_byte, then value per type encoded in low 2 bits of id]
//! [Payload bytes]
//! ```
//!
//! Header field types (low 2 bits of field ID):
//! - 0 (NIL):  no value
//! - 1 (U2):   2-byte big-endian unsigned
//! - 2 (STR):  null-terminated UTF-8 string
//! - 3 (BYTES): 1-byte length prefix + bytes

use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};
use tracing::{debug, info, trace, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default DASP/SOX UDP port.
pub const DASP_DEFAULT_PORT: u16 = 1876;

/// Maximum concurrent DASP sessions (embedded device limit).
pub const MAX_SESSIONS: usize = 4;

/// Session timeout in seconds — if no message received within this window
/// the session is considered dead.
pub const SESSION_TIMEOUT_SECS: u64 = 60;

/// DASP protocol version 1.0.
pub const DASP_VERSION: u16 = 0x0100;

/// Ideal max packet size (bytes).
pub const IDEAL_MAX: u16 = 512;

/// Absolute max packet size (bytes).
pub const ABS_MAX: u16 = 512;

/// Receive window size.
pub const RECEIVE_MAX: u16 = 31;

/// Receive timeout in seconds (encoded on wire).
pub const RECEIVE_TIMEOUT_SECS: u16 = 30;

/// Platform identifier returned in discovery responses.
pub const PLATFORM_ID: &str = "sandstar-rust";

/// Max UDP receive buffer.
const RECV_BUF_SIZE: usize = 1500;

// ---------------------------------------------------------------------------
// Message Types
// ---------------------------------------------------------------------------

/// DASP message types per the specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DaspMsgType {
    Discover = 0,
    Hello = 1,
    Challenge = 2,
    Authenticate = 3,
    Welcome = 4,
    Keepalive = 5,
    Datagram = 6,
    Close = 7,
}

impl DaspMsgType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Discover),
            1 => Some(Self::Hello),
            2 => Some(Self::Challenge),
            3 => Some(Self::Authenticate),
            4 => Some(Self::Welcome),
            5 => Some(Self::Keepalive),
            6 => Some(Self::Datagram),
            7 => Some(Self::Close),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Header Field IDs (from DaspConst.java)
// ---------------------------------------------------------------------------

/// The low 2 bits encode the value type: 0=NIL, 1=U2, 2=STR, 3=BYTES.
mod field_id {
    pub const VERSION: u8 = 0x05;           // (1, U2)
    pub const REMOTE_ID: u8 = 0x09;         // (2, U2)
    pub const DIGEST_ALGORITHM: u8 = 0x0e;  // (3, STR)
    pub const NONCE: u8 = 0x13;             // (4, BYTES)
    pub const USERNAME: u8 = 0x16;          // (5, STR)
    pub const DIGEST: u8 = 0x1b;            // (6, BYTES)
    pub const IDEAL_MAX: u8 = 0x1d;         // (7, U2)
    pub const ABS_MAX: u8 = 0x21;           // (8, U2)
    pub const ACK: u8 = 0x25;               // (9, U2)
    pub const ACK_MORE: u8 = 0x2b;          // (a, BYTES)
    pub const RECEIVE_MAX: u8 = 0x2d;       // (b, U2)
    pub const RECEIVE_TIMEOUT: u8 = 0x31;   // (c, U2)
    pub const ERROR_CODE: u8 = 0x35;        // (d, U2)
    pub const PLATFORM_ID: u8 = 0x3a;       // (e, STR)
}

/// Error codes per the DASP specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum DaspErrorCode {
    IncompatibleVersion = 0xe1,
    Busy = 0xe2,
    DigestNotSupported = 0xe3,
    NotAuthenticated = 0xe4,
    Timeout = 0xe5,
}

// ---------------------------------------------------------------------------
// Parsed DASP Header
// ---------------------------------------------------------------------------

/// Parsed DASP message with all possible header fields.
#[derive(Debug, Clone)]
pub struct DaspHeader {
    pub session_id: u16,
    pub seq_num: u16,
    pub msg_type: DaspMsgType,
    // Optional header fields
    pub version: Option<u16>,
    pub remote_id: Option<u16>,
    pub digest_algorithm: Option<String>,
    pub nonce: Option<Vec<u8>>,
    pub username: Option<String>,
    pub digest: Option<Vec<u8>>,
    pub ideal_max: Option<u16>,
    pub abs_max: Option<u16>,
    pub ack: Option<u16>,
    pub ack_more: Option<Vec<u8>>,
    pub receive_max: Option<u16>,
    pub receive_timeout_secs: Option<u16>,
    pub error_code: Option<u16>,
    pub platform_id: Option<String>,
    /// Offset in original buffer where payload begins.
    pub payload_offset: usize,
}

// ---------------------------------------------------------------------------
// Encode / Decode
// ---------------------------------------------------------------------------

/// Parse a raw UDP packet into a `DaspHeader`.
///
/// Returns `None` if the packet is too short or the message type is unknown.
pub fn parse_header(data: &[u8]) -> Option<DaspHeader> {
    if data.len() < 5 {
        return None;
    }

    let session_id = u16::from_be_bytes([data[0], data[1]]);
    let seq_num = u16::from_be_bytes([data[2], data[3]]);
    let type_fields = data[4];
    let msg_type_raw = type_fields >> 4;
    let num_fields = (type_fields & 0x0f) as usize;

    let msg_type = DaspMsgType::from_u8(msg_type_raw)?;

    let mut hdr = DaspHeader {
        session_id,
        seq_num,
        msg_type,
        version: None,
        remote_id: None,
        digest_algorithm: None,
        nonce: None,
        username: None,
        digest: None,
        ideal_max: None,
        abs_max: None,
        ack: None,
        ack_more: None,
        receive_max: None,
        receive_timeout_secs: None,
        error_code: None,
        platform_id: None,
        payload_offset: 0,
    };

    let mut pos = 5usize;
    for _ in 0..num_fields {
        if pos >= data.len() {
            return None;
        }
        let id = data[pos];
        pos += 1;
        let field_type = id & 0x03;

        match field_type {
            0 => {
                // NIL — no value
            }
            1 => {
                // U2
                if pos + 2 > data.len() {
                    return None;
                }
                let val = u16::from_be_bytes([data[pos], data[pos + 1]]);
                pos += 2;
                match id {
                    field_id::VERSION => hdr.version = Some(val),
                    field_id::REMOTE_ID => hdr.remote_id = Some(val),
                    field_id::IDEAL_MAX => hdr.ideal_max = Some(val),
                    field_id::ABS_MAX => hdr.abs_max = Some(val),
                    field_id::ACK => hdr.ack = Some(val),
                    field_id::RECEIVE_MAX => hdr.receive_max = Some(val),
                    field_id::RECEIVE_TIMEOUT => hdr.receive_timeout_secs = Some(val),
                    field_id::ERROR_CODE => hdr.error_code = Some(val),
                    _ => { /* unknown U2 field — skip */ }
                }
            }
            2 => {
                // STR (null-terminated)
                let start = pos;
                while pos < data.len() && data[pos] != 0 {
                    pos += 1;
                }
                if pos >= data.len() {
                    return None; // missing null terminator
                }
                let s = String::from_utf8_lossy(&data[start..pos]).into_owned();
                pos += 1; // skip null terminator
                match id {
                    field_id::DIGEST_ALGORITHM => hdr.digest_algorithm = Some(s),
                    field_id::USERNAME => hdr.username = Some(s),
                    field_id::PLATFORM_ID => hdr.platform_id = Some(s),
                    _ => { /* unknown STR field — skip */ }
                }
            }
            3 => {
                // BYTES (length-prefixed)
                if pos >= data.len() {
                    return None;
                }
                let blen = data[pos] as usize;
                pos += 1;
                if pos + blen > data.len() {
                    return None;
                }
                let bytes = data[pos..pos + blen].to_vec();
                pos += blen;
                match id {
                    field_id::NONCE => hdr.nonce = Some(bytes),
                    field_id::DIGEST => hdr.digest = Some(bytes),
                    field_id::ACK_MORE => hdr.ack_more = Some(bytes),
                    _ => { /* unknown BYTES field — skip */ }
                }
            }
            _ => unreachable!(),
        }
    }

    hdr.payload_offset = pos;
    Some(hdr)
}

/// Encode a DASP message into a buffer.  Returns the number of bytes written.
///
/// The caller provides optional header fields; only non-`None` fields are
/// encoded.  `payload` is appended after the headers.
pub fn encode_message(hdr: &DaspHeader, payload: &[u8], buf: &mut Vec<u8>) {
    buf.clear();

    // Session ID
    buf.extend_from_slice(&hdr.session_id.to_be_bytes());
    // Sequence number
    buf.extend_from_slice(&hdr.seq_num.to_be_bytes());
    // Placeholder for (msgType << 4) | numFields
    buf.push(0);

    let mut num_fields: u8 = 0;

    // Helper closures replaced by inline encoding to keep borrow checker happy.

    if let Some(v) = hdr.version {
        num_fields += 1;
        buf.push(field_id::VERSION);
        buf.extend_from_slice(&v.to_be_bytes());
    }
    if let Some(v) = hdr.remote_id {
        num_fields += 1;
        buf.push(field_id::REMOTE_ID);
        buf.extend_from_slice(&v.to_be_bytes());
    }
    if let Some(ref s) = hdr.digest_algorithm {
        num_fields += 1;
        buf.push(field_id::DIGEST_ALGORITHM);
        buf.extend_from_slice(s.as_bytes());
        buf.push(0); // null terminator
    }
    if let Some(ref b) = hdr.nonce {
        num_fields += 1;
        buf.push(field_id::NONCE);
        buf.push(b.len() as u8);
        buf.extend_from_slice(b);
    }
    if let Some(ref s) = hdr.username {
        num_fields += 1;
        buf.push(field_id::USERNAME);
        buf.extend_from_slice(s.as_bytes());
        buf.push(0);
    }
    if let Some(ref b) = hdr.digest {
        num_fields += 1;
        buf.push(field_id::DIGEST);
        buf.push(b.len() as u8);
        buf.extend_from_slice(b);
    }
    if let Some(v) = hdr.ideal_max {
        if v != IDEAL_MAX {
            num_fields += 1;
            buf.push(field_id::IDEAL_MAX);
            buf.extend_from_slice(&v.to_be_bytes());
        }
    }
    if let Some(v) = hdr.abs_max {
        if v != ABS_MAX {
            num_fields += 1;
            buf.push(field_id::ABS_MAX);
            buf.extend_from_slice(&v.to_be_bytes());
        }
    }
    if let Some(v) = hdr.ack {
        num_fields += 1;
        buf.push(field_id::ACK);
        buf.extend_from_slice(&v.to_be_bytes());
    }
    if let Some(ref b) = hdr.ack_more {
        num_fields += 1;
        buf.push(field_id::ACK_MORE);
        buf.push(b.len() as u8);
        buf.extend_from_slice(b);
    }
    if let Some(v) = hdr.receive_max {
        if v != RECEIVE_MAX {
            num_fields += 1;
            buf.push(field_id::RECEIVE_MAX);
            buf.extend_from_slice(&v.to_be_bytes());
        }
    }
    if let Some(v) = hdr.receive_timeout_secs {
        if v != RECEIVE_TIMEOUT_SECS {
            num_fields += 1;
            buf.push(field_id::RECEIVE_TIMEOUT);
            buf.extend_from_slice(&v.to_be_bytes());
        }
    }
    if let Some(v) = hdr.error_code {
        num_fields += 1;
        buf.push(field_id::ERROR_CODE);
        buf.extend_from_slice(&v.to_be_bytes());
    }
    if let Some(ref s) = hdr.platform_id {
        num_fields += 1;
        buf.push(field_id::PLATFORM_ID);
        buf.extend_from_slice(s.as_bytes());
        buf.push(0);
    }

    // Back-patch byte 4
    buf[4] = ((hdr.msg_type as u8) << 4) | (num_fields & 0x0f);

    // Payload
    buf.extend_from_slice(payload);
}

// ---------------------------------------------------------------------------
// SHA-1 Authentication
// ---------------------------------------------------------------------------

/// Compute the server-side expected digest for DASP authentication.
///
/// The client sends: `SHA1( SHA1(username + ":" + password) + nonce )`
///
/// The server stores `credentials = SHA1(username + ":" + password)` and
/// computes `SHA1(credentials + nonce)` to compare.
///
/// Per the Java source (`DaspSession.java` lines 373-378):
/// ```java
/// byte[] cred = md.digest((user + ":" + pass).getBytes("UTF-8"));
/// md.reset();
/// md.update(cred);
/// md.update(challenge.nonce());
/// byte[] digest = md.digest();
/// ```
pub fn compute_credentials(username: &str, password: &str) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(format!("{username}:{password}").as_bytes());
    hasher.finalize().into()
}

pub fn compute_auth_digest(credentials: &[u8; 20], nonce: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(credentials);
    hasher.update(nonce);
    hasher.finalize().into()
}

// ---------------------------------------------------------------------------
// DASP Session
// ---------------------------------------------------------------------------

/// A DASP session with a connected client.
pub struct DaspSession {
    pub session_id: u16,
    pub remote_id: u16,
    pub remote_addr: SocketAddr,
    pub authenticated: bool,
    pub send_seq: u16,
    pub recv_seq: u16,
    pub last_activity: Instant,
    pub nonce: Vec<u8>,
    /// Tuned ideal max (minimum of ours and client's).
    pub ideal_max: u16,
    /// Tuned absolute max.
    pub abs_max: u16,
}

impl DaspSession {
    fn new(session_id: u16, remote_id: u16, remote_addr: SocketAddr, nonce: Vec<u8>) -> Self {
        Self {
            session_id,
            remote_id,
            remote_addr,
            authenticated: false,
            send_seq: 0,
            recv_seq: 0,
            last_activity: Instant::now(),
            nonce,
            ideal_max: IDEAL_MAX,
            abs_max: ABS_MAX,
        }
    }

    fn next_seq(&mut self) -> u16 {
        let s = self.send_seq;
        self.send_seq = self.send_seq.wrapping_add(1);
        s
    }

    /// Returns true if the session has exceeded the timeout.
    pub fn is_expired(&self) -> bool {
        self.last_activity.elapsed() > Duration::from_secs(SESSION_TIMEOUT_SECS)
    }

    fn touch(&mut self) {
        self.last_activity = Instant::now();
    }
}

// ---------------------------------------------------------------------------
// DASP Transport
// ---------------------------------------------------------------------------

/// DASP transport — manages a UDP socket and sessions.
pub struct DaspTransport {
    socket: UdpSocket,
    sessions: HashMap<u16, DaspSession>,
    /// Maps remote SocketAddr to session_id for fast lookup during handshake.
    addr_to_session: HashMap<SocketAddr, u16>,
    next_session_id: u16,
    credentials: [u8; 20],
    /// Scratch buffer for encoding outgoing messages.
    encode_buf: Vec<u8>,
}

impl DaspTransport {
    /// Create a new DASP transport bound to the given port.
    ///
    /// The socket is set to non-blocking mode so `poll()` never blocks.
    pub fn bind(port: u16, username: &str, password: &str) -> std::io::Result<Self> {
        let addr = format!("0.0.0.0:{port}");
        let socket = UdpSocket::bind(&addr)?;
        socket.set_nonblocking(true)?;

        let credentials = compute_credentials(username, password);

        info!(port, "DASP transport bound");

        Ok(Self {
            socket,
            sessions: HashMap::new(),
            addr_to_session: HashMap::new(),
            next_session_id: 1,
            credentials,
            encode_buf: Vec::with_capacity(RECV_BUF_SIZE),
        })
    }

    /// Create a DASP transport from an existing `UdpSocket`.
    ///
    /// Useful for testing — the caller provides an already-bound socket.
    pub fn from_socket(socket: UdpSocket, username: &str, password: &str) -> std::io::Result<Self> {
        socket.set_nonblocking(true)?;
        let credentials = compute_credentials(username, password);
        Ok(Self {
            socket,
            sessions: HashMap::new(),
            addr_to_session: HashMap::new(),
            next_session_id: 1,
            credentials,
            encode_buf: Vec::with_capacity(RECV_BUF_SIZE),
        })
    }

    /// Return the local address the socket is bound to.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Number of active sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Get a reference to a session by ID.
    pub fn session(&self, id: u16) -> Option<&DaspSession> {
        self.sessions.get(&id)
    }

    /// Process one incoming UDP packet.
    ///
    /// Returns `Some((session_id, payload))` if the packet is an authenticated
    /// DATAGRAM containing a SOX payload.  Returns `None` for handshake
    /// messages, keepalives, errors, or if no packet is available.
    pub fn poll(&mut self) -> Option<(u16, Vec<u8>)> {
        let mut recv_buf = [0u8; RECV_BUF_SIZE];
        let (len, from) = match self.socket.recv_from(&mut recv_buf) {
            Ok(v) => v,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return None;
            }
            Err(e) => {
                warn!("DASP recv error: {e}");
                return None;
            }
        };

        let data = &recv_buf[..len];
        let hdr = match parse_header(data) {
            Some(h) => h,
            None => {
                debug!("DASP: failed to parse header from {from}");
                return None;
            }
        };

        trace!(
            "DASP recv from {from}: type={:?} session=0x{:04x} seq={}",
            hdr.msg_type,
            hdr.session_id,
            hdr.seq_num
        );

        match hdr.msg_type {
            DaspMsgType::Discover => {
                self.handle_discover(from);
                None
            }
            DaspMsgType::Hello => {
                if let Err(e) = self.handle_hello(from, &hdr) {
                    warn!("DASP: error handling HELLO from {from}: {e}");
                }
                None
            }
            DaspMsgType::Authenticate => {
                if let Err(e) = self.handle_authenticate(from, &hdr) {
                    warn!("DASP: error handling AUTHENTICATE from {from}: {e}");
                }
                None
            }
            DaspMsgType::Keepalive => {
                self.handle_keepalive(hdr.session_id);
                None
            }
            DaspMsgType::Close => {
                self.handle_close(hdr.session_id);
                None
            }
            DaspMsgType::Datagram => {
                // Verify session exists and is authenticated
                if let Some(session) = self.sessions.get_mut(&hdr.session_id) {
                    if !session.authenticated {
                        debug!("DASP: datagram for unauthenticated session 0x{:04x}", hdr.session_id);
                        return None;
                    }
                    if session.remote_addr != from {
                        debug!("DASP: address mismatch for session 0x{:04x}", hdr.session_id);
                        return None;
                    }
                    session.touch();
                    session.recv_seq = hdr.seq_num;

                    // Send ACK
                    let _ = self.send_ack(hdr.session_id, hdr.seq_num);

                    let payload = data[hdr.payload_offset..].to_vec();
                    if payload.is_empty() {
                        return None;
                    }
                    Some((hdr.session_id, payload))
                } else {
                    debug!("DASP: datagram for unknown session 0x{:04x}", hdr.session_id);
                    None
                }
            }
            DaspMsgType::Challenge | DaspMsgType::Welcome => {
                // These are client-side responses; server should not receive them.
                debug!("DASP: unexpected {:?} from {from}", hdr.msg_type);
                None
            }
        }
    }

    /// Send a SOX response payload to a session as a DATAGRAM.
    pub fn send_to_session(&mut self, session_id: u16, payload: &[u8]) -> std::io::Result<()> {
        let (remote_addr, remote_id, seq) = {
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "session not found"))?;
            let seq = session.next_seq();
            (session.remote_addr, session.remote_id, seq)
        };

        let hdr = DaspHeader {
            session_id: remote_id,
            seq_num: seq,
            msg_type: DaspMsgType::Datagram,
            version: None,
            remote_id: None,
            digest_algorithm: None,
            nonce: None,
            username: None,
            digest: None,
            ideal_max: None,
            abs_max: None,
            ack: None,
            ack_more: None,
            receive_max: None,
            receive_timeout_secs: None,
            error_code: None,
            platform_id: None,
            payload_offset: 0,
        };

        encode_message(&hdr, payload, &mut self.encode_buf);
        self.socket.send_to(&self.encode_buf, remote_addr)?;
        Ok(())
    }

    /// Send a COV event to a session (same as send_to_session but semantically distinct).
    pub fn send_event(&mut self, session_id: u16, payload: &[u8]) -> std::io::Result<()> {
        self.send_to_session(session_id, payload)
    }

    /// Remove sessions that have not had any activity within the timeout.
    pub fn cleanup_expired(&mut self) {
        let expired: Vec<u16> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.is_expired())
            .map(|(id, _)| *id)
            .collect();

        for id in expired {
            info!("DASP: expiring session 0x{id:04x}");
            if let Some(session) = self.sessions.remove(&id) {
                self.addr_to_session.remove(&session.remote_addr);
                // Send CLOSE with TIMEOUT error code
                let _ = self.send_close(session.remote_id, session.remote_addr, Some(DaspErrorCode::Timeout));
            }
        }
    }

    /// Return the IDs of sessions that have expired (no activity within timeout).
    ///
    /// This is useful for cleaning up subscription state before calling
    /// `cleanup_expired()` which removes the sessions from the transport.
    pub fn expired_session_ids(&self) -> Vec<u16> {
        self.sessions
            .iter()
            .filter(|(_, s)| s.is_expired())
            .map(|(id, _)| *id)
            .collect()
    }

    /// Return list of authenticated session IDs.
    pub fn authenticated_sessions(&self) -> Vec<u16> {
        self.sessions
            .iter()
            .filter(|(_, s)| s.authenticated)
            .map(|(id, _)| *id)
            .collect()
    }

    // -----------------------------------------------------------------------
    // Handshake handlers
    // -----------------------------------------------------------------------

    fn handle_discover(&mut self, from: SocketAddr) {
        debug!("DASP: discover from {from}");
        // Respond with a discover message containing our platform ID.
        let hdr = DaspHeader {
            session_id: 0xffff,
            seq_num: 0,
            msg_type: DaspMsgType::Discover,
            version: None,
            remote_id: None,
            digest_algorithm: None,
            nonce: None,
            username: None,
            digest: None,
            ideal_max: None,
            abs_max: None,
            ack: None,
            ack_more: None,
            receive_max: None,
            receive_timeout_secs: None,
            error_code: None,
            platform_id: Some(PLATFORM_ID.to_string()),
            payload_offset: 0,
        };
        encode_message(&hdr, &[], &mut self.encode_buf);
        let _ = self.socket.send_to(&self.encode_buf, from);
    }

    fn handle_hello(&mut self, from: SocketAddr, hello: &DaspHeader) -> std::io::Result<()> {
        // Check version
        if let Some(v) = hello.version {
            if v != DASP_VERSION {
                warn!("DASP: incompatible version 0x{v:04x} from {from}");
                self.send_close(hello.remote_id.unwrap_or(0xffff), from, Some(DaspErrorCode::IncompatibleVersion))?;
                return Ok(());
            }
        }

        // Check session limit
        if self.sessions.len() >= MAX_SESSIONS {
            warn!("DASP: too many sessions, rejecting HELLO from {from}");
            self.send_close(hello.remote_id.unwrap_or(0xffff), from, Some(DaspErrorCode::Busy))?;
            return Ok(());
        }

        // If there's already a pending session from this address, remove it
        if let Some(old_id) = self.addr_to_session.remove(&from) {
            self.sessions.remove(&old_id);
        }

        // Allocate session ID
        let session_id = self.alloc_session_id();
        let remote_id = hello.remote_id.unwrap_or(0);

        // Generate nonce (10 bytes like Java implementation)
        let nonce = generate_nonce();

        // Tune parameters
        let mut session = DaspSession::new(session_id, remote_id, from, nonce.clone());
        if let Some(im) = hello.ideal_max {
            session.ideal_max = session.ideal_max.min(im);
        }
        if let Some(am) = hello.abs_max {
            session.abs_max = session.abs_max.min(am);
        }
        // Record initial recv_seq from client's hello
        session.recv_seq = hello.seq_num;

        info!(
            "DASP: HELLO from {from}, assigned session 0x{session_id:04x}, remote 0x{remote_id:04x}"
        );

        self.sessions.insert(session_id, session);
        self.addr_to_session.insert(from, session_id);

        // Send CHALLENGE
        let chal = DaspHeader {
            session_id: remote_id,
            seq_num: 0,
            msg_type: DaspMsgType::Challenge,
            version: None,
            remote_id: Some(session_id),
            digest_algorithm: None, // default is SHA-1
            nonce: Some(nonce),
            username: None,
            digest: None,
            ideal_max: None,
            abs_max: None,
            ack: None,
            ack_more: None,
            receive_max: None,
            receive_timeout_secs: None,
            error_code: None,
            platform_id: None,
            payload_offset: 0,
        };
        encode_message(&chal, &[], &mut self.encode_buf);
        self.socket.send_to(&self.encode_buf, from)?;

        Ok(())
    }

    fn handle_authenticate(&mut self, from: SocketAddr, auth: &DaspHeader) -> std::io::Result<()> {
        // The authenticate message is addressed to our session ID
        let session_id = auth.session_id;
        let session = match self.sessions.get(&session_id) {
            Some(s) => s,
            None => {
                debug!("DASP: AUTHENTICATE for unknown session 0x{session_id:04x}");
                return Ok(());
            }
        };

        if session.remote_addr != from {
            debug!("DASP: AUTHENTICATE address mismatch for session 0x{session_id:04x}");
            return Ok(());
        }

        // Verify digest
        let expected = compute_auth_digest(&self.credentials, &session.nonce);
        let actual = auth.digest.as_deref().unwrap_or(&[]);

        if actual.len() != 20 || actual != expected.as_slice() {
            warn!("DASP: authentication failed for session 0x{session_id:04x} from {from}");
            let remote_id = session.remote_id;
            self.sessions.remove(&session_id);
            self.addr_to_session.remove(&from);
            self.send_close(remote_id, from, Some(DaspErrorCode::NotAuthenticated))?;
            return Ok(());
        }

        // Authentication succeeded
        let session = self.sessions.get_mut(&session_id).expect("session must exist");
        session.authenticated = true;
        session.touch();

        let remote_id = session.remote_id;

        info!(
            "DASP: session 0x{session_id:04x} authenticated (user={:?})",
            auth.username
        );

        // Send WELCOME
        let welcome = DaspHeader {
            session_id: remote_id,
            seq_num: 0,
            msg_type: DaspMsgType::Welcome,
            version: None,
            remote_id: Some(session_id),
            digest_algorithm: None,
            nonce: None,
            username: None,
            digest: None,
            ideal_max: Some(session.ideal_max),
            abs_max: Some(session.abs_max),
            ack: None,
            ack_more: None,
            receive_max: Some(RECEIVE_MAX),
            receive_timeout_secs: Some(RECEIVE_TIMEOUT_SECS),
            error_code: None,
            platform_id: None,
            payload_offset: 0,
        };
        encode_message(&welcome, &[], &mut self.encode_buf);
        self.socket.send_to(&self.encode_buf, from)?;

        Ok(())
    }

    fn handle_keepalive(&mut self, session_id: u16) {
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.touch();
            trace!("DASP: keepalive for session 0x{session_id:04x}");

            // Send keepalive response (same message type)
            let resp = DaspHeader {
                session_id: session.remote_id,
                seq_num: 0xffff,
                msg_type: DaspMsgType::Keepalive,
                version: None,
                remote_id: None,
                digest_algorithm: None,
                nonce: None,
                username: None,
                digest: None,
                ideal_max: None,
                abs_max: None,
                ack: None,
                ack_more: None,
                receive_max: None,
                receive_timeout_secs: None,
                error_code: None,
                platform_id: None,
                payload_offset: 0,
            };
            let addr = session.remote_addr;
            encode_message(&resp, &[], &mut self.encode_buf);
            let _ = self.socket.send_to(&self.encode_buf, addr);
        } else {
            debug!("DASP: keepalive for unknown session 0x{session_id:04x}");
        }
    }

    fn handle_close(&mut self, session_id: u16) {
        if let Some(session) = self.sessions.remove(&session_id) {
            info!("DASP: session 0x{session_id:04x} closed by client");
            self.addr_to_session.remove(&session.remote_addr);
        } else {
            debug!("DASP: close for unknown session 0x{session_id:04x}");
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn send_close(
        &mut self,
        remote_id: u16,
        addr: SocketAddr,
        error_code: Option<DaspErrorCode>,
    ) -> std::io::Result<()> {
        let hdr = DaspHeader {
            session_id: remote_id,
            seq_num: 0xffff,
            msg_type: DaspMsgType::Close,
            version: None,
            remote_id: None,
            digest_algorithm: None,
            nonce: None,
            username: None,
            digest: None,
            ideal_max: None,
            abs_max: None,
            ack: None,
            ack_more: None,
            receive_max: None,
            receive_timeout_secs: None,
            error_code: error_code.map(|e| e as u16),
            platform_id: None,
            payload_offset: 0,
        };
        encode_message(&hdr, &[], &mut self.encode_buf);
        self.socket.send_to(&self.encode_buf, addr)?;
        Ok(())
    }

    fn send_ack(&mut self, session_id: u16, ack_seq: u16) -> std::io::Result<()> {
        let session = match self.sessions.get(&session_id) {
            Some(s) => s,
            None => return Ok(()),
        };
        let remote_id = session.remote_id;
        let addr = session.remote_addr;

        let hdr = DaspHeader {
            session_id: remote_id,
            seq_num: 0xffff,
            msg_type: DaspMsgType::Datagram,
            version: None,
            remote_id: None,
            digest_algorithm: None,
            nonce: None,
            username: None,
            digest: None,
            ideal_max: None,
            abs_max: None,
            ack: Some(ack_seq),
            ack_more: None,
            receive_max: None,
            receive_timeout_secs: None,
            error_code: None,
            platform_id: None,
            payload_offset: 0,
        };
        encode_message(&hdr, &[], &mut self.encode_buf);
        self.socket.send_to(&self.encode_buf, addr)?;
        Ok(())
    }

    fn alloc_session_id(&mut self) -> u16 {
        loop {
            let id = self.next_session_id;
            self.next_session_id = self.next_session_id.wrapping_add(1);
            // 0xffff is reserved for "no session" in the DASP spec
            if id == 0xffff {
                continue;
            }
            if !self.sessions.contains_key(&id) {
                return id;
            }
        }
    }
}

/// Generate a random 10-byte nonce (matches Java implementation size).
fn generate_nonce() -> Vec<u8> {
    use rand::Rng;
    let mut rng = rand::rng();
    let mut nonce = vec![0u8; 10];
    rng.fill(&mut nonce[..]);
    nonce
}

// ---------------------------------------------------------------------------
// Helper to build a default (empty) DaspHeader for a given message type
// ---------------------------------------------------------------------------

/// Build a DaspHeader with all optional fields set to None.
pub fn empty_header(msg_type: DaspMsgType, session_id: u16, seq_num: u16) -> DaspHeader {
    DaspHeader {
        session_id,
        seq_num,
        msg_type,
        version: None,
        remote_id: None,
        digest_algorithm: None,
        nonce: None,
        username: None,
        digest: None,
        ideal_max: None,
        abs_max: None,
        ack: None,
        ack_more: None,
        receive_max: None,
        receive_timeout_secs: None,
        error_code: None,
        platform_id: None,
        payload_offset: 0,
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;

    /// Bind a DaspTransport to 127.0.0.1:0 for testing (avoids Windows
    /// issues with sending to 0.0.0.0).
    fn test_transport(user: &str, pass: &str) -> DaspTransport {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind test socket");
        DaspTransport::from_socket(socket, user, pass).expect("from_socket")
    }

    // -----------------------------------------------------------------------
    // 1. DaspTransport::bind succeeds on available port
    // -----------------------------------------------------------------------
    #[test]
    fn test_bind_succeeds() {
        let transport = test_transport("admin", "pass");
        let addr = transport.local_addr().expect("local_addr");
        assert_ne!(addr.port(), 0);
    }

    // -----------------------------------------------------------------------
    // 2. parse_header — valid HELLO
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_hello() {
        // Build a HELLO message manually
        let mut hdr = empty_header(DaspMsgType::Hello, 0xffff, 0x0000);
        hdr.remote_id = Some(0x1234);
        hdr.version = Some(DASP_VERSION);

        let mut buf = Vec::new();
        encode_message(&hdr, &[], &mut buf);

        let parsed = parse_header(&buf).expect("parse should succeed");
        assert_eq!(parsed.msg_type, DaspMsgType::Hello);
        assert_eq!(parsed.session_id, 0xffff);
        assert_eq!(parsed.seq_num, 0x0000);
        assert_eq!(parsed.version, Some(DASP_VERSION));
        assert_eq!(parsed.remote_id, Some(0x1234));
    }

    // -----------------------------------------------------------------------
    // 3. parse_header — valid DATAGRAM with ack
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_datagram_with_ack() {
        let mut hdr = empty_header(DaspMsgType::Datagram, 0x0042, 0x0007);
        hdr.ack = Some(0x0005);

        let payload = b"hello sox";
        let mut buf = Vec::new();
        encode_message(&hdr, payload, &mut buf);

        let parsed = parse_header(&buf).expect("parse should succeed");
        assert_eq!(parsed.msg_type, DaspMsgType::Datagram);
        assert_eq!(parsed.session_id, 0x0042);
        assert_eq!(parsed.seq_num, 0x0007);
        assert_eq!(parsed.ack, Some(0x0005));
        assert_eq!(&buf[parsed.payload_offset..], b"hello sox");
    }

    // -----------------------------------------------------------------------
    // 4. build_header roundtrip (encode → parse)
    // -----------------------------------------------------------------------
    #[test]
    fn test_encode_decode_roundtrip() {
        let hdr = DaspHeader {
            session_id: 0x1234,
            seq_num: 0x00ab,
            msg_type: DaspMsgType::Challenge,
            version: None,
            remote_id: Some(0x5678),
            digest_algorithm: None,
            nonce: Some(vec![0xde, 0xad, 0xbe, 0xef]),
            username: None,
            digest: None,
            ideal_max: None,
            abs_max: None,
            ack: None,
            ack_more: None,
            receive_max: None,
            receive_timeout_secs: None,
            error_code: None,
            platform_id: None,
            payload_offset: 0,
        };

        let mut buf = Vec::new();
        encode_message(&hdr, &[], &mut buf);

        let parsed = parse_header(&buf).expect("roundtrip parse");
        assert_eq!(parsed.msg_type, DaspMsgType::Challenge);
        assert_eq!(parsed.session_id, 0x1234);
        assert_eq!(parsed.seq_num, 0x00ab);
        assert_eq!(parsed.remote_id, Some(0x5678));
        assert_eq!(parsed.nonce, Some(vec![0xde, 0xad, 0xbe, 0xef]));
    }

    // -----------------------------------------------------------------------
    // 5. compute_auth_digest matches known test vector
    // -----------------------------------------------------------------------
    #[test]
    fn test_auth_digest_known_vector() {
        // SHA1("admin:pass") is a known value we can compute independently.
        let cred = compute_credentials("admin", "pass");
        let nonce = [0x01, 0x02, 0x03, 0x04];
        let digest = compute_auth_digest(&cred, &nonce);

        // Verify by recomputing manually
        let mut h1 = Sha1::new();
        h1.update(b"admin:pass");
        let cred_check: [u8; 20] = h1.finalize().into();
        assert_eq!(cred, cred_check);

        let mut h2 = Sha1::new();
        h2.update(&cred_check);
        h2.update(&[0x01, 0x02, 0x03, 0x04]);
        let expected: [u8; 20] = h2.finalize().into();
        assert_eq!(digest, expected);
    }

    // -----------------------------------------------------------------------
    // 6. Full handshake: HELLO → CHALLENGE → AUTHENTICATE → WELCOME
    // -----------------------------------------------------------------------
    #[test]
    fn test_full_handshake() {
        // Create server transport
        let mut server = test_transport("admin", "password");
        let server_addr = server.local_addr().unwrap();

        // Create client socket
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_nonblocking(false).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        // --- Step 1: Client sends HELLO ---
        let mut hello = empty_header(DaspMsgType::Hello, 0xffff, 0);
        hello.remote_id = Some(0xaaaa);
        hello.version = Some(DASP_VERSION);
        let mut buf = Vec::new();
        encode_message(&hello, &[], &mut buf);
        client.send_to(&buf, server_addr).unwrap();

        // Server processes HELLO
        // Set socket to blocking briefly to receive
        server.socket.set_nonblocking(false).unwrap();
        server.socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let result = server.poll();
        assert!(result.is_none(), "HELLO should not return data");
        assert_eq!(server.session_count(), 1, "session should be created");

        // --- Step 2: Client receives CHALLENGE ---
        let mut recv_buf = [0u8; RECV_BUF_SIZE];
        let (len, _) = client.recv_from(&mut recv_buf).expect("receive challenge");
        let challenge = parse_header(&recv_buf[..len]).expect("parse challenge");
        assert_eq!(challenge.msg_type, DaspMsgType::Challenge);
        let server_session_id = challenge.remote_id.expect("should have remote_id");
        let nonce = challenge.nonce.expect("should have nonce");

        // --- Step 3: Client sends AUTHENTICATE ---
        let cred = compute_credentials("admin", "password");
        let digest = compute_auth_digest(&cred, &nonce);

        let mut auth = empty_header(DaspMsgType::Authenticate, server_session_id, 0);
        auth.username = Some("admin".to_string());
        auth.digest = Some(digest.to_vec());
        encode_message(&auth, &[], &mut buf);
        client.send_to(&buf, server_addr).unwrap();

        // Server processes AUTHENTICATE
        let result = server.poll();
        assert!(result.is_none(), "AUTHENTICATE should not return data");

        // Verify session is authenticated
        let session = server.session(server_session_id).expect("session should exist");
        assert!(session.authenticated, "session should be authenticated");

        // --- Step 4: Client receives WELCOME ---
        let (len, _) = client.recv_from(&mut recv_buf).expect("receive welcome");
        let welcome = parse_header(&recv_buf[..len]).expect("parse welcome");
        assert_eq!(welcome.msg_type, DaspMsgType::Welcome);
        assert!(welcome.remote_id.is_some());

        // Restore non-blocking
        server.socket.set_nonblocking(true).unwrap();
    }

    // -----------------------------------------------------------------------
    // 7. Session expiry after timeout
    // -----------------------------------------------------------------------
    #[test]
    fn test_session_expiry() {
        let mut session = DaspSession::new(1, 2, "127.0.0.1:5000".parse().unwrap(), vec![]);
        assert!(!session.is_expired());

        // Manually set last_activity to the past
        session.last_activity = Instant::now() - Duration::from_secs(SESSION_TIMEOUT_SECS + 1);
        assert!(session.is_expired());
    }

    // -----------------------------------------------------------------------
    // 8. MAX_SESSIONS limit enforcement
    // -----------------------------------------------------------------------
    #[test]
    fn test_max_sessions_limit() {
        let mut server = test_transport("admin", "pass");
        let server_addr = server.local_addr().unwrap();
        server.socket.set_nonblocking(false).unwrap();
        server.socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        let _client = UdpSocket::bind("127.0.0.1:0").unwrap();

        // Fill up sessions
        for i in 0..MAX_SESSIONS {
            let mut hello = empty_header(DaspMsgType::Hello, 0xffff, 0);
            hello.remote_id = Some(i as u16 + 100);
            hello.version = Some(DASP_VERSION);
            let mut buf = Vec::new();
            encode_message(&hello, &[], &mut buf);
            // Send from different "ports" by creating new sockets
            let c = UdpSocket::bind("127.0.0.1:0").unwrap();
            c.send_to(&buf, server_addr).unwrap();
            server.poll();
        }
        assert_eq!(server.session_count(), MAX_SESSIONS);

        // Next HELLO should get a CLOSE(BUSY)
        let mut hello = empty_header(DaspMsgType::Hello, 0xffff, 0);
        hello.remote_id = Some(0x9999);
        hello.version = Some(DASP_VERSION);
        let mut buf = Vec::new();
        encode_message(&hello, &[], &mut buf);
        let overflow_client = UdpSocket::bind("127.0.0.1:0").unwrap();
        overflow_client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        overflow_client.send_to(&buf, server_addr).unwrap();
        server.poll();

        // Session count should not increase
        assert_eq!(server.session_count(), MAX_SESSIONS);

        // Client should receive CLOSE with BUSY error
        let mut recv_buf = [0u8; RECV_BUF_SIZE];
        let (len, _) = overflow_client.recv_from(&mut recv_buf).expect("should receive close");
        let close = parse_header(&recv_buf[..len]).expect("parse close");
        assert_eq!(close.msg_type, DaspMsgType::Close);
        assert_eq!(close.error_code, Some(DaspErrorCode::Busy as u16));
    }

    // -----------------------------------------------------------------------
    // 9. KEEPALIVE response
    // -----------------------------------------------------------------------
    #[test]
    fn test_keepalive_response() {
        let mut server = test_transport("admin", "pass");
        let server_addr = server.local_addr().unwrap();
        server.socket.set_nonblocking(false).unwrap();
        server.socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        // Create a session manually for testing keepalive
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let client_addr = client.local_addr().unwrap();

        let session_id = server.alloc_session_id();
        server.sessions.insert(
            session_id,
            DaspSession::new(session_id, 0x42, client_addr, vec![]),
        );

        // Send keepalive
        let ka = empty_header(DaspMsgType::Keepalive, session_id, 0xffff);
        let mut buf = Vec::new();
        encode_message(&ka, &[], &mut buf);
        client.send_to(&buf, server_addr).unwrap();

        server.poll();

        // Client should receive keepalive back
        let mut recv_buf = [0u8; RECV_BUF_SIZE];
        let (len, _) = client.recv_from(&mut recv_buf).expect("receive keepalive response");
        let resp = parse_header(&recv_buf[..len]).expect("parse keepalive");
        assert_eq!(resp.msg_type, DaspMsgType::Keepalive);
    }

    // -----------------------------------------------------------------------
    // 10. CLOSE removes session
    // -----------------------------------------------------------------------
    #[test]
    fn test_close_removes_session() {
        let mut server = test_transport("admin", "pass");
        let server_addr = server.local_addr().unwrap();
        server.socket.set_nonblocking(false).unwrap();
        server.socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client_addr = client.local_addr().unwrap();

        let session_id = server.alloc_session_id();
        server.sessions.insert(
            session_id,
            DaspSession::new(session_id, 0x42, client_addr, vec![]),
        );
        server.addr_to_session.insert(client_addr, session_id);

        assert_eq!(server.session_count(), 1);

        let close = empty_header(DaspMsgType::Close, session_id, 0xffff);
        let mut buf = Vec::new();
        encode_message(&close, &[], &mut buf);
        client.send_to(&buf, server_addr).unwrap();

        server.poll();
        assert_eq!(server.session_count(), 0);
    }

    // -----------------------------------------------------------------------
    // 11. Invalid message type handling
    // -----------------------------------------------------------------------
    #[test]
    fn test_invalid_message_type() {
        // Craft a packet with invalid message type 0x0F
        let buf = [0x00, 0x01, 0x00, 0x02, 0xF0]; // type=15, 0 fields
        let result = parse_header(&buf);
        assert!(result.is_none(), "invalid message type should return None");
    }

    // -----------------------------------------------------------------------
    // 12. Packet too short
    // -----------------------------------------------------------------------
    #[test]
    fn test_packet_too_short() {
        assert!(parse_header(&[]).is_none());
        assert!(parse_header(&[0x00]).is_none());
        assert!(parse_header(&[0x00, 0x01, 0x02, 0x03]).is_none());
    }

    // -----------------------------------------------------------------------
    // 13. Authentication failure — wrong password
    // -----------------------------------------------------------------------
    #[test]
    fn test_auth_failure_wrong_password() {
        let mut server = test_transport("admin", "correct");
        let server_addr = server.local_addr().unwrap();
        server.socket.set_nonblocking(false).unwrap();
        server.socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        // Send HELLO
        let mut hello = empty_header(DaspMsgType::Hello, 0xffff, 0);
        hello.remote_id = Some(0xbbbb);
        hello.version = Some(DASP_VERSION);
        let mut buf = Vec::new();
        encode_message(&hello, &[], &mut buf);
        client.send_to(&buf, server_addr).unwrap();
        server.poll();

        // Receive CHALLENGE
        let mut recv_buf = [0u8; RECV_BUF_SIZE];
        let (len, _) = client.recv_from(&mut recv_buf).unwrap();
        let challenge = parse_header(&recv_buf[..len]).unwrap();
        let server_session_id = challenge.remote_id.unwrap();
        let nonce = challenge.nonce.unwrap();

        // Send AUTHENTICATE with WRONG credentials
        let bad_cred = compute_credentials("admin", "wrong");
        let bad_digest = compute_auth_digest(&bad_cred, &nonce);

        let mut auth = empty_header(DaspMsgType::Authenticate, server_session_id, 0);
        auth.username = Some("admin".to_string());
        auth.digest = Some(bad_digest.to_vec());
        encode_message(&auth, &[], &mut buf);
        client.send_to(&buf, server_addr).unwrap();
        server.poll();

        // Session should be removed
        assert_eq!(server.session_count(), 0);

        // Client should receive CLOSE with NOT_AUTHENTICATED
        let (len, _) = client.recv_from(&mut recv_buf).unwrap();
        let close = parse_header(&recv_buf[..len]).unwrap();
        assert_eq!(close.msg_type, DaspMsgType::Close);
        assert_eq!(close.error_code, Some(DaspErrorCode::NotAuthenticated as u16));
    }

    // -----------------------------------------------------------------------
    // 14. Datagram delivery after authentication
    // -----------------------------------------------------------------------
    #[test]
    fn test_datagram_delivery() {
        let mut server = test_transport("admin", "pass");
        let server_addr = server.local_addr().unwrap();
        server.socket.set_nonblocking(false).unwrap();
        server.socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let client_addr = client.local_addr().unwrap();

        // Create an already-authenticated session
        let session_id = server.alloc_session_id();
        let mut session = DaspSession::new(session_id, 0x42, client_addr, vec![]);
        session.authenticated = true;
        server.sessions.insert(session_id, session);
        server.addr_to_session.insert(client_addr, session_id);

        // Send a DATAGRAM with SOX payload
        let sox_payload = vec![b'v', 0x01]; // SOX readVersion request
        let dg = empty_header(DaspMsgType::Datagram, session_id, 1);
        let mut buf = Vec::new();
        encode_message(&dg, &sox_payload, &mut buf);
        client.send_to(&buf, server_addr).unwrap();

        let result = server.poll();
        assert!(result.is_some(), "should receive datagram");
        let (sid, payload) = result.unwrap();
        assert_eq!(sid, session_id);
        assert_eq!(payload, sox_payload);
    }

    // -----------------------------------------------------------------------
    // 15. Datagram rejected from unauthenticated session
    // -----------------------------------------------------------------------
    #[test]
    fn test_datagram_rejected_unauthenticated() {
        let mut server = test_transport("admin", "pass");
        let server_addr = server.local_addr().unwrap();
        server.socket.set_nonblocking(false).unwrap();
        server.socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client_addr = client.local_addr().unwrap();

        let session_id = server.alloc_session_id();
        let session = DaspSession::new(session_id, 0x42, client_addr, vec![]);
        // NOT authenticated
        server.sessions.insert(session_id, session);

        let dg = empty_header(DaspMsgType::Datagram, session_id, 1);
        let mut buf = Vec::new();
        encode_message(&dg, &[0x01, 0x02], &mut buf);
        client.send_to(&buf, server_addr).unwrap();

        let result = server.poll();
        assert!(result.is_none(), "unauthenticated datagram should be rejected");
    }

    // -----------------------------------------------------------------------
    // 16. send_to_session delivers to correct client
    // -----------------------------------------------------------------------
    #[test]
    fn test_send_to_session() {
        let mut server = test_transport("admin", "pass");
        let server_addr = server.local_addr().unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let client_addr = client.local_addr().unwrap();

        let session_id = server.alloc_session_id();
        let mut session = DaspSession::new(session_id, 0x42, client_addr, vec![]);
        session.authenticated = true;
        server.sessions.insert(session_id, session);

        let payload = b"response data";
        server.send_to_session(session_id, payload).expect("send should succeed");

        let mut recv_buf = [0u8; RECV_BUF_SIZE];
        let (len, from) = client.recv_from(&mut recv_buf).expect("client should receive");
        assert_eq!(from, server_addr);
        let hdr = parse_header(&recv_buf[..len]).unwrap();
        assert_eq!(hdr.msg_type, DaspMsgType::Datagram);
        assert_eq!(hdr.session_id, 0x42); // addressed to client's remote_id
        assert_eq!(&recv_buf[hdr.payload_offset..len], payload);
    }

    // -----------------------------------------------------------------------
    // 17. cleanup_expired removes old sessions
    // -----------------------------------------------------------------------
    #[test]
    fn test_cleanup_expired() {
        let mut server = test_transport("admin", "pass");

        let session_id = server.alloc_session_id();
        let addr: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        let mut session = DaspSession::new(session_id, 0x42, addr, vec![]);
        session.last_activity = Instant::now() - Duration::from_secs(SESSION_TIMEOUT_SECS + 10);
        server.sessions.insert(session_id, session);
        server.addr_to_session.insert(addr, session_id);

        assert_eq!(server.session_count(), 1);
        server.cleanup_expired();
        assert_eq!(server.session_count(), 0);
    }

    // -----------------------------------------------------------------------
    // 18. Encode/parse with string fields (platform_id, username)
    // -----------------------------------------------------------------------
    #[test]
    fn test_string_fields_roundtrip() {
        let mut hdr = empty_header(DaspMsgType::Discover, 0xffff, 0);
        hdr.platform_id = Some("sandstar-rust-1.0".to_string());

        let mut buf = Vec::new();
        encode_message(&hdr, &[], &mut buf);

        let parsed = parse_header(&buf).unwrap();
        assert_eq!(parsed.platform_id.as_deref(), Some("sandstar-rust-1.0"));
    }

    // -----------------------------------------------------------------------
    // 19. Encode/parse with error_code field
    // -----------------------------------------------------------------------
    #[test]
    fn test_error_code_field() {
        let mut hdr = empty_header(DaspMsgType::Close, 0x1234, 0xffff);
        hdr.error_code = Some(DaspErrorCode::NotAuthenticated as u16);

        let mut buf = Vec::new();
        encode_message(&hdr, &[], &mut buf);

        let parsed = parse_header(&buf).unwrap();
        assert_eq!(parsed.msg_type, DaspMsgType::Close);
        assert_eq!(parsed.error_code, Some(0xe4));
    }

    // -----------------------------------------------------------------------
    // 20. Duplicate HELLO from same address replaces pending session
    // -----------------------------------------------------------------------
    #[test]
    fn test_duplicate_hello_replaces_session() {
        let mut server = test_transport("admin", "pass");
        let server_addr = server.local_addr().unwrap();
        server.socket.set_nonblocking(false).unwrap();
        server.socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        // First HELLO
        let mut hello = empty_header(DaspMsgType::Hello, 0xffff, 0);
        hello.remote_id = Some(0x1111);
        hello.version = Some(DASP_VERSION);
        let mut buf = Vec::new();
        encode_message(&hello, &[], &mut buf);
        client.send_to(&buf, server_addr).unwrap();
        server.poll();
        let mut recv_buf = [0u8; RECV_BUF_SIZE];
        let _ = client.recv_from(&mut recv_buf); // consume challenge

        assert_eq!(server.session_count(), 1);

        // Second HELLO from same address
        hello.remote_id = Some(0x2222);
        encode_message(&hello, &[], &mut buf);
        client.send_to(&buf, server_addr).unwrap();
        server.poll();

        // Should still be only 1 session (old one replaced)
        assert_eq!(server.session_count(), 1);
    }
}
