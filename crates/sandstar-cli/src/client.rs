//! IPC client: connect to the engine server and exchange commands.

use sandstar_ipc::protocol::{read_frame, write_frame};
use sandstar_ipc::types::{EngineCommand, EngineResponse};
use std::io;
use std::net::TcpStream;

/// Send a command to the engine server and return the response.
pub fn send_command(address: &str, cmd: &EngineCommand) -> io::Result<EngineResponse> {
    let mut stream = connect(address)?;

    write_frame(&mut stream, cmd)?;

    match read_frame(&mut stream)? {
        Some(response) => Ok(response),
        None => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "server closed connection without responding",
        )),
    }
}

/// Connect to the engine server.
///
/// On Unix: tries Unix domain socket first if path doesn't contain ':'.
/// On Windows: always uses TCP.
#[cfg(unix)]
fn connect(address: &str) -> io::Result<ConnStream> {
    if !address.contains(':') {
        let stream = std::os::unix::net::UnixStream::connect(address)?;
        return Ok(ConnStream::Unix(stream));
    }
    let stream = TcpStream::connect(address)?;
    Ok(ConnStream::Tcp(stream))
}

#[cfg(not(unix))]
fn connect(address: &str) -> io::Result<TcpStream> {
    TcpStream::connect(address)
}

// --- Platform-specific stream type ---

#[cfg(unix)]
enum ConnStream {
    Unix(std::os::unix::net::UnixStream),
    Tcp(TcpStream),
}

#[cfg(unix)]
impl io::Read for ConnStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            ConnStream::Unix(s) => s.read(buf),
            ConnStream::Tcp(s) => s.read(buf),
        }
    }
}

#[cfg(unix)]
impl io::Write for ConnStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            ConnStream::Unix(s) => s.write(buf),
            ConnStream::Tcp(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            ConnStream::Unix(s) => s.flush(),
            ConnStream::Tcp(s) => s.flush(),
        }
    }
}
