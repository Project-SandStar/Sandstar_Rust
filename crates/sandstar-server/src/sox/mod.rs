//! SOX (Sedona Object eXchange) protocol implementation.
//!
//! This module provides the binary wire-protocol parser and builder for SOX,
//! the native communication protocol used by Sedona Framework devices.
//!
//! # Sub-modules
//!
//! - [`dasp`] — DASP transport layer (UDP sessions, authentication, reliability)
//! - [`sox_protocol`] — SOX command codec
//! - [`sox_handlers`] — SOX command dispatch

pub mod dasp;
pub mod sox_handlers;
pub mod sox_protocol;

pub use sox_protocol::*;

use crate::sox::dasp::DaspTransport;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{error, info};

/// Start the SOX/DASP server as a background tokio task.
///
/// The server binds a UDP socket on `port`, runs the DASP handshake for
/// incoming connections, and dispatches authenticated SOX datagrams.
///
/// Returns a `JoinHandle` that can be used to await or abort the task.
pub fn spawn_sox_server(
    port: u16,
    username: String,
    password: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run_sox_server(port, username, password).await;
    })
}

/// Main SOX server loop.
///
/// This runs on a dedicated tokio task. The DASP transport uses non-blocking
/// UDP, so we yield periodically to avoid busy-spinning.
async fn run_sox_server(port: u16, username: String, password: String) {
    let mut transport = match DaspTransport::bind(port, &username, &password) {
        Ok(t) => t,
        Err(e) => {
            error!("Failed to bind DASP transport on port {port}: {e}");
            return;
        }
    };

    info!("SOX/DASP server listening on UDP port {port}");

    let mut cleanup_interval = tokio::time::interval(Duration::from_secs(10));

    loop {
        // Poll for incoming packets (non-blocking)
        if let Some((session_id, payload)) = transport.poll() {
            // TODO: dispatch SOX payload to sox_handlers
            info!(
                "SOX datagram from session 0x{session_id:04x}: {} bytes",
                payload.len()
            );
        }

        // Periodic cleanup
        tokio::select! {
            _ = cleanup_interval.tick() => {
                transport.cleanup_expired();
            }
            _ = tokio::time::sleep(Duration::from_millis(10)) => {
                // Small yield to avoid busy-spinning
            }
        }
    }
}
