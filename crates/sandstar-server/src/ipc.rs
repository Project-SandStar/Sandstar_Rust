//! IPC transport layer.
//!
//! On Unix: Unix domain sockets.
//! On Windows: TCP on localhost (named pipes require winapi; TCP is simpler for dev).
//!
//! Connections are accepted and served on their own tasks, off the main loop
//! (R-28). A connection task reads one command frame, hands it to the main
//! loop over a channel, and writes the reply. Reading, getting the command to
//! and back from the engine, and writing each have a total deadline, so a slow
//! or stuck client never holds the engine, REST, SOX or the watchdog. On a
//! failure the client gets an `EngineResponse::Error` when it can still be
//! written. Wire format is unchanged: `[u32 LE length][bincode]`.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sandstar_ipc::protocol::MAX_FRAME_SIZE;
use sandstar_ipc::types::{EngineCommand, EngineResponse};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::time::{timeout, timeout_at, Instant};
use tracing::{error, warn};

// --- Platform abstraction ---

/// Opaque listener type.
#[cfg(unix)]
pub type Listener = tokio::net::UnixListener;

#[cfg(not(unix))]
pub type Listener = tokio::net::TcpListener;

/// Opaque stream type.
#[cfg(unix)]
pub type Stream = tokio::net::UnixStream;

#[cfg(not(unix))]
pub type Stream = tokio::net::TcpStream;

/// Total time to read one command frame, and again to write the reply.
pub const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Time for the command to reach the engine and its reply to come back
/// (queue wait included); as `EngineHandle`'s 10 s.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Connections served at the same time; more get a "busy" reply.
pub const MAX_CONNECTIONS: usize = 4;

/// How long `Shutdown` waits for its reply to be written before stopping.
pub const SHUTDOWN_REPLY_WAIT: Duration = Duration::from_secs(1);

/// Pause after a failed `accept` (e.g. out of file descriptors).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Time for a short error reply on a failure path.
const ERROR_REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// At most one log line per kind of IPC failure in this period.
const LOG_EVERY: Duration = Duration::from_secs(10);

/// One command for the main loop.
pub struct IpcRequest {
    pub cmd: EngineCommand,
    /// The reply goes back to the connection task.
    pub reply: oneshot::Sender<EngineResponse>,
    /// Fires once the reply is written, so `Shutdown` can wait for it.
    pub written: oneshot::Receiver<()>,
}

impl IpcRequest {
    /// The client's task has given up (deadline passed): do not run it.
    pub fn abandoned(&self) -> bool {
        self.reply.is_closed()
    }
}

/// Variant name of a command, for logs (no field values).
pub fn command_name(cmd: &EngineCommand) -> String {
    let debug = format!("{cmd:?}");
    debug
        .split(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Lets one log line through per `LOG_EVERY`, counting the rest.
struct LogLimit {
    last: Mutex<Option<std::time::Instant>>,
    suppressed: AtomicU64,
}

impl LogLimit {
    const fn new() -> Self {
        Self {
            last: Mutex::new(None),
            suppressed: AtomicU64::new(0),
        }
    }

    /// `Some(n)`: log now; `n` lines were suppressed since the last one.
    fn check(&self) -> Option<u64> {
        let now = std::time::Instant::now();
        let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        if last.is_some_and(|t| now.duration_since(t) < LOG_EVERY) {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        *last = Some(now);
        Some(self.suppressed.swap(0, Ordering::Relaxed))
    }
}

static BUSY_LOG: LogLimit = LogLimit::new();
static CONN_ERROR_LOG: LogLimit = LogLimit::new();
static ACCEPT_ERROR_LOG: LogLimit = LogLimit::new();

/// Create the IPC listener.
pub async fn create_listener(path: &str) -> io::Result<Listener> {
    #[cfg(unix)]
    {
        // Remove stale socket file
        let _ = std::fs::remove_file(path);
        let listener = tokio::net::UnixListener::bind(path)?;
        // Restrict socket permissions to owner+group (0660)
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660));
        }
        Ok(listener)
    }

    #[cfg(not(unix))]
    {
        let listener = tokio::net::TcpListener::bind(path).await?;
        Ok(listener)
    }
}

/// Accept one connection.
pub async fn accept(listener: &Listener) -> io::Result<Stream> {
    let (stream, _addr) = listener.accept().await?;
    Ok(stream)
}

/// Accept connections until the main loop is gone, each served on its own task.
pub fn spawn_accept_loop(
    listener: Listener,
    tx: mpsc::Sender<IpcRequest>,
    slots: Arc<Semaphore>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !tx.is_closed() {
            match accept(&listener).await {
                Ok(stream) => spawn_connection(stream, tx.clone(), slots.clone()),
                Err(e) => {
                    if let Some(n) = ACCEPT_ERROR_LOG.check() {
                        error!(err = %e, suppressed = n, "failed to accept IPC connection");
                    }
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                }
            }
        }
    })
}

/// Serve `stream` on its own task if a connection slot is free; otherwise
/// tell the client the server is busy.
pub fn spawn_connection(stream: Stream, tx: mpsc::Sender<IpcRequest>, slots: Arc<Semaphore>) {
    let Ok(permit) = slots.try_acquire_owned() else {
        if let Some(n) = BUSY_LOG.check() {
            warn!(
                max = MAX_CONNECTIONS,
                suppressed = n,
                "IPC: too many connections, refusing"
            );
        }
        tokio::spawn(async move {
            let mut stream = stream;
            reply_error(&mut stream, "busy: too many IPC connections, try again").await;
        });
        return;
    };
    tokio::spawn(async move {
        let _permit = permit;
        if let Err(e) = serve(stream, &tx, IO_TIMEOUT, REPLY_TIMEOUT).await {
            if let Some(n) = CONN_ERROR_LOG.check() {
                warn!(err = %e, suppressed = n, "IPC connection failed");
            }
        }
    });
}

/// Read one command, pass it to the main loop, write the reply.
///
/// A clean disconnect before any byte is `Ok`. Every step that waits on the
/// client or the main loop has a deadline; errors name the command and say
/// whether it ran.
pub async fn serve<S>(
    mut stream: S,
    tx: &mpsc::Sender<IpcRequest>,
    io_timeout: Duration,
    reply_timeout: Duration,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let cmd: EngineCommand = match timeout(io_timeout, read_frame(&mut stream)).await {
        Err(_) => return Err(timed_out("client too slow sending the command")),
        Ok(Err(e)) => {
            if e.kind() == io::ErrorKind::InvalidData {
                reply_error(&mut stream, &format!("bad command frame: {e}")).await;
            }
            return Err(e);
        }
        Ok(Ok(None)) => return Ok(()),
        Ok(Ok(Some(cmd))) => cmd,
    };
    let name = command_name(&cmd);

    // One deadline for queueing the command and getting its reply.
    let deadline = Instant::now() + reply_timeout;
    let (reply_tx, reply_rx) = oneshot::channel();
    let (written_tx, written_rx) = oneshot::channel();
    let request = IpcRequest {
        cmd,
        reply: reply_tx,
        written: written_rx,
    };
    match timeout_at(deadline, tx.send(request)).await {
        Err(_) => {
            reply_error(&mut stream, "engine busy: command not queued, not run").await;
            return Err(timed_out(&format!("{name}: engine queue full, not run")));
        }
        Ok(Err(_)) => {
            reply_error(&mut stream, "server is stopping: command not run").await;
            return Err(broken(&format!("{name}: engine loop has stopped, not run")));
        }
        Ok(Ok(())) => {}
    }

    // On timeout `reply_rx` is dropped here, so the main loop skips the
    // command if it has not started it yet.
    let response = match timeout_at(deadline, reply_rx).await {
        Err(_) => {
            reply_error(
                &mut stream,
                "timed out waiting for the engine: command not run",
            )
            .await;
            return Err(timed_out(&format!("{name}: engine did not answer in time")));
        }
        Ok(Err(_)) => {
            reply_error(&mut stream, "engine did not run the command").await;
            return Err(broken(&format!("{name}: engine loop dropped the command")));
        }
        Ok(Ok(r)) => r,
    };

    match timeout(io_timeout, write_frame(&mut stream, &response)).await {
        Err(_) => {
            return Err(timed_out(&format!(
                "{name} ran, client too slow reading the reply"
            )))
        }
        Ok(Err(e)) => {
            return Err(io::Error::new(
                e.kind(),
                format!("{name} ran, reply not delivered: {e}"),
            ))
        }
        Ok(Ok(())) => {}
    }
    // Nobody waits unless the command was Shutdown.
    let _ = written_tx.send(());
    Ok(())
}

/// Best effort: the connection is failing anyway.
async fn reply_error<S: AsyncWrite + Unpin>(stream: &mut S, msg: &str) {
    let response = EngineResponse::Error(msg.to_string());
    let _ = timeout(ERROR_REPLY_TIMEOUT, write_frame(stream, &response)).await;
}

fn timed_out(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, what.to_string())
}

fn broken(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, what.to_string())
}

/// Async version of `sandstar_ipc::protocol::read_frame`. `None` only when
/// the client closed before sending anything; a partial frame is an error.
async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<EngineCommand>> {
    let mut len_buf = [0u8; 4];
    if reader.read(&mut len_buf[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut len_buf[1..]).await?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame too large: {len} bytes (max {MAX_FRAME_SIZE})"),
        ));
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await?;
    bincode::deserialize(&payload)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Async version of `sandstar_ipc::protocol::write_frame`.
async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &EngineResponse,
) -> io::Result<()> {
    let payload =
        bincode::serialize(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "reply too large"))?;
    writer.write_all(&len.to_le_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await
}

/// Clean up the socket file on shutdown (Unix only).
pub fn cleanup(path: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(path);
    }

    #[cfg(not(unix))]
    {
        let _ = path; // TCP sockets don't need cleanup
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    // Tests run against the platform transport: a Unix socket file on Unix
    // (the production path on the device), TCP on localhost elsewhere.
    #[cfg(unix)]
    type Target = std::path::PathBuf;
    #[cfg(not(unix))]
    type Target = std::net::SocketAddr;

    #[cfg(unix)]
    async fn bind_test(name: &str) -> (Listener, Target) {
        let path = std::env::temp_dir().join(format!(
            "sandstar-ipc-test-{}-{name}.sock",
            std::process::id()
        ));
        let listener = create_listener(path.to_str().unwrap()).await.unwrap();
        (listener, path)
    }

    #[cfg(not(unix))]
    async fn bind_test(_name: &str) -> (Listener, Target) {
        let listener = create_listener("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    async fn connect(target: &Target) -> Stream {
        Stream::connect(target).await.unwrap()
    }

    fn cleanup_test(target: &Target) {
        #[cfg(unix)]
        cleanup(target.to_str().unwrap());
        #[cfg(not(unix))]
        let _ = target;
    }

    fn frame(cmd: &EngineCommand) -> Vec<u8> {
        let mut buf = Vec::new();
        sandstar_ipc::protocol::write_frame(&mut buf, cmd).unwrap();
        buf
    }

    async fn read_reply<R: AsyncRead + Unpin>(r: &mut R) -> EngineResponse {
        let mut len = [0u8; 4];
        r.read_exact(&mut len).await.unwrap();
        let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
        r.read_exact(&mut body).await.unwrap();
        bincode::deserialize(&body).unwrap()
    }

    fn error_text(r: &EngineResponse) -> &str {
        match r {
            EngineResponse::Error(s) => s,
            _ => panic!("expected an error reply"),
        }
    }

    /// Stand-in for the main loop: answers every live command with `Ok`.
    fn answer_all(mut rx: mpsc::Receiver<IpcRequest>) -> tokio::task::JoinHandle<usize> {
        tokio::spawn(async move {
            let mut n = 0;
            while let Some(req) = rx.recv().await {
                n += 1;
                let _ = req.reply.send(EngineResponse::Ok);
            }
            n
        })
    }

    #[tokio::test]
    async fn test_create_listener() {
        let (_listener, target) = bind_test("create").await;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o660, "socket must be owner+group only");
        }
        #[cfg(not(unix))]
        assert_ne!(target.port(), 0);
        cleanup_test(&target);
    }

    #[test]
    fn test_cleanup_idempotent() {
        // Calling cleanup on a non-existent path should not panic
        cleanup("127.0.0.1:0");
        cleanup("/tmp/nonexistent-sandstar-test-socket");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_cleanup_removes_socket_file() {
        let (_listener, target) = bind_test("cleanup").await;
        assert!(target.exists());
        cleanup(target.to_str().unwrap());
        assert!(!target.exists());
    }

    #[test]
    fn test_command_name_has_no_field_values() {
        let cmd = EngineCommand::ReadChannel { channel: 1113 };
        assert_eq!(command_name(&cmd), "ReadChannel");
        assert_eq!(command_name(&EngineCommand::Status), "Status");
    }

    #[tokio::test]
    async fn test_round_trip_over_platform_socket() {
        let (listener, target) = bind_test("roundtrip").await;
        let (tx, rx) = mpsc::channel(MAX_CONNECTIONS);
        let main_loop = answer_all(rx);
        let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let accept_loop = spawn_accept_loop(listener, tx, slots);

        let mut c = connect(&target).await;
        c.write_all(&frame(&EngineCommand::Status)).await.unwrap();
        assert!(matches!(read_reply(&mut c).await, EngineResponse::Ok));
        accept_loop.abort();
        let _ = accept_loop.await;
        assert_eq!(main_loop.await.unwrap(), 1);
        cleanup_test(&target);
    }

    #[tokio::test(start_paused = true)]
    async fn test_read_deadline_is_total_not_per_read() {
        // One byte every 1 s: every single read is quick, the whole frame
        // would finish at ~7 s. A per-read 5 s limit would let it through.
        let (mut client, server) = duplex(64);
        let (tx, mut rx) = mpsc::channel(1);
        let start = Instant::now();
        let task = tokio::spawn(async move { serve(server, &tx, IO_TIMEOUT, REPLY_TIMEOUT).await });
        for b in frame(&EngineCommand::Status) {
            if client.write_all(&[b]).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let err = task.await.unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let took = Instant::now() - start;
        assert!(
            took >= IO_TIMEOUT && took < IO_TIMEOUT + Duration::from_secs(2),
            "{took:?}"
        );
        assert!(
            rx.try_recv().is_err(),
            "an unfinished frame must not reach the engine"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_client_that_never_reads_the_reply_times_out() {
        // A tiny pipe the client never drains: the reply cannot be written.
        let (mut client, server) = duplex(8);
        let (tx, mut rx) = mpsc::channel(1);
        let task = tokio::spawn(async move { serve(server, &tx, IO_TIMEOUT, REPLY_TIMEOUT).await });
        client
            .write_all(&frame(&EngineCommand::ListChannels))
            .await
            .unwrap();
        let req = rx.recv().await.unwrap();
        let mut written = req.written;
        let channels = (0..200)
            .map(|i| sandstar_ipc::types::ChannelInfo {
                id: i,
                label: format!("channel {i}"),
                channel_type: "Analog".into(),
                direction: "In".into(),
                enabled: true,
                status: "Ok".into(),
                raw: 0.0,
                cur: 0.0,
            })
            .collect();
        let _ = req.reply.send(EngineResponse::Channels(channels));
        let err = task.await.unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("ListChannels ran"), "{err}");
        assert!(written.try_recv().is_err(), "no ack for an unwritten reply");
        let _ = client; // kept open, never read
    }

    #[tokio::test]
    async fn test_written_ack_follows_the_reply() {
        let (mut client, server) = duplex(1024);
        let (tx, mut rx) = mpsc::channel(1);
        let task = tokio::spawn(async move { serve(server, &tx, IO_TIMEOUT, REPLY_TIMEOUT).await });
        client
            .write_all(&frame(&EngineCommand::Shutdown))
            .await
            .unwrap();
        let req = rx.recv().await.unwrap();
        let _ = req.reply.send(EngineResponse::Ok);
        req.written.await.expect("ack after the reply is written");
        assert!(matches!(read_reply(&mut client).await, EngineResponse::Ok));
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn test_stuck_engine_times_out_and_the_command_is_abandoned() {
        let (mut client, server) = duplex(1024);
        let (tx, mut rx) = mpsc::channel(1);
        let task = tokio::spawn(async move { serve(server, &tx, IO_TIMEOUT, REPLY_TIMEOUT).await });
        client
            .write_all(&frame(&EngineCommand::Status))
            .await
            .unwrap();
        let held = rx.recv().await.unwrap(); // never answered
        assert!(!held.abandoned());
        let err = task.await.unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            held.abandoned(),
            "the main loop must see the client gave up"
        );
        assert!(error_text(&read_reply(&mut client).await).contains("not run"));
    }

    #[tokio::test(start_paused = true)]
    async fn test_full_queue_times_out_without_queueing() {
        // The queue is full and nothing drains it (engine in a poll).
        let (tx, mut rx) = mpsc::channel(1);
        let (blocker, _) = oneshot::channel();
        let (_, blocker_written) = oneshot::channel();
        tx.send(IpcRequest {
            cmd: EngineCommand::Status,
            reply: blocker,
            written: blocker_written,
        })
        .await
        .unwrap();
        let (mut client, server) = duplex(1024);
        client
            .write_all(&frame(&EngineCommand::Status))
            .await
            .unwrap();
        let err = serve(server, &tx, IO_TIMEOUT, REPLY_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("not run"), "{err}");
        assert!(error_text(&read_reply(&mut client).await).contains("not queued"));
        rx.recv().await.unwrap();
        assert!(rx.try_recv().is_err(), "only the blocker was queued");
    }

    #[tokio::test]
    async fn test_oversized_length_is_refused() {
        let (mut client, server) = duplex(256);
        let (tx, mut rx) = mpsc::channel(1);
        client
            .write_all(&(MAX_FRAME_SIZE + 1).to_le_bytes())
            .await
            .unwrap();
        let err = serve(server, &tx, IO_TIMEOUT, REPLY_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(rx.try_recv().is_err());
        assert!(error_text(&read_reply(&mut client).await).contains("bad command frame"));
    }

    #[tokio::test]
    async fn test_garbage_payload_is_refused() {
        let (mut client, server) = duplex(64);
        let (tx, mut rx) = mpsc::channel(1);
        client
            .write_all(&[3, 0, 0, 0, 0xff, 0xff, 0xff])
            .await
            .unwrap();
        let err = serve(server, &tx, IO_TIMEOUT, REPLY_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_empty_connection_is_quiet() {
        let (client, server) = duplex(64);
        drop(client);
        let (tx, mut rx) = mpsc::channel(1);
        serve(server, &tx, IO_TIMEOUT, REPLY_TIMEOUT).await.unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_partial_length_prefix_is_an_error() {
        let (mut client, server) = duplex(64);
        client.write_all(&[10, 0]).await.unwrap();
        drop(client);
        let (tx, _rx) = mpsc::channel(1);
        let err = serve(server, &tx, IO_TIMEOUT, REPLY_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn test_short_payload_is_an_error() {
        // Length says 10, two bytes arrive, then the client hangs up.
        let (mut client, server) = duplex(64);
        client.write_all(&[10, 0, 0, 0, 1, 2]).await.unwrap();
        drop(client);
        let (tx, _rx) = mpsc::channel(1);
        let err = serve(server, &tx, IO_TIMEOUT, REPLY_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn test_connections_over_the_limit_get_busy_and_slots_come_back() {
        let (listener, target) = bind_test("limit").await;
        let (tx, mut rx) = mpsc::channel(MAX_CONNECTIONS);
        let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let mut idle = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            idle.push(connect(&target).await);
            let stream = accept(&listener).await.unwrap();
            spawn_connection(stream, tx.clone(), slots.clone());
        }
        tokio::task::yield_now().await;
        assert_eq!(slots.available_permits(), 0);

        let mut extra = connect(&target).await;
        let stream = accept(&listener).await.unwrap();
        spawn_connection(stream, tx.clone(), slots.clone());
        let reply = tokio::time::timeout(Duration::from_secs(2), read_reply(&mut extra))
            .await
            .expect("refused at once");
        assert!(error_text(&reply).contains("busy"));

        // Idle clients hang up: every slot is released.
        drop(idle);
        tokio::time::timeout(Duration::from_secs(2), async {
            while slots.available_permits() < MAX_CONNECTIONS {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("permits released");
        assert!(rx.try_recv().is_err());
        cleanup_test(&target);
    }

    #[tokio::test]
    async fn test_a_slow_client_does_not_hold_another() {
        let (tx, rx) = mpsc::channel(MAX_CONNECTIONS);
        let main_loop = answer_all(rx);
        // A client that sent half a length prefix and went quiet.
        let (mut slow, slow_server) = duplex(64);
        slow.write_all(&[1, 0]).await.unwrap();
        let slow_task = tokio::spawn({
            let tx = tx.clone();
            async move { serve(slow_server, &tx, IO_TIMEOUT, REPLY_TIMEOUT).await }
        });
        // A normal client is answered while the slow one waits.
        let (mut fast, fast_server) = duplex(1024);
        fast.write_all(&frame(&EngineCommand::Status))
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            serve(fast_server, &tx, IO_TIMEOUT, REPLY_TIMEOUT),
        )
        .await
        .expect("served without waiting for the slow client")
        .unwrap();
        assert!(matches!(read_reply(&mut fast).await, EngineResponse::Ok));
        slow_task.abort();
        drop(tx);
        drop(slow);
        let _ = main_loop.await;
    }
}
