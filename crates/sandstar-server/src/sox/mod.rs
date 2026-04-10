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
pub mod dyn_slots;
pub mod name_intern;
pub mod sox_handlers;
pub mod sox_protocol;

pub use sox_protocol::*;

use crate::rest::EngineHandle;
use crate::sox::dasp::DaspTransport;
use crate::sox::dyn_slots::DynSlotStore;
use crate::sox::sox_handlers::{
    handle_put_chunk, handle_sox_request_with_dyn, is_put_transfer_active, parse_write_request,
    ComponentTree, ManifestDb, SubscriptionManager,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Default manifest directory on the BeagleBone device.
pub const DEFAULT_MANIFESTS_DIR: &str = "/home/eacio/sandstar/etc/manifests";

/// Start the SOX/DASP server as a background tokio task.
///
/// The server binds a UDP socket on `port`, runs the DASP handshake for
/// incoming connections, and dispatches authenticated SOX datagrams.
/// Channel data and writes are proxied through the `EngineHandle`.
///
/// `manifests_dir` is the path to the kit manifest XML files. If `None`,
/// uses the default path (`/home/eacio/sandstar/etc/manifests`).
///
/// Returns a `JoinHandle` that can be used to await or abort the task.
/// Thread-safe handle to a shared [`DynSlotStore`].
pub type DynSlotStoreHandle = Arc<std::sync::RwLock<DynSlotStore>>;

/// Thread-safe handle to a shared [`ComponentTree`].
///
/// Uses `std::sync::RwLock` (not tokio) because all tree operations are
/// fast in-memory ops with no I/O.
pub type SharedComponentTree = Arc<std::sync::RwLock<ComponentTree>>;

/// Result from spawning the SOX server: the join handle, shared tree, and manifest DB.
pub struct SoxServerHandles {
    pub join_handle: JoinHandle<()>,
    pub tree: SharedComponentTree,
    pub manifest_db: Arc<ManifestDb>,
}

pub fn spawn_sox_server(
    port: u16,
    username: String,
    password: String,
    engine_handle: EngineHandle,
    manifests_dir: Option<String>,
    dyn_store: Option<DynSlotStoreHandle>,
) -> SoxServerHandles {
    // Load manifest database synchronously before spawning the async task.
    let dir = manifests_dir.unwrap_or_else(|| DEFAULT_MANIFESTS_DIR.to_string());
    let manifest_db = Arc::new(ManifestDb::load(&dir));

    // Build the initial tree synchronously so we can share it with REST.
    // The async task will populate it from channels on its first tick.
    let tree = Arc::new(std::sync::RwLock::new(ComponentTree::new_with_manifest(
        manifest_db.clone(),
    )));

    let tree_clone = tree.clone();
    let manifest_db_clone = manifest_db.clone();

    let join_handle = tokio::spawn(async move {
        run_sox_server(
            port,
            username,
            password,
            engine_handle,
            manifest_db_clone,
            dyn_store,
            tree_clone,
        )
        .await;
    });

    SoxServerHandles {
        join_handle,
        tree,
        manifest_db,
    }
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
    manifest_db: Arc<ManifestDb>,
    dyn_store_handle: Option<DynSlotStoreHandle>,
    shared_tree: SharedComponentTree,
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
    {
        let initial_tree = match engine_handle.list_channels().await {
            Ok(channels) => {
                let t = ComponentTree::from_channels_with_manifest(&channels, manifest_db.clone());
                info!(
                    components = t.len(),
                    manifest_types = manifest_db.type_count(),
                    "SOX component tree built"
                );
                t
            }
            Err(e) => {
                warn!("SOX: failed to get initial channels: {e}, starting with empty tree");
                ComponentTree::new_with_manifest(manifest_db.clone())
            }
        };
        // Replace the empty shared tree with the fully initialized one.
        *shared_tree.write().unwrap() = initial_tree;
    }

    // Load persisted user-added components from disk (survives restarts).
    {
        let mut tree = shared_tree.write().unwrap();
        match tree.load_user_components() {
            Ok(0) => debug!("SOX: no persisted components to load"),
            Ok(n) => info!(count = n, "SOX: restored persisted user components"),
            Err(e) => warn!("SOX: failed to load persisted components: {e}"),
        }
    }

    // Dynamic slot store (side-car tag dictionaries for components).
    // If a shared handle was provided, use it; otherwise create a local one.
    let dyn_store_handle = dyn_store_handle
        .unwrap_or_else(|| Arc::new(std::sync::RwLock::new(DynSlotStore::with_defaults())));
    let dyn_persist_path = {
        let config_dir = std::env::var("SANDSTAR_CONFIG_DIR")
            .unwrap_or_else(|_| "/home/eacio/sandstar/etc/config".to_string());
        format!("{config_dir}/dyn_slots.json")
    };

    // Load persisted dynamic tags from disk (survives restarts).
    {
        let mut ds = dyn_store_handle.write().unwrap();
        match ds.load(&dyn_persist_path) {
            Ok(0) => debug!("dyn_slots: no persisted tags to load"),
            Ok(n) => info!(count = n, "dyn_slots: restored persisted tags"),
            Err(e) => warn!("dyn_slots: failed to load persisted tags: {e}"),
        }
    }

    let mut subscriptions = SubscriptionManager::new();

    // Timers
    let mut cleanup_interval = tokio::time::interval(Duration::from_secs(10));
    cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut tree_refresh_interval = tokio::time::interval(Duration::from_secs(1));
    tree_refresh_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Persistence save timer: check dirty flag every 5 seconds.
    let mut persist_interval = tokio::time::interval(Duration::from_secs(5));
    persist_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut force_full_cov = false;
    let mut first_cov_burst = false; // true on the tick immediately after subscribe
    let mut pending_cov: std::collections::VecDeque<u16> = std::collections::VecDeque::new();

    loop {
        // 1. Poll for incoming DASP packets (non-blocking).
        //    Process up to 16 packets per iteration to drain bursts without starving timers.
        //    Acquire the tree write lock for the entire packet processing section
        //    (no .await points here, so holding std::sync::RwLock is safe).
        let mut packets_this_round = 0;
        {
            let mut tree = shared_tree.write().unwrap();
            while packets_this_round < 16 {
                match transport.poll() {
                    Some((session_id, payload)) => {
                        packets_this_round += 1;

                        // Intercept 'k' (0x6B) during active PUT transfer — route to
                        // chunk handler instead of the normal Invoke handler.
                        // In Sedona, 'k' is used for both invoke and file chunk transfer.
                        // During a put, chunks are received silently (no response).
                        if payload.len() >= 2 && payload[0] == b'k' && is_put_transfer_active() {
                            debug!(
                                session = session_id,
                                "SOX: routing 'k' to put chunk handler"
                            );
                            let chunk_payload = &payload[2..]; // skip cmd + replyNum
                            handle_put_chunk(chunk_payload);
                            continue; // no response for put chunks
                        }

                        if let Some(request) = SoxRequest::parse(&payload) {
                            // Log write/invoke at info level, everything else at debug
                            if request.cmd as u8 == b'w' || request.cmd as u8 == b'k' {
                                info!(
                                    session = session_id,
                                    cmd = request.cmd as u8,
                                    req_id = request.req_id,
                                    "SOX write/invoke received"
                                );
                            } else {
                                debug!(
                                    session = session_id,
                                    cmd = request.cmd as u8,
                                    req_id = request.req_id,
                                    "SOX request"
                                );
                            }
                            // Handle write commands: forward to engine via EngineHandle.
                            if request.cmd == sox_protocol::SoxCmd::Write {
                                if let Some(write_req) = parse_write_request(&request, &tree) {
                                    info!(
                                        comp_id = write_req.comp_id,
                                        slot_id = write_req.slot_id,
                                        "SOX: write parsed"
                                    );
                                    if let Some((channel_id, value)) =
                                        write_req.to_channel_write(&tree)
                                    {
                                        info!(channel_id, value, "SOX: writing to engine");
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

                            // Save comp_id + parent_id before delete (component removed by handle_sox_request)
                            let delete_comp_id = if request.cmd as u8 == b'd'
                                && request.payload.len() >= 2
                            {
                                Some(u16::from_be_bytes([request.payload[0], request.payload[1]]))
                            } else {
                                None
                            };
                            let delete_parent_id = if let Some(cid) = delete_comp_id {
                                tree.get(cid).map(|c| c.parent_id).unwrap_or(0)
                            } else {
                                0
                            };

                            let response = handle_sox_request_with_dyn(
                                &request,
                                &mut tree,
                                &mut subscriptions,
                                session_id,
                                Some(&dyn_store_handle),
                            );
                            let response_bytes = response.to_bytes();
                            if let Err(e) = transport.send_to_session(session_id, &response_bytes) {
                                debug!(session = session_id, "SOX: failed to send response: {e}");
                            }

                            // After Delete: clean up dynamic tags for the deleted component.
                            if let Some(cid) = delete_comp_id {
                                if let Ok(mut ds) = dyn_store_handle.write() {
                                    if ds.tag_count(cid) > 0 {
                                        info!(
                                            comp_id = cid,
                                            "dyn_slots: cleaning up tags for deleted component"
                                        );
                                        ds.remove_all(cid);
                                    }
                                }
                            }

                            // After Write to a channel component: push COV event.
                            if request.cmd as u8 == b'w' && request.payload.len() >= 2 {
                                let written_comp =
                                    u16::from_be_bytes([request.payload[0], request.payload[1]]);
                                if tree.is_channel_comp(written_comp) {
                                    let events = subscriptions.build_events(&[written_comp], &tree);
                                    for (sid, evt) in events {
                                        let _ = transport.send_to_session(sid, &evt);
                                    }
                                }
                            }

                            // After invoke ('k') or write ('w') on a non-channel component:
                            // push a CONFIG COV event so the editor updates displayed values.
                            // This is needed for components like ConstFloat where "out" is a
                            // config slot — the editor's applyProps for 'c' reads config slots.
                            if (request.cmd as u8 == b'k' || request.cmd as u8 == b'w')
                                && request.payload.len() >= 2
                            {
                                let target_comp =
                                    u16::from_be_bytes([request.payload[0], request.payload[1]]);
                                if !tree.is_channel_comp(target_comp) {
                                    let events =
                                        subscriptions.build_config_events(target_comp, &tree);
                                    for (sid, evt) in events {
                                        let _ = transport.send_to_session(sid, &evt);
                                    }
                                }
                            }

                            // After Add/Delete: push tree event for the parent so editor refreshes
                            if request.cmd as u8 == b'a' || request.cmd as u8 == b'd' {
                                // For Add: payload[0..2] = parentId
                                // For Delete: payload[0..2] = compId (deleted), look up parent from saved value
                                let parent_id =
                                    if request.cmd as u8 == b'a' && request.payload.len() >= 2 {
                                        u16::from_be_bytes([request.payload[0], request.payload[1]])
                                    } else {
                                        delete_parent_id // saved before handle_sox_request
                                    };
                                // Send tree event for parent component
                                if let Some(parent) = tree.get(parent_id) {
                                    let mut evt = Vec::with_capacity(64);
                                    evt.push(b'e');
                                    evt.push(0xFF);
                                    evt.extend_from_slice(&parent_id.to_be_bytes());
                                    evt.push(b't'); // tree change
                                    evt.push(parent.kit_id);
                                    evt.push(parent.type_id);
                                    // name (null-terminated)
                                    evt.extend_from_slice(parent.name.as_bytes());
                                    evt.push(0x00);
                                    evt.extend_from_slice(&parent.parent_id.to_be_bytes());
                                    evt.push(0xFF); // permissions
                                    evt.push(parent.children.len() as u8);
                                    for &child_id in &parent.children {
                                        evt.extend_from_slice(&child_id.to_be_bytes());
                                    }
                                    // Also send tree event for the new/deleted component itself
                                    let _ = transport.send_to_session(session_id, &evt);
                                }
                                // If it was an Add, also send tree event for the new component
                                if request.cmd as u8 == b'a' {
                                    let new_id =
                                        u16::from_be_bytes([response_bytes[2], response_bytes[3]]);
                                    if let Some(comp) = tree.get(new_id) {
                                        let mut evt = Vec::with_capacity(64);
                                        evt.push(b'e');
                                        evt.push(0xFF);
                                        evt.extend_from_slice(&new_id.to_be_bytes());
                                        evt.push(b't');
                                        evt.push(comp.kit_id);
                                        evt.push(comp.type_id);
                                        evt.extend_from_slice(comp.name.as_bytes());
                                        evt.push(0x00);
                                        evt.extend_from_slice(&comp.parent_id.to_be_bytes());
                                        evt.push(0xFF);
                                        evt.push(0); // no children
                                        let _ = transport.send_to_session(session_id, &evt);
                                    }
                                }
                            }

                            // After Link add/delete: push LINKS COV events for affected components.
                            if request.cmd as u8 == b'l'
                                && response_bytes[0] == b'L'
                                && request.payload.len() >= 7
                            {
                                // Parse the affected comp IDs from the link request payload
                                // payload: u1 subcmd, u2 fromComp, u1 fromSlot, u2 toComp, u1 toSlot
                                let from_comp =
                                    u16::from_be_bytes([request.payload[1], request.payload[2]]);
                                let to_comp =
                                    u16::from_be_bytes([request.payload[4], request.payload[5]]);
                                // Send links COV event for both affected components
                                for &affected_id in &[from_comp, to_comp] {
                                    if let Some(comp) = tree.get(affected_id) {
                                        let mut evt = Vec::with_capacity(64);
                                        evt.push(b'e');
                                        evt.push(0xFF);
                                        evt.extend_from_slice(&affected_id.to_be_bytes());
                                        evt.push(b'l'); // what = links
                                                        // Write links for this component + 0xFFFF terminator
                                        for link in &comp.links {
                                            evt.extend_from_slice(&link.from_comp.to_be_bytes());
                                            evt.push(link.from_slot);
                                            evt.extend_from_slice(&link.to_comp.to_be_bytes());
                                            evt.push(link.to_slot);
                                        }
                                        evt.extend_from_slice(&0xFFFFu16.to_be_bytes());
                                        let _ = transport.send_to_session(session_id, &evt);
                                    }
                                }
                            }

                            // After subscribe: queue all channel components for COV push.
                            if request.cmd as u8 == b's' {
                                force_full_cov = true;
                                // Also push link events for all components that have links
                                for comp_id in tree.comp_ids() {
                                    if let Some(comp) = tree.get(comp_id) {
                                        if !comp.links.is_empty() {
                                            let mut evt = Vec::with_capacity(64);
                                            evt.push(b'e');
                                            evt.push(0xFF);
                                            evt.extend_from_slice(&comp_id.to_be_bytes());
                                            evt.push(b'l');
                                            for link in &comp.links {
                                                evt.extend_from_slice(
                                                    &link.from_comp.to_be_bytes(),
                                                );
                                                evt.push(link.from_slot);
                                                evt.extend_from_slice(&link.to_comp.to_be_bytes());
                                                evt.push(link.to_slot);
                                            }
                                            evt.extend_from_slice(&0xFFFFu16.to_be_bytes());
                                            let _ = transport.send_to_session(session_id, &evt);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    None => break,
                }
            }
        } // drop tree write lock before await points

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
                // Acquire channel list first (async), then lock tree for mutation.
                match engine_handle.list_channels().await {
                    Ok(channels) => {
                        // All tree operations below are synchronous — safe to hold std::sync::RwLock.
                        let mut tree = shared_tree.write().unwrap();

                        let changed = tree.update_from_channels(&channels);

                        // Execute link propagation and component logic
                        let link_changed = tree.execute_links();
                        let comp_changed = tree.execute_components();
                        if !link_changed.is_empty() || !comp_changed.is_empty() {
                            info!(links = link_changed.len(), comps = comp_changed.len(), "SOX: dataflow executed");
                        }

                        // Logic→Channel bridge: when a logic component output is
                        // wired to a channel component's "out" slot, forward the
                        // value to the engine so it reaches real hardware.
                        let all_changed: Vec<u16> = link_changed.iter()
                            .chain(comp_changed.iter())
                            .copied()
                            .collect();
                        let channel_writes = tree.collect_channel_writes(&all_changed);
                        // Collect writes to spawn after dropping the lock.
                        let writes_to_spawn = channel_writes;

                        // On first tick after subscribe, queue ALL channel comps for burst push
                        if force_full_cov {
                            force_full_cov = false;
                            first_cov_burst = true;
                            pending_cov.clear();
                            for id in sox_handlers::CHANNEL_COMP_BASE..tree.channel_comp_end {
                                pending_cov.push_back(id);
                            }
                            // Also queue non-channel components
                            for &id in link_changed.iter().chain(comp_changed.iter()) {
                                if !pending_cov.contains(&id) {
                                    pending_cov.push_back(id);
                                }
                            }
                            info!(count = pending_cov.len(), "SOX: queued full COV after subscribe (burst mode)");
                        }

                        // Add newly changed comps to pending queue
                        for id in &changed {
                            if !pending_cov.contains(id) {
                                pending_cov.push_back(*id);
                            }
                        }

                        // Push BOTH config AND runtime COV events for non-channel components
                        // that changed (link propagation or component execution results).
                        // Config events update config slots (meta, out for ConstFloat).
                        // Runtime events update runtime slots (in1, in2, out for Add2).
                        for &id in link_changed.iter().chain(comp_changed.iter()) {
                            if !tree.is_channel_comp(id) {
                                // Config COV
                                let events = subscriptions.build_config_events(id, &tree);
                                for (session_id, event_bytes) in events {
                                    let _ = transport.send_to_session(session_id, &event_bytes);
                                }
                                // Runtime COV (for slots like in1, in2 on Add2)
                                if let Some(comp) = tree.get(id) {
                                    let watchers: Vec<u16> = subscriptions.get_watchers(id)
                                        .map(|w| w.iter().copied().collect())
                                        .unwrap_or_default();
                                    if !watchers.is_empty() {
                                        let mut evt = Vec::with_capacity(64);
                                        evt.push(b'e');
                                        evt.push(0xFF);
                                        evt.extend_from_slice(&id.to_be_bytes());
                                        evt.push(b'r'); // runtime
                                        for slot in &comp.slots {
                                            if slot.flags & sox_handlers::SLOT_FLAG_ACTION != 0 { continue; }
                                            if slot.flags & sox_handlers::SLOT_FLAG_CONFIG != 0 { continue; }
                                            sox_handlers::encode_slot_value_raw(&mut evt, &slot.value);
                                        }
                                        for sid in &watchers {
                                            let _ = transport.send_to_session(*sid, &evt);
                                        }
                                    }
                                }
                            }
                        }

                        // Push COV events with proper seq_nums (required for editor to process).
                        // First tick after subscribe: burst up to 200 events to populate
                        // the editor immediately (~150 channels sent in one shot).
                        // Normal ticks: 50 events per 1-second tick. DASP ACKs return
                        // within ~100ms, so 50/tick is well within the send window.
                        let batch_limit = if first_cov_burst {
                            first_cov_burst = false;
                            200
                        } else {
                            50
                        };
                        let mut sent = 0;
                        while sent < batch_limit {
                            let Some(comp_id) = pending_cov.pop_front() else { break };
                            let events = subscriptions.build_events(&[comp_id], &tree);
                            for (session_id, event_bytes) in events {
                                let _ = transport.send_to_session(session_id, &event_bytes);
                                sent += 1;
                            }
                        }

                        // Drop tree lock before spawning async writes.
                        drop(tree);

                        // Fire-and-forget channel writes from logic→channel bridge.
                        for (channel_id, value) in writes_to_spawn {
                            info!(channel_id, value, "SOX: logic→channel write");
                            let handle = engine_handle.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle
                                    .write_channel(
                                        channel_id,
                                        Some(value),
                                        17, // priority level 17 (default) — same as browser writes
                                        "sox-logic".to_string(),
                                        0.0,
                                    )
                                    .await
                                {
                                    warn!(
                                        channel = channel_id,
                                        "SOX logic→channel write failed: {e}"
                                    );
                                }
                            });
                        }
                    }
                    Err(e) => {
                        debug!("SOX: failed to refresh channels: {e}");
                    }
                }
            }
            _ = persist_interval.tick() => {
                // Save user-added components to disk if dirty.
                let mut tree = shared_tree.write().unwrap();
                if tree.take_dirty() {
                    if let Err(e) = tree.save_user_components() {
                        warn!("SOX: failed to save user components: {e}");
                    }
                }
                drop(tree);
                // Save dynamic slot store to disk if dirty.
                if let Ok(mut ds) = dyn_store_handle.write() {
                    if ds.take_dirty() {
                        if let Err(e) = ds.save(&dyn_persist_path) {
                            warn!("dyn_slots: failed to save: {e}");
                        }
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(10)) => {
                // Small yield to avoid busy-spinning when no timers fire.
            }
        }
    }
}
