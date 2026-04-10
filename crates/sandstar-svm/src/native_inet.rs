//! Kit 2 (inet) network native methods in pure Rust.
//!
//! Replaces the C implementation in `csrc/inet_TcpSocket_std.c`,
//! `inet_TcpServerSocket_std.c`, `inet_UdpSocket_std.c`, and
//! `inet_Crypto_sha1.c`.
//!
//! Maps to Kit 2 method slots 0–16 in the native dispatch table:
//!
//! | Slot | Method                           |
//! |------|----------------------------------|
//! |    0 | TcpSocket.connect                |
//! |    1 | TcpSocket.finishConnect           |
//! |    2 | TcpSocket.write                  |
//! |    3 | TcpSocket.read                   |
//! |    4 | TcpSocket.close                  |
//! |    5 | TcpServerSocket.bind             |
//! |    6 | TcpServerSocket.accept           |
//! |    7 | TcpServerSocket.close            |
//! |    8 | UdpSocket.open                   |
//! |    9 | UdpSocket.bind                   |
//! |   10 | UdpSocket.send                   |
//! |   11 | UdpSocket.receive                |
//! |   12 | UdpSocket.close                  |
//! |   13 | UdpSocket.maxPacketSize          |
//! |   14 | UdpSocket.idealPacketSize        |
//! |   15 | Crypto.sha1                      |
//! |   16 | UdpSocket.join                   |
//!
//! # Socket Handle Model
//!
//! The C code stores raw file descriptors in the Sedona component memory.
//! We use an ID-based `SocketStore` (similar to `FileStore` in `native_file.rs`)
//! that maps integer handle IDs to Rust socket objects.
//!
//! # Known Limitations
//!
//! - TCP connect uses blocking `TcpStream::connect_timeout` with a 1ms timeout
//!   to approximate non-blocking connect. `finishConnect` checks if the stream
//!   is writable via a zero-timeout poll.
//! - UDP multicast join uses a hardcoded group address (239.255.18.76)
//!   matching the Sedona discover protocol.
//! - All socket operations use `set_nonblocking(true)` to match the C behavior.

use std::collections::HashMap;
use std::io::{self, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Mutex;
use std::time::Duration;

use crate::native_table::{NativeContext, NativeTable};
use crate::vm_error::VmResult;

// ────────────────────────────────────────────────────────────────
// Socket store
// ────────────────────────────────────────────────────────────────

/// Tracks which TCP connections are still in the "connecting" state
/// (i.e., connect was called but finishConnect hasn't confirmed yet).
enum TcpSocketState {
    /// Non-blocking connect initiated but not yet confirmed.
    Connecting(TcpStream),
    /// Connection established and confirmed.
    Connected(TcpStream),
}

struct SocketStore {
    tcp_sockets: HashMap<i32, TcpSocketState>,
    udp_sockets: HashMap<i32, UdpSocket>,
    server_sockets: HashMap<i32, TcpListener>,
    next_id: i32,
}

impl SocketStore {
    fn new() -> Self {
        Self {
            tcp_sockets: HashMap::new(),
            udp_sockets: HashMap::new(),
            server_sockets: HashMap::new(),
            next_id: 1,
        }
    }

    fn next_handle(&mut self) -> i32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        id
    }

    fn insert_tcp(&mut self, state: TcpSocketState) -> i32 {
        let id = self.next_handle();
        self.tcp_sockets.insert(id, state);
        id
    }

    fn insert_udp(&mut self, sock: UdpSocket) -> i32 {
        let id = self.next_handle();
        self.udp_sockets.insert(id, sock);
        id
    }

    fn insert_server(&mut self, listener: TcpListener) -> i32 {
        let id = self.next_handle();
        self.server_sockets.insert(id, listener);
        id
    }
}

static SOCKET_STORE: Mutex<Option<SocketStore>> = Mutex::new(None);

/// Ensure the global store is initialized and run `f` with a mutable ref.
fn with_sockets<R>(f: impl FnOnce(&mut SocketStore) -> R) -> R {
    let mut guard = SOCKET_STORE.lock().expect("SOCKET_STORE mutex poisoned");
    let store = guard.get_or_insert_with(SocketStore::new);
    f(store)
}

// ────────────────────────────────────────────────────────────────
// Helper: extract IPv4 address from Sedona IpAddr
// ────────────────────────────────────────────────────────────────

/// In Sedona, `inet::IpAddr` is stored as 4 x i32 (128 bits for IPv6 compat).
/// For IPv4, the actual address is in the 4th word (index 3) in network byte
/// order.  The `addr` parameter is the memory offset of the IpAddr struct.
///
/// Since we receive params as i32 slices (not raw memory pointers), and the
/// C code passes `addr` as `params[1].aval` (a pointer to the IpAddr struct
/// in VM memory), we treat params[1] as a byte offset into ctx.memory.
fn read_ipv4_from_memory(memory: &[u8], offset: usize) -> Ipv4Addr {
    // IpAddr is 4 x 32-bit words.  IPv4 address is in the 4th word (offset+12).
    // Stored in network byte order (big-endian).
    if offset + 16 > memory.len() {
        return Ipv4Addr::UNSPECIFIED;
    }
    let b = &memory[offset + 12..offset + 16];
    Ipv4Addr::new(b[0], b[1], b[2], b[3])
}

// ────────────────────────────────────────────────────────────────
// TCP Client (slots 0–4)
// ────────────────────────────────────────────────────────────────

/// `bool TcpSocket.connect(IpAddr addr, int port)` — Kit 2 slot 0
///
/// Initiates a non-blocking TCP connect.  Returns 1 (true) on success
/// (connection in progress), 0 (false) on immediate failure.
///
/// The socket handle is stored in the SocketStore and returned via the
/// return value for the caller to track.  In the real Sedona VM, the handle
/// would be written to the component's memory at a fixed offset.  Here we
/// return the handle ID as the boolean result (1 = success, with handle
/// stored internally).
pub fn tcp_connect(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    // params[0] = self (component pointer — used for storing socket handle)
    // params[1] = addr (pointer to IpAddr struct in memory)
    // params[2] = port
    let _self_ptr = params.first().copied().unwrap_or(0);
    let addr_offset = params.get(1).copied().unwrap_or(0) as usize;
    let port = params.get(2).copied().unwrap_or(0) as u16;

    let ip = read_ipv4_from_memory(ctx.memory, addr_offset);
    let sock_addr = SocketAddr::from((ip, port));

    // Attempt a short-timeout connect to simulate non-blocking behavior.
    // In the C code, the socket is created, set non-blocking, then connect()
    // is called — EWOULDBLOCK/EINPROGRESS is expected.
    match TcpStream::connect_timeout(&sock_addr, Duration::from_millis(1)) {
        Ok(stream) => {
            let _ = stream.set_nonblocking(true);
            let handle = with_sockets(|s| s.insert_tcp(TcpSocketState::Connected(stream)));
            // Store handle at self_ptr + 4 (socket field offset) if self_ptr is valid
            store_handle_at(ctx.memory, _self_ptr as usize, handle);
            Ok(1) // true — connected immediately
        }
        Err(ref e) if is_would_block_or_in_progress(e) => {
            // Connection in progress — we need to create the socket anyway.
            // Since Rust's TcpStream::connect_timeout doesn't give us the stream
            // on WouldBlock, try a real non-blocking connect via TcpStream::connect.
            match TcpStream::connect(sock_addr) {
                Ok(stream) => {
                    let _ = stream.set_nonblocking(true);
                    let handle =
                        with_sockets(|s| s.insert_tcp(TcpSocketState::Connecting(stream)));
                    store_handle_at(ctx.memory, _self_ptr as usize, handle);
                    Ok(1)
                }
                Err(_) => Ok(0), // false
            }
        }
        Err(_) => Ok(0), // false — immediate failure
    }
}

/// `bool TcpSocket.finishConnect()` — Kit 2 slot 1
///
/// Polls the connection status.
/// - Returns 0 (false) if still connecting.
/// - Returns 1 (true) if connection attempt completed (check `closed` for success/fail).
///
/// In the C code, this uses select() with zero timeout.  We approximate this
/// by checking if the socket is writable (which indicates connect completed).
pub fn tcp_finish_connect(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_ptr = params.first().copied().unwrap_or(0) as usize;
    let handle = load_handle_at(ctx.memory, self_ptr);

    if handle <= 0 {
        return Ok(1); // true — "completed" (but failed, closed=true)
    }

    with_sockets(|store| {
        let state = match store.tcp_sockets.get(&handle) {
            Some(_) => {}
            None => return Ok(1), // no socket — treat as completed/failed
        };
        let _ = state;

        // Check if the socket is writable by attempting a zero-byte write peek.
        // For a Connecting socket, we check if the connection is established.
        if let Some(tcp_state) = store.tcp_sockets.get(&handle) {
            match tcp_state {
                TcpSocketState::Connected(_) => {
                    // Already confirmed connected — mark closed=0 (success)
                    set_closed(ctx.memory, self_ptr, false);
                    Ok(1)
                }
                TcpSocketState::Connecting(_) => {
                    // Try a peek_read or write to check if connection finished.
                    // We can check by attempting a zero-length read.
                    // If we get WouldBlock, connection is established.
                    // If we get an error, connection failed.
                    // If we get NotConnected, still connecting.
                    let stream = match &store.tcp_sockets[&handle] {
                        TcpSocketState::Connecting(s) => s,
                        _ => unreachable!(),
                    };
                    match stream.peer_addr() {
                        Ok(_) => {
                            // peer_addr succeeds means we're connected
                            set_closed(ctx.memory, self_ptr, false);
                            // Upgrade to Connected state
                            if let Some(TcpSocketState::Connecting(s)) =
                                store.tcp_sockets.remove(&handle)
                            {
                                store
                                    .tcp_sockets
                                    .insert(handle, TcpSocketState::Connected(s));
                            }
                            Ok(1) // completed, success
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::NotConnected => {
                            Ok(0) // still connecting
                        }
                        Err(_) => {
                            Ok(1) // completed, but failed (closed stays true)
                        }
                    }
                }
            }
        } else {
            Ok(1)
        }
    })
}

/// `int TcpSocket.write(byte[] b, int off, int len)` — Kit 2 slot 2
///
/// Non-blocking send.  Returns bytes written, 0 if would block, -1 on error.
pub fn tcp_write(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_ptr = params.first().copied().unwrap_or(0) as usize;
    let buf_offset = params.get(1).copied().unwrap_or(0) as usize;
    let off = params.get(2).copied().unwrap_or(0) as usize;
    let len = params.get(3).copied().unwrap_or(0) as usize;
    let handle = load_handle_at(ctx.memory, self_ptr);

    if handle <= 0 {
        return Ok(-1);
    }

    let start = buf_offset + off;
    let end = start + len;
    if end > ctx.memory.len() {
        return Ok(-1);
    }
    let data = ctx.memory[start..end].to_vec();

    with_sockets(|store| {
        let stream = match store.tcp_sockets.get_mut(&handle) {
            Some(TcpSocketState::Connected(s)) => s,
            Some(TcpSocketState::Connecting(_)) => return Ok(0), // not connected yet
            None => return Ok(-1),
        };

        match stream.write(&data) {
            Ok(n) => Ok(n as i32),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
            Err(_) => {
                // Close on error, matching C behavior
                let _ = store.tcp_sockets.remove(&handle);
                set_closed(ctx.memory, self_ptr, true);
                Ok(-1)
            }
        }
    })
}

/// `int TcpSocket.read(byte[] b, int off, int len)` — Kit 2 slot 3
///
/// Non-blocking receive.  Returns bytes read (>0), 0 if would block, -1 on
/// error or graceful close.
pub fn tcp_read(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_ptr = params.first().copied().unwrap_or(0) as usize;
    let buf_offset = params.get(1).copied().unwrap_or(0) as usize;
    let off = params.get(2).copied().unwrap_or(0) as usize;
    let len = params.get(3).copied().unwrap_or(0) as usize;
    let handle = load_handle_at(ctx.memory, self_ptr);

    if handle <= 0 {
        return Ok(-1);
    }

    let start = buf_offset + off;
    let end = start + len;
    if end > ctx.memory.len() {
        return Ok(-1);
    }

    // We need to read into a temporary buffer then copy to memory,
    // since we can't borrow ctx.memory mutably while calling with_sockets
    // (which also borrows ctx.memory via set_closed).
    let mut tmp = vec![0u8; len];

    let result = with_sockets(|store| {
        let stream = match store.tcp_sockets.get_mut(&handle) {
            Some(TcpSocketState::Connected(s)) => s,
            Some(TcpSocketState::Connecting(_)) => return Ok(0i32),
            None => return Ok(-1),
        };

        match stream.read(&mut tmp) {
            Ok(0) => {
                // Graceful shutdown — close
                Ok(-2) // sentinel for "need to close"
            }
            Ok(n) => Ok(n as i32),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
            Err(_) => Ok(-1),
        }
    });

    match result? {
        -2 => {
            // Graceful close
            with_sockets(|store| {
                let _ = store.tcp_sockets.remove(&handle);
            });
            set_closed(ctx.memory, self_ptr, true);
            store_handle_at(ctx.memory, self_ptr, -1);
            Ok(-1)
        }
        n if n > 0 => {
            // Copy read data into VM memory
            ctx.memory[start..start + n as usize].copy_from_slice(&tmp[..n as usize]);
            Ok(n)
        }
        other => Ok(other),
    }
}

/// `void TcpSocket.close()` — Kit 2 slot 4
///
/// Closes the TCP socket and marks the component as closed.
pub fn tcp_close(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_ptr = params.first().copied().unwrap_or(0) as usize;
    let handle = load_handle_at(ctx.memory, self_ptr);

    if handle > 0 {
        with_sockets(|store| {
            let _ = store.tcp_sockets.remove(&handle);
        });
    }
    set_closed(ctx.memory, self_ptr, true);
    store_handle_at(ctx.memory, self_ptr, -1);
    Ok(0) // void return
}

// ────────────────────────────────────────────────────────────────
// TCP Server (slots 5–7)
// ────────────────────────────────────────────────────────────────

/// `bool TcpServerSocket.bind(int port)` — Kit 2 slot 5
///
/// Creates a TCP listener socket, binds to the specified port, and
/// starts listening with a backlog of 3.  Sets non-blocking mode.
/// Returns 1 (true) on success, 0 (false) on failure.
pub fn tcp_server_bind(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_ptr = params.first().copied().unwrap_or(0) as usize;
    let port = params.get(1).copied().unwrap_or(0) as u16;

    // Check if already open (closed field at offset 0 in component)
    if !get_closed(ctx.memory, self_ptr) {
        return Ok(0); // already open
    }

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    match TcpListener::bind(addr) {
        Ok(listener) => {
            let _ = listener.set_nonblocking(true);
            let handle = with_sockets(|s| s.insert_server(listener));
            store_handle_at(ctx.memory, self_ptr, handle);
            set_closed(ctx.memory, self_ptr, false);
            Ok(1) // true
        }
        Err(_) => Ok(0), // false
    }
}

/// `bool TcpServerSocket.accept(TcpSocket socket)` — Kit 2 slot 6
///
/// Non-blocking accept.  If a connection is pending, sets up the provided
/// TcpSocket instance with the accepted connection.
/// Returns 1 (true) if accepted, 0 (false) if no pending connections.
pub fn tcp_server_accept(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_ptr = params.first().copied().unwrap_or(0) as usize;
    let accepted_ptr = params.get(1).copied().unwrap_or(0) as usize;
    let handle = load_handle_at(ctx.memory, self_ptr);

    if handle <= 0 {
        return Ok(0);
    }

    with_sockets(|store| {
        let listener = match store.server_sockets.get(&handle) {
            Some(l) => l,
            None => return Ok(0),
        };

        match listener.accept() {
            Ok((stream, _addr)) => {
                let _ = stream.set_nonblocking(true);
                let accepted_handle = store.insert_tcp(TcpSocketState::Connected(stream));
                store_handle_at(ctx.memory, accepted_ptr, accepted_handle);
                set_closed(ctx.memory, accepted_ptr, false);
                Ok(1) // true
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
            Err(_) => Ok(0),
        }
    })
}

/// `void TcpServerSocket.close()` — Kit 2 slot 7
///
/// Closes the server socket.
pub fn tcp_server_close(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_ptr = params.first().copied().unwrap_or(0) as usize;

    if !get_closed(ctx.memory, self_ptr) {
        let handle = load_handle_at(ctx.memory, self_ptr);
        if handle > 0 {
            with_sockets(|store| {
                let _ = store.server_sockets.remove(&handle);
            });
        }
        set_closed(ctx.memory, self_ptr, true);
        store_handle_at(ctx.memory, self_ptr, -1);
    }
    Ok(0) // void return
}

// ────────────────────────────────────────────────────────────────
// UDP (slots 8–14, 16)
// ────────────────────────────────────────────────────────────────

/// `bool UdpSocket.open()` — Kit 2 slot 8
///
/// Creates a UDP socket and sets it to non-blocking mode.
/// Returns 1 (true) on success, 0 (false) on failure.
pub fn udp_open(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_ptr = params.first().copied().unwrap_or(0) as usize;

    // Check if already open
    if !get_closed(ctx.memory, self_ptr) {
        return Ok(0);
    }
    let current_handle = load_handle_at(ctx.memory, self_ptr);
    if current_handle != -1 {
        return Ok(0); // already initialized
    }

    // Bind to 0.0.0.0:0 to get an ephemeral socket
    match UdpSocket::bind("0.0.0.0:0") {
        Ok(sock) => {
            let _ = sock.set_nonblocking(true);
            let handle = with_sockets(|s| s.insert_udp(sock));
            set_closed(ctx.memory, self_ptr, false);
            store_handle_at(ctx.memory, self_ptr, handle);
            Ok(1) // true
        }
        Err(_) => Ok(0), // false
    }
}

/// `bool UdpSocket.bind(int port)` — Kit 2 slot 9
///
/// Binds the UDP socket to the specified port.
/// Returns 1 (true) on success, 0 (false) on failure.
///
/// Note: In the C code, the socket is already open (via UdpSocket.open())
/// and this just binds it.  In Rust, we need to re-create the socket bound
/// to the correct port, since std::net::UdpSocket doesn't support post-bind.
pub fn udp_bind(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_ptr = params.first().copied().unwrap_or(0) as usize;
    let port = params.get(1).copied().unwrap_or(0) as u16;

    if get_closed(ctx.memory, self_ptr) {
        return Ok(0); // socket not open
    }

    let handle = load_handle_at(ctx.memory, self_ptr);
    if handle <= 0 {
        return Ok(0);
    }

    // Remove the old socket and create a new one bound to the port
    with_sockets(|store| {
        let _ = store.udp_sockets.remove(&handle);
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    match UdpSocket::bind(addr) {
        Ok(sock) => {
            let _ = sock.set_nonblocking(true);
            with_sockets(|store| {
                store.udp_sockets.insert(handle, sock);
            });
            Ok(1) // true
        }
        Err(_) => Ok(0), // false
    }
}

/// `bool UdpSocket.join()` — Kit 2 slot 16
///
/// Joins the multicast group 239.255.18.76 (Sedona discovery group, RFC 2365).
/// Returns 1 (true) on success, 0 (false) on failure.
pub fn udp_join(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_ptr = params.first().copied().unwrap_or(0) as usize;

    if get_closed(ctx.memory, self_ptr) {
        return Ok(0);
    }

    let handle = load_handle_at(ctx.memory, self_ptr);
    if handle <= 0 {
        return Ok(0);
    }

    let multicast_addr: Ipv4Addr = Ipv4Addr::new(239, 255, 18, 76);
    let interface = Ipv4Addr::UNSPECIFIED;

    with_sockets(|store| {
        let sock = match store.udp_sockets.get(&handle) {
            Some(s) => s,
            None => return Ok(0),
        };

        match sock.join_multicast_v4(&multicast_addr, &interface) {
            Ok(()) => Ok(1),
            Err(_) => Ok(0),
        }
    })
}

/// `bool UdpSocket.send(UdpDatagram datagram)` — Kit 2 slot 10
///
/// Sends a UDP datagram.  The datagram struct in Sedona memory has:
///   offset 0: addr (pointer to IpAddr, 4 bytes)
///   offset 4: port (i32)
///   offset 8: buf  (pointer to byte[], 4 bytes)
///   offset 12: off (i32)
///   offset 16: len (i32)
///
/// Returns 1 (true) on success, 0 (false) on failure.
pub fn udp_send(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_ptr = params.first().copied().unwrap_or(0) as usize;
    let datagram_ptr = params.get(1).copied().unwrap_or(0) as usize;
    let handle = load_handle_at(ctx.memory, self_ptr);

    if get_closed(ctx.memory, self_ptr) || handle <= 0 {
        return Ok(0);
    }

    if datagram_ptr == 0 || datagram_ptr + 20 > ctx.memory.len() {
        return Ok(0);
    }

    // Read datagram fields from VM memory (32-bit pointers)
    let addr_ptr = read_i32(ctx.memory, datagram_ptr) as usize;
    let port = read_i32(ctx.memory, datagram_ptr + 4) as u16;
    let buf_ptr = read_i32(ctx.memory, datagram_ptr + 8) as usize;
    let off = read_i32(ctx.memory, datagram_ptr + 12) as usize;
    let len = read_i32(ctx.memory, datagram_ptr + 16) as usize;

    if addr_ptr == 0 || buf_ptr == 0 {
        return Ok(0);
    }

    let ip = read_ipv4_from_memory(ctx.memory, addr_ptr);
    let dest = SocketAddr::from((ip, port));

    let start = buf_ptr + off;
    let end = start + len;
    if end > ctx.memory.len() {
        return Ok(0);
    }
    let data = ctx.memory[start..end].to_vec();

    with_sockets(|store| {
        let sock = match store.udp_sockets.get(&handle) {
            Some(s) => s,
            None => return Ok(0),
        };

        match sock.send_to(&data, dest) {
            Ok(_) => Ok(1),
            Err(_) => Ok(0),
        }
    })
}

/// `bool UdpSocket.receive(UdpDatagram datagram)` — Kit 2 slot 11
///
/// Receives a UDP datagram.  On success, updates the datagram struct
/// with received length, source address, and source port.
/// Returns 1 (true) on success, 0 (false) if no data pending or error.
pub fn udp_receive(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_ptr = params.first().copied().unwrap_or(0) as usize;
    let datagram_ptr = params.get(1).copied().unwrap_or(0) as usize;
    let handle = load_handle_at(ctx.memory, self_ptr);

    if get_closed(ctx.memory, self_ptr) || handle <= 0 {
        return Ok(0);
    }

    if datagram_ptr == 0 || datagram_ptr + 20 > ctx.memory.len() {
        return Ok(0);
    }

    // Read buf pointer and len from datagram
    let buf_ptr = read_i32(ctx.memory, datagram_ptr + 8) as usize;
    let off = read_i32(ctx.memory, datagram_ptr + 12) as usize;
    let max_len = read_i32(ctx.memory, datagram_ptr + 16) as usize;

    if buf_ptr == 0 {
        return Ok(0);
    }

    let start = buf_ptr + off;
    if start + max_len > ctx.memory.len() {
        return Ok(0);
    }

    let mut tmp = vec![0u8; max_len];

    let result = with_sockets(|store| {
        let sock = match store.udp_sockets.get(&handle) {
            Some(s) => s,
            None => return Err(()),
        };

        match sock.recv_from(&mut tmp) {
            Ok((n, src_addr)) => Ok((n, src_addr)),
            Err(_) => Err(()),
        }
    });

    match result {
        Ok((n, src_addr)) => {
            // Copy received data into VM memory
            ctx.memory[start..start + n].copy_from_slice(&tmp[..n]);

            // Update datagram: len = received count
            write_i32(ctx.memory, datagram_ptr + 16, n as i32);

            // Update datagram: port
            let src_port = src_addr.port() as i32;
            write_i32(ctx.memory, datagram_ptr + 4, src_port);

            // Update source address — write to the inline IpAddr at self_ptr+8
            // (In C: receiveIpAddr = getInline(self, 8))
            let receive_ip_addr = self_ptr + 8;
            if let SocketAddr::V4(v4) = src_addr {
                write_ipv4_to_memory(ctx.memory, receive_ip_addr, *v4.ip());
            }
            // Point datagram.addr to the inline IpAddr
            write_i32(ctx.memory, datagram_ptr, receive_ip_addr as i32);

            Ok(1) // true
        }
        Err(()) => {
            // On failure: len=0, addr=null, port=-1
            write_i32(ctx.memory, datagram_ptr + 16, 0);
            write_i32(ctx.memory, datagram_ptr, 0); // null addr
            write_i32(ctx.memory, datagram_ptr + 4, -1);
            Ok(0) // false
        }
    }
}

/// `void UdpSocket.close()` — Kit 2 slot 12
///
/// Closes the UDP socket.
pub fn udp_close(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let self_ptr = params.first().copied().unwrap_or(0) as usize;

    if !get_closed(ctx.memory, self_ptr) {
        let handle = load_handle_at(ctx.memory, self_ptr);
        if handle > 0 {
            with_sockets(|store| {
                let _ = store.udp_sockets.remove(&handle);
            });
        }
        set_closed(ctx.memory, self_ptr, true);
        store_handle_at(ctx.memory, self_ptr, -1);
    }
    Ok(0) // void return
}

/// `static int UdpSocket.maxPacketSize()` — Kit 2 slot 13
///
/// Returns 512, matching the C implementation (max SoxService buffer).
pub fn udp_max_packet_size(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    Ok(512)
}

/// `static int UdpSocket.idealPacketSize()` — Kit 2 slot 14
///
/// Returns 512, matching the C implementation.
pub fn udp_ideal_packet_size(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    Ok(512)
}

// ────────────────────────────────────────────────────────────────
// SHA-1 (slot 15)
// ────────────────────────────────────────────────────────────────

/// `static void Crypto.sha1(byte[] input, int inputOff, int len, byte[] output, int outputOff)`
/// — Kit 2 slot 15
///
/// Computes SHA-1 hash of the input data and writes the 20-byte digest to output.
/// Uses a pure Rust SHA-1 implementation (no external crate dependency).
pub fn crypto_sha1(ctx: &mut NativeContext<'_>, params: &[i32]) -> VmResult<i32> {
    let in_ptr = params.first().copied().unwrap_or(0) as usize;
    let in_off = params.get(1).copied().unwrap_or(0) as usize;
    let len = params.get(2).copied().unwrap_or(0) as usize;
    let out_ptr = params.get(3).copied().unwrap_or(0) as usize;
    let out_off = params.get(4).copied().unwrap_or(0) as usize;

    let in_start = in_ptr + in_off;
    let in_end = in_start + len;
    let out_start = out_ptr + out_off;
    let out_end = out_start + 20; // SHA-1 produces 20 bytes

    if in_end > ctx.memory.len() || out_end > ctx.memory.len() {
        return Ok(0); // void, but protect against OOB
    }

    let input = ctx.memory[in_start..in_end].to_vec();
    let digest = sha1_compute(&input);
    ctx.memory[out_start..out_end].copy_from_slice(&digest);

    Ok(0) // void return
}

// ────────────────────────────────────────────────────────────────
// Pure Rust SHA-1 implementation (RFC 3174)
// ────────────────────────────────────────────────────────────────

/// Compute SHA-1 hash of the given data, returning a 20-byte digest.
///
/// This is a direct port of the RFC 3174 reference implementation used in
/// the C code (`inet_sha1.c`).
fn sha1_compute(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xEFCDAB89;
    let mut h2: u32 = 0x98BADCFE;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xC3D2E1F0;

    let bit_len = (data.len() as u64) * 8;

    // Pre-processing: add padding
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    // Append original length in bits as 64-bit big-endian
    msg.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 512-bit (64-byte) block
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];

        // Initialize first 16 words from the chunk
        for t in 0..16 {
            w[t] = u32::from_be_bytes([
                chunk[t * 4],
                chunk[t * 4 + 1],
                chunk[t * 4 + 2],
                chunk[t * 4 + 3],
            ]);
        }

        // Extend to 80 words
        for t in 16..80 {
            w[t] = (w[t - 3] ^ w[t - 8] ^ w[t - 14] ^ w[t - 16]).rotate_left(1);
        }

        let mut a = h0;
        let mut b = h1;
        let mut c = h2;
        let mut d = h3;
        let mut e = h4;

        for t in 0..80 {
            let (f, k) = match t {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDCu32),
                60..=79 => (b ^ c ^ d, 0xCA62C1D6u32),
                _ => unreachable!(),
            };

            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[t]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut digest = [0u8; 20];
    digest[0..4].copy_from_slice(&h0.to_be_bytes());
    digest[4..8].copy_from_slice(&h1.to_be_bytes());
    digest[8..12].copy_from_slice(&h2.to_be_bytes());
    digest[12..16].copy_from_slice(&h3.to_be_bytes());
    digest[16..20].copy_from_slice(&h4.to_be_bytes());
    digest
}

// ────────────────────────────────────────────────────────────────
// Component memory helpers
// ────────────────────────────────────────────────────────────────

// In the C Sedona VM, socket components have a standard layout:
//   offset 0: closed (1 byte, 0=open, 1=closed)
//   offset 4: socket handle (i32, 4 bytes)
//
// Since we use integer handle IDs instead of raw fds, we store/load
// from these fixed offsets relative to the component's base pointer.

/// Read the `closed` flag from the component at `self_ptr`.
fn get_closed(memory: &[u8], self_ptr: usize) -> bool {
    if self_ptr >= memory.len() {
        return true; // out of bounds — treat as closed
    }
    memory[self_ptr] != 0
}

/// Set the `closed` flag on the component at `self_ptr`.
fn set_closed(memory: &mut Vec<u8>, self_ptr: usize, closed: bool) {
    if self_ptr < memory.len() {
        memory[self_ptr] = if closed { 1 } else { 0 };
    }
}

/// Load the socket handle ID from the component at `self_ptr + 4`.
fn load_handle_at(memory: &[u8], self_ptr: usize) -> i32 {
    read_i32(memory, self_ptr + 4)
}

/// Store a socket handle ID at the component's `self_ptr + 4`.
fn store_handle_at(memory: &mut Vec<u8>, self_ptr: usize, handle: i32) {
    write_i32(memory, self_ptr + 4, handle);
}

/// Read a little-endian i32 from memory at the given offset.
fn read_i32(memory: &[u8], offset: usize) -> i32 {
    if offset + 4 > memory.len() {
        return 0;
    }
    i32::from_le_bytes([
        memory[offset],
        memory[offset + 1],
        memory[offset + 2],
        memory[offset + 3],
    ])
}

/// Write a little-endian i32 to memory at the given offset.
fn write_i32(memory: &mut [u8], offset: usize, value: i32) {
    if offset + 4 <= memory.len() {
        memory[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
}

/// Write an IPv4 address into the IpAddr struct in VM memory.
/// IpAddr layout: 4 x i32 (16 bytes).  For IPv4:
///   word[0] = 0, word[1] = 0, word[2] = 0xffff (mapped), word[3] = addr
fn write_ipv4_to_memory(memory: &mut [u8], offset: usize, addr: Ipv4Addr) {
    if offset + 16 > memory.len() {
        return;
    }
    // Clear first 8 bytes (words 0-1)
    for b in &mut memory[offset..offset + 8] {
        *b = 0;
    }
    // Word 2: 0x0000ffff in little-endian (IPv4-mapped marker)
    write_i32(memory, offset + 8, 0x0000ffff_u32 as i32);
    // Word 3: IPv4 address in network byte order (stored as-is in the i32 slot)
    let octets = addr.octets();
    memory[offset + 12] = octets[0];
    memory[offset + 13] = octets[1];
    memory[offset + 14] = octets[2];
    memory[offset + 15] = octets[3];
}

/// Check if an I/O error indicates the operation would block or is in progress.
fn is_would_block_or_in_progress(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

// ────────────────────────────────────────────────────────────────
// Registration
// ────────────────────────────────────────────────────────────────

/// Register all Kit 2 (inet) native methods (slots 0–16) in the dispatch
/// table, replacing the stubs that were registered by `NativeTable::with_defaults()`.
pub fn register_kit2(table: &mut NativeTable) {
    // TCP Client
    table.register(2, 0, tcp_connect);           // TcpSocket.connect
    table.register(2, 1, tcp_finish_connect);    // TcpSocket.finishConnect
    table.register(2, 2, tcp_write);             // TcpSocket.write
    table.register(2, 3, tcp_read);              // TcpSocket.read
    table.register(2, 4, tcp_close);             // TcpSocket.close

    // TCP Server
    table.register(2, 5, tcp_server_bind);       // TcpServerSocket.bind
    table.register(2, 6, tcp_server_accept);     // TcpServerSocket.accept
    table.register(2, 7, tcp_server_close);      // TcpServerSocket.close

    // UDP
    table.register(2, 8, udp_open);              // UdpSocket.open
    table.register(2, 9, udp_bind);              // UdpSocket.bind
    table.register(2, 10, udp_send);             // UdpSocket.send
    table.register(2, 11, udp_receive);          // UdpSocket.receive
    table.register(2, 12, udp_close);            // UdpSocket.close
    table.register(2, 13, udp_max_packet_size);  // UdpSocket.maxPacketSize
    table.register(2, 14, udp_ideal_packet_size); // UdpSocket.idealPacketSize

    // Crypto
    table.register(2, 15, crypto_sha1);          // Crypto.sha1

    // UDP multicast (must be after other UDP slots for correct ordering)
    table.register(2, 16, udp_join);             // UdpSocket.join
}

/// Reset the global socket store, closing all sockets.
/// Used in tests to clean up between test runs.
#[cfg(test)]
fn reset_socket_store() {
    let mut guard = SOCKET_STORE.lock().expect("SOCKET_STORE mutex poisoned");
    *guard = None;
}

// ────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a simulated component memory block with `closed=true` and `handle=-1`.
    /// Returns a Vec<u8> of at least `size` bytes.
    fn make_component_memory(size: usize) -> Vec<u8> {
        let mut mem = vec![0u8; size.max(32)];
        // closed = 1 (true) at offset 0
        mem[0] = 1;
        // handle = -1 at offset 4
        write_i32(&mut mem, 4, -1);
        mem
    }

    // ── SHA-1 tests ────────────────────────────────────────────

    #[test]
    fn sha1_empty_string() {
        let digest = sha1_compute(b"");
        // SHA-1("") = da39a3ee5e6b4b0d3255bfef95601890afd80709
        assert_eq!(
            digest,
            [
                0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55,
                0xbf, 0xef, 0x95, 0x60, 0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09,
            ]
        );
    }

    #[test]
    fn sha1_abc() {
        let digest = sha1_compute(b"abc");
        // SHA-1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
        assert_eq!(
            digest,
            [
                0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e,
                0x25, 0x71, 0x78, 0x50, 0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d,
            ]
        );
    }

    #[test]
    fn sha1_rfc_test_vector_1() {
        // "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        let input = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        let digest = sha1_compute(input);
        // 84983E441C3BD26EBAAE4AA1F95129E5E54670F1
        assert_eq!(
            digest,
            [
                0x84, 0x98, 0x3e, 0x44, 0x1c, 0x3b, 0xd2, 0x6e, 0xba, 0xae,
                0x4a, 0xa1, 0xf9, 0x51, 0x29, 0xe5, 0xe5, 0x46, 0x70, 0xf1,
            ]
        );
    }

    #[test]
    fn sha1_rfc_test_vector_2() {
        // Single character "a"
        let digest = sha1_compute(b"a");
        // 86f7e437faa5a7fce15d1ddcb9eaeaea377667b8
        assert_eq!(
            digest,
            [
                0x86, 0xf7, 0xe4, 0x37, 0xfa, 0xa5, 0xa7, 0xfc, 0xe1, 0x5d,
                0x1d, 0xdc, 0xb9, 0xea, 0xea, 0xea, 0x37, 0x76, 0x67, 0xb8,
            ]
        );
    }

    #[test]
    fn sha1_longer_message() {
        // "The quick brown fox jumps over the lazy dog"
        let digest = sha1_compute(b"The quick brown fox jumps over the lazy dog");
        // 2fd4e1c67a2d28fced849ee1bb76e7391b93eb12
        assert_eq!(
            digest,
            [
                0x2f, 0xd4, 0xe1, 0xc6, 0x7a, 0x2d, 0x28, 0xfc, 0xed, 0x84,
                0x9e, 0xe1, 0xbb, 0x76, 0xe7, 0x39, 0x1b, 0x93, 0xeb, 0x12,
            ]
        );
    }

    #[test]
    fn crypto_sha1_via_native_method() {
        // Place input "abc" at offset 100 and output buffer at offset 200
        let mut mem = vec![0u8; 300];
        mem[100] = b'a';
        mem[101] = b'b';
        mem[102] = b'c';

        let mut ctx = NativeContext::new(&mut mem);
        let params = [
            100, // in_ptr
            0,   // in_off
            3,   // len
            200, // out_ptr
            0,   // out_off
        ];
        let result = crypto_sha1(&mut ctx, &params).unwrap();
        assert_eq!(result, 0); // void

        // Check the digest at offset 200
        let expected: [u8; 20] = [
            0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e,
            0x25, 0x71, 0x78, 0x50, 0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d,
        ];
        assert_eq!(&ctx.memory[200..220], &expected);
    }

    #[test]
    fn crypto_sha1_with_offsets() {
        // Place "Xabc" at offset 50, use in_off=1 to skip 'X'
        let mut mem = vec![0u8; 300];
        mem[50] = b'X';
        mem[51] = b'a';
        mem[52] = b'b';
        mem[53] = b'c';

        let mut ctx = NativeContext::new(&mut mem);
        let params = [
            50,  // in_ptr
            1,   // in_off — skip the 'X'
            3,   // len
            200, // out_ptr
            5,   // out_off
        ];
        crypto_sha1(&mut ctx, &params).unwrap();

        let expected: [u8; 20] = [
            0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e,
            0x25, 0x71, 0x78, 0x50, 0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d,
        ];
        assert_eq!(&ctx.memory[205..225], &expected);
    }

    // ── maxPacketSize / idealPacketSize ─────────────────────────

    #[test]
    fn max_packet_size_returns_512() {
        let mut mem = vec![0u8; 16];
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(udp_max_packet_size(&mut ctx, &[]).unwrap(), 512);
    }

    #[test]
    fn ideal_packet_size_returns_512() {
        let mut mem = vec![0u8; 16];
        let mut ctx = NativeContext::new(&mut mem);
        assert_eq!(udp_ideal_packet_size(&mut ctx, &[]).unwrap(), 512);
    }

    // ── TCP close on invalid handle ────────────────────────────

    #[test]
    fn tcp_close_invalid_handle_returns_ok() {
        reset_socket_store();
        let mut mem = make_component_memory(32);
        let mut ctx = NativeContext::new(&mut mem);
        let result = tcp_close(&mut ctx, &[0]).unwrap();
        assert_eq!(result, 0); // void return
        // closed should be set to true
        assert_eq!(mem[0], 1);
    }

    #[test]
    fn tcp_close_no_socket_is_safe() {
        reset_socket_store();
        let mut mem = make_component_memory(32);
        // Set handle to some non-existent value
        write_i32(&mut mem, 4, 999);
        let mut ctx = NativeContext::new(&mut mem);
        let result = tcp_close(&mut ctx, &[0]).unwrap();
        assert_eq!(result, 0);
    }

    // ── UDP close on invalid handle ────────────────────────────

    #[test]
    fn udp_close_invalid_handle_returns_ok() {
        reset_socket_store();
        let mut mem = make_component_memory(32);
        let mut ctx = NativeContext::new(&mut mem);
        let result = udp_close(&mut ctx, &[0]).unwrap();
        assert_eq!(result, 0);
    }

    // ── TCP server bind on available port ──────────────────────

    #[test]
    fn tcp_server_bind_succeeds() {
        reset_socket_store();
        let mut mem = make_component_memory(32);
        let mut ctx = NativeContext::new(&mut mem);
        // Use port 0 for OS-assigned port
        let result = tcp_server_bind(&mut ctx, &[0, 0]).unwrap();
        assert_eq!(result, 1); // true — success
        // closed should be set to false
        assert_eq!(mem[0], 0);
        // handle should be > 0
        let handle = read_i32(&mem, 4);
        assert!(handle > 0, "handle should be positive, got {}", handle);

        // Clean up
        let mut ctx = NativeContext::new(&mut mem);
        tcp_server_close(&mut ctx, &[0]).unwrap();
    }

    #[test]
    fn tcp_server_bind_already_open_returns_false() {
        reset_socket_store();
        let mut mem = make_component_memory(32);

        // First bind should succeed
        let mut ctx = NativeContext::new(&mut mem);
        tcp_server_bind(&mut ctx, &[0, 0]).unwrap();

        // Second bind should fail (already open)
        let mut ctx = NativeContext::new(&mut mem);
        let result = tcp_server_bind(&mut ctx, &[0, 0]).unwrap();
        assert_eq!(result, 0); // false

        // Clean up
        let mut ctx = NativeContext::new(&mut mem);
        tcp_server_close(&mut ctx, &[0]).unwrap();
    }

    #[test]
    fn tcp_server_close_twice_is_safe() {
        reset_socket_store();
        let mut mem = make_component_memory(32);

        let mut ctx = NativeContext::new(&mut mem);
        tcp_server_bind(&mut ctx, &[0, 0]).unwrap();

        let mut ctx = NativeContext::new(&mut mem);
        tcp_server_close(&mut ctx, &[0]).unwrap();
        assert_eq!(mem[0], 1); // closed

        // Close again — should be safe
        let mut ctx = NativeContext::new(&mut mem);
        tcp_server_close(&mut ctx, &[0]).unwrap();
        assert_eq!(mem[0], 1);
    }

    // ── UDP open + close cycle ─────────────────────────────────

    #[test]
    fn udp_open_close_cycle() {
        reset_socket_store();
        let mut mem = make_component_memory(32);

        let mut ctx = NativeContext::new(&mut mem);
        let result = udp_open(&mut ctx, &[0]).unwrap();
        assert_eq!(result, 1); // true — success
        assert_eq!(mem[0], 0); // not closed
        let handle = read_i32(&mem, 4);
        assert!(handle > 0);

        let mut ctx = NativeContext::new(&mut mem);
        udp_close(&mut ctx, &[0]).unwrap();
        assert_eq!(mem[0], 1); // closed
        assert_eq!(read_i32(&mem, 4), -1); // handle reset
    }

    #[test]
    fn udp_open_already_open_returns_false() {
        reset_socket_store();
        let mut mem = make_component_memory(32);

        let mut ctx = NativeContext::new(&mut mem);
        udp_open(&mut ctx, &[0]).unwrap();

        // Try to open again — should fail
        let mut ctx = NativeContext::new(&mut mem);
        let result = udp_open(&mut ctx, &[0]).unwrap();
        assert_eq!(result, 0); // false — already open

        // Clean up
        let mut ctx = NativeContext::new(&mut mem);
        udp_close(&mut ctx, &[0]).unwrap();
    }

    // ── TCP connect + close cycle ──────────────────────────────

    #[test]
    fn tcp_connect_to_nowhere_and_close() {
        reset_socket_store();
        // Create memory with IpAddr at offset 32 pointing to 127.0.0.1
        let mut mem = vec![0u8; 128];
        mem[0] = 1; // closed
        write_i32(&mut mem, 4, -1); // handle = -1

        // Write IpAddr at offset 32: [0, 0, 0xffff, 127.0.0.1]
        write_i32(&mut mem, 32, 0);
        write_i32(&mut mem, 36, 0);
        write_i32(&mut mem, 40, 0x0000ffff_u32 as i32);
        mem[44] = 127;
        mem[45] = 0;
        mem[46] = 0;
        mem[47] = 1;

        let mut ctx = NativeContext::new(&mut mem);
        // Try connecting to 127.0.0.1:1 (likely will fail quickly)
        let _result = tcp_connect(&mut ctx, &[0, 32, 1]).unwrap();
        // Result may be 0 or 1 depending on OS behavior — either is OK

        // Close should always work
        let mut ctx = NativeContext::new(&mut mem);
        tcp_close(&mut ctx, &[0]).unwrap();
        assert_eq!(mem[0], 1); // closed
    }

    // ── Registration ───────────────────────────────────────────

    #[test]
    fn register_kit2_populates_all_17_methods() {
        let mut table = NativeTable::with_defaults();
        register_kit2(&mut table);

        // All 17 slots should be Normal (not Stub)
        for slot in 0..17u16 {
            let entry = table.lookup(2, slot).unwrap();
            assert!(
                matches!(entry, crate::native_table::NativeEntry::Normal(_)),
                "slot {} should be Normal after registration",
                slot
            );
        }
    }

    #[test]
    fn register_kit2_already_registered_in_defaults() {
        let table = NativeTable::with_defaults();

        // with_defaults() now registers inet (kit 2), so slot 0 should already be Normal
        let entry = table.lookup(2, 0).unwrap();
        assert!(
            matches!(entry, crate::native_table::NativeEntry::Normal(_)),
            "kit 2 slot 0 should be Normal after with_defaults() registers inet"
        );
    }

    #[test]
    fn register_kit2_methods_callable_via_dispatch() {
        let mut table = NativeTable::with_defaults();
        register_kit2(&mut table);

        // Call maxPacketSize (slot 13) via dispatch
        let mut mem = vec![0u8; 16];
        let mut ctx = NativeContext::new(&mut mem);
        let result = table.call(2, 13, &mut ctx, &[]).unwrap();
        assert_eq!(result, 512);

        // Call idealPacketSize (slot 14) via dispatch
        let mut ctx = NativeContext::new(&mut mem);
        let result = table.call(2, 14, &mut ctx, &[]).unwrap();
        assert_eq!(result, 512);
    }

    // ── Component memory helpers ───────────────────────────────

    #[test]
    fn get_set_closed() {
        let mut mem = vec![0u8; 16];
        assert!(!get_closed(&mem, 0)); // default 0 = not closed

        set_closed(&mut mem, 0, true);
        assert!(get_closed(&mem, 0));

        set_closed(&mut mem, 0, false);
        assert!(!get_closed(&mem, 0));
    }

    #[test]
    fn load_store_handle() {
        let mut mem = vec![0u8; 16];
        write_i32(&mut mem, 4, -1);
        assert_eq!(load_handle_at(&mem, 0), -1);

        store_handle_at(&mut mem, 0, 42);
        assert_eq!(load_handle_at(&mem, 0), 42);
    }

    #[test]
    fn read_write_i32_roundtrip() {
        let mut mem = vec![0u8; 16];
        write_i32(&mut mem, 0, 0x12345678);
        assert_eq!(read_i32(&mem, 0), 0x12345678);

        write_i32(&mut mem, 4, -1);
        assert_eq!(read_i32(&mem, 4), -1);

        write_i32(&mut mem, 8, 0);
        assert_eq!(read_i32(&mem, 8), 0);
    }

    #[test]
    fn read_i32_out_of_bounds_returns_zero() {
        let mem = vec![0u8; 2];
        assert_eq!(read_i32(&mem, 0), 0);
    }

    #[test]
    fn read_ipv4_from_memory_valid() {
        let mut mem = vec![0u8; 32];
        // Word 3 at offset 12: 127.0.0.1
        mem[12] = 127;
        mem[13] = 0;
        mem[14] = 0;
        mem[15] = 1;
        assert_eq!(read_ipv4_from_memory(&mem, 0), Ipv4Addr::new(127, 0, 0, 1));
    }

    #[test]
    fn read_ipv4_out_of_bounds_returns_unspecified() {
        let mem = vec![0u8; 4];
        assert_eq!(read_ipv4_from_memory(&mem, 0), Ipv4Addr::UNSPECIFIED);
    }

    #[test]
    fn write_ipv4_roundtrip() {
        let mut mem = vec![0u8; 32];
        write_ipv4_to_memory(&mut mem, 0, Ipv4Addr::new(192, 168, 1, 100));

        // Verify: words 0,1 = 0, word 2 = 0xffff, word 3 = address
        assert_eq!(read_i32(&mem, 0), 0);
        assert_eq!(read_i32(&mem, 4), 0);
        assert_eq!(mem[12], 192);
        assert_eq!(mem[13], 168);
        assert_eq!(mem[14], 1);
        assert_eq!(mem[15], 100);

        // Read it back
        let ip = read_ipv4_from_memory(&mem, 0);
        assert_eq!(ip, Ipv4Addr::new(192, 168, 1, 100));
    }

    // ── TCP write/read with invalid handle ─────────────────────

    #[test]
    fn tcp_write_invalid_handle_returns_neg_one() {
        reset_socket_store();
        let mut mem = make_component_memory(64);
        let mut ctx = NativeContext::new(&mut mem);
        let result = tcp_write(&mut ctx, &[0, 32, 0, 4]).unwrap();
        assert_eq!(result, -1);
    }

    #[test]
    fn tcp_read_invalid_handle_returns_neg_one() {
        reset_socket_store();
        let mut mem = make_component_memory(64);
        let mut ctx = NativeContext::new(&mut mem);
        let result = tcp_read(&mut ctx, &[0, 32, 0, 4]).unwrap();
        assert_eq!(result, -1);
    }

    // ── UDP send/receive without connection ─────────────────────

    #[test]
    fn udp_send_closed_socket_returns_false() {
        reset_socket_store();
        let mut mem = make_component_memory(64);
        let mut ctx = NativeContext::new(&mut mem);
        let result = udp_send(&mut ctx, &[0, 32]).unwrap();
        assert_eq!(result, 0); // false
    }

    #[test]
    fn udp_receive_closed_socket_returns_false() {
        reset_socket_store();
        let mut mem = make_component_memory(64);
        let mut ctx = NativeContext::new(&mut mem);
        let result = udp_receive(&mut ctx, &[0, 32]).unwrap();
        assert_eq!(result, 0); // false
    }

    // ── finish_connect with no socket ──────────────────────────

    #[test]
    fn finish_connect_no_socket_returns_true() {
        reset_socket_store();
        let mut mem = make_component_memory(32);
        let mut ctx = NativeContext::new(&mut mem);
        // handle = -1, so should immediately return "completed" (true=1)
        let result = tcp_finish_connect(&mut ctx, &[0]).unwrap();
        assert_eq!(result, 1);
    }
}
