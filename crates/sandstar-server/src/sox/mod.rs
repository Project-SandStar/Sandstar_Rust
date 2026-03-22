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

use crate::rest::EngineHandle;
use crate::sox::dasp::DaspTransport;
use crate::sox::sox_handlers::{
    handle_sox_request, parse_write_request, ComponentTree, SubscriptionManager,
};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Start the SOX/DASP server as a background tokio task.
///
/// The server binds a UDP socket on `port`, runs the DASP handshake for
/// incoming connections, and dispatches authenticated SOX datagrams.
/// Channel data and writes are proxied through the `EngineHandle`.
///
/// Returns a `JoinHandle` that can be used to await or abort the task.
pub fn spawn_sox_server(
    port: u16,
    username: String,
    password: String,
    engine_handle: EngineHandle,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run_sox_server(port, username, password, engine_handle).await;
    })
}

/// Main SOX server loop.
///
/// This runs on a dedicated tokio task. The DASP transport uses non-blocking
/// UDP, so we yield periodically to avoid busy-spinning.
///
/// The loop:
/// 1. Polls for incoming DASP packets and dispatches SOX commands.
/// 2. Periodically refreshes the virtual component tree from engine channel data.
/// 3. Sends COV event payloads to subscribed sessions.
/// 4. Cleans up expired DASP sessions and their subscriptions.
async fn run_sox_server(
    port: u16,
    username: String,
    password: String,
    engine_handle: EngineHandle,
) {
    let mut transport = match DaspTransport::bind(port, &username, &password) {
        Ok(t) => t,
        Err(e) => {
            error!("SOX server failed to bind on port {port}: {e}");
            return;
        }
    };

    info!(port, "SOX/DASP server listening");

    // Build initial component tree from current channel data.
    // Values will be corrected on the first `update_from_channels` tick.
    let mut tree = match engine_handle.list_channels().await {
        Ok(channels) => {
            let t = ComponentTree::from_channels(&channels);
            info!(components = t.len(), "SOX component tree built");
            t
        }
        Err(e) => {
            warn!("SOX: failed to get initial channels: {e}, starting with empty tree");
            ComponentTree::new()
        }
    };

    let mut subscriptions = SubscriptionManager::new();

    // Timers
    let mut cleanup_interval = tokio::time::interval(Duration::from_secs(10));
    cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut tree_refresh_interval = tokio::time::interval(Duration::from_secs(1));
    tree_refresh_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // 1. Poll for incoming DASP packets (non-blocking).
        //    Process up to 16 packets per iteration to drain bursts without starving timers.
        let mut packets_this_round = 0;
        while packets_this_round < 16 {
            match transport.poll() {
                Some((session_id, payload)) => {
                    packets_this_round += 1;
                    if let Some(request) = SoxRequest::parse(&payload) {
                        debug!(session = session_id, cmd = request.cmd as u8, req_id = request.req_id, "SOX request");
                        // Handle write commands: forward to engine via EngineHandle.
                        if request.cmd == sox_protocol::SoxCmd::Write {
                            if let Some(write_req) = parse_write_request(&request) {
                                if let Some((channel_id, value)) =
                                    write_req.to_channel_write(&tree)
                                {
                                    let handle = engine_handle.clone();
                                    // Fire-and-forget write (don't block the SOX loop).
                                    tokio::spawn(async move {
                                        if let Err(e) = handle
                                            .write_channel(
                                                channel_id,
                                                Some(value),
                                                8, // priority level 8 (operator)
                                                "sox".to_string(),
                                                0.0,
                                            )
                                            .await
                                        {
                                            warn!(
                                                channel = channel_id,
                                                "SOX write failed: {e}"
                                            );
                                        }
                                    });
                                }
                            }
                        }

                        let response =
                            handle_sox_request(&request, &tree, &mut subscriptions, session_id);
                        let response_bytes = response.to_bytes();
                        if let Err(e) =
                            transport.send_to_session(session_id, &response_bytes)
                        {
                            debug!(
                                session = session_id,
                                "SOX: failed to send response: {e}"
                            );
                        }

                        // After batchSubscribe: push initial state events for all subscribed components
                        if request.cmd as u8 == b's' && request.payload.len() > 3 {
                            // batchSubscribe detected (>3 bytes = not doSubscribe)
                            let all_comp_ids = tree.comp_ids();
                            let initial_events = subscriptions.build_events(&all_comp_ids, &tree);
                            if !initial_events.is_empty() {
                                info!(session = session_id, count = initial_events.len(), "SOX: pushing initial state after batchSubscribe");
                                for (sid, evt) in initial_events {
                                    let _ = transport.send_to_session(sid, &evt);
                                }
                            }
                        }
                    }
                }
                None => break,
            }
        }

        // 2. Wait for timers (or a small sleep if no timers fire).
        tokio::select! {
            _ = cleanup_interval.tick() => {
                // Clean up expired DASP sessions and their subscriptions.
                let expired_sessions = transport.expired_session_ids();
                for sid in &expired_sessions {
                    subscriptions.unsubscribe_all(*sid);
                }
                transport.cleanup_expired();
            }
            _ = tree_refresh_interval.tick() => {
                // Refresh the component tree from engine channel data.
                match engine_handle.list_channels().await {
                    Ok(channels) => {
                        let changed = tree.update_from_channels(&channels);
                        // Push COV events to subscribed sessions.
                        if !changed.is_empty() {
                            let events = subscriptions.build_events(&changed, &tree);
                            if !events.is_empty() {
                                debug!(changed = changed.len(), events = events.len(), "SOX: pushing COV events");
                            }
                            for (session_id, event_bytes) in events {
                                if let Err(e) = transport.send_to_session(session_id, &event_bytes) {
                                    debug!(session = session_id, "SOX: COV send failed: {e}");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        debug!("SOX: failed to refresh channels: {e}");
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(10)) => {
                // Small yield to avoid busy-spinning when no timers fire.
            }
        }
    }
}
