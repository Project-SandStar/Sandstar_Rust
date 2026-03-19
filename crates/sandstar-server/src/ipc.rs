//! IPC transport layer.
//!
//! On Unix: Unix domain sockets.
//! On Windows: TCP on localhost (named pipes require winapi; TCP is simpler for dev).

use std::io;
use std::time::Duration;

// --- Platform abstraction ---

/// Opaque listener type.
#[cfg(unix)]
pub type Listener = tokio::net::UnixListener;

#[cfg(not(unix))]
pub type Listener = tokio::net::TcpListener;

/// Opaque stream type.
#[cfg(unix)]
pub type Stream = std::os::unix::net::UnixStream;

#[cfg(not(unix))]
pub type Stream = std::net::TcpStream;

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

/// Accept one connection, returning a synchronous stream for frame I/O.
///
/// We accept via tokio (async) then convert to std (sync) for the
/// blocking bincode read/write in the single-threaded runtime.
pub async fn accept(listener: &Listener) -> io::Result<(Stream, Vec<u8>, Vec<u8>)> {
    #[cfg(unix)]
    {
        let (stream, _addr) = listener.accept().await?;
        let std_stream = stream.into_std()?;
        std_stream.set_nonblocking(false)?;
        // Prevent a hung client from blocking the event loop + watchdog.
        std_stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        Ok((std_stream, Vec::new(), Vec::new()))
    }

    #[cfg(not(unix))]
    {
        let (stream, _addr) = listener.accept().await?;
        let std_stream = stream.into_std()?;
        std_stream.set_nonblocking(false)?;
        // Prevent a hung client from blocking the event loop + watchdog.
        std_stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        Ok((std_stream, Vec::new(), Vec::new()))
    }
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

    #[tokio::test]
    async fn test_create_listener() {
        // Bind to port 0 for dynamic assignment
        let listener = create_listener("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Should have been assigned a real port
        assert_ne!(addr.port(), 0);
    }

    #[tokio::test]
    async fn test_accept_sets_read_timeout() {
        let listener = create_listener("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Connect a client
        let _client = std::net::TcpStream::connect(addr).unwrap();

        // Accept it on the server side
        let (stream, _, _) = accept(&listener).await.unwrap();
        let timeout = stream.read_timeout().unwrap();
        assert_eq!(timeout, Some(Duration::from_secs(5)));
    }

    #[test]
    fn test_cleanup_idempotent() {
        // Calling cleanup on a non-existent path should not panic
        cleanup("127.0.0.1:0");
        cleanup("/tmp/nonexistent-sandstar-test-socket");
        // If we get here without panicking, the test passes
    }

    #[tokio::test]
    async fn test_round_trip_frame() {
        use sandstar_ipc::protocol::{read_frame, write_frame};
        use sandstar_ipc::types::{EngineCommand, EngineResponse};

        let listener = create_listener("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Client sends a command
        let handle = std::thread::spawn(move || {
            let mut client = std::net::TcpStream::connect(addr).unwrap();
            let cmd = EngineCommand::Status;
            write_frame(&mut client, &cmd).unwrap();

            // Read back the response
            let resp: EngineResponse = read_frame(&mut client).unwrap().unwrap();
            resp
        });

        // Server accepts and echoes back a response
        let (mut stream, _, _) = accept(&listener).await.unwrap();
        let cmd: EngineCommand = sandstar_ipc::protocol::read_frame(&mut stream)
            .unwrap()
            .unwrap();
        assert!(matches!(cmd, EngineCommand::Status));

        let resp = EngineResponse::Ok;
        sandstar_ipc::protocol::write_frame(&mut stream, &resp).unwrap();

        let client_resp = handle.join().unwrap();
        assert!(matches!(client_resp, EngineResponse::Ok));
    }

    #[tokio::test]
    async fn test_concurrent_connections() {
        let listener = create_listener("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Three sequential clients connect and exchange a frame
        for i in 0..3 {
            let addr_clone = addr;
            let handle = std::thread::spawn(move || {
                let mut client = std::net::TcpStream::connect(addr_clone).unwrap();
                let cmd = sandstar_ipc::types::EngineCommand::ListChannels;
                sandstar_ipc::protocol::write_frame(&mut client, &cmd).unwrap();
            });

            let (mut stream, _, _) = accept(&listener).await.unwrap();
            let cmd: sandstar_ipc::types::EngineCommand =
                sandstar_ipc::protocol::read_frame(&mut stream)
                    .unwrap()
                    .unwrap();
            assert!(
                matches!(cmd, sandstar_ipc::types::EngineCommand::ListChannels),
                "client {} should send ListChannels",
                i
            );

            handle.join().unwrap();
        }
    }
}
