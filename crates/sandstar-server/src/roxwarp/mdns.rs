//! mDNS peer discovery for roxWarp clusters (Phase 9.0b).
//!
//! Advertises this node on the local subnet as
//! `_sandstar-roxwarp._tcp.local.` with the node ID as the service
//! instance name and the cluster port as the service port. Browses for
//! the same service type and reports discovered peers via a callback
//! so the [`ClusterManager`](super::cluster::ClusterManager) can add
//! them to its peer table dynamically.
//!
//! mDNS is **opt-in** via `ClusterConfig.enable_mdns` (default `false`)
//! — when disabled, the cluster still works using the static `peers`
//! list. Both can be used simultaneously: mDNS-discovered peers are
//! merged into the same `peer_states` map that the static list
//! populates.
//!
//! ## Wire format
//!
//! Service type: `_sandstar-roxwarp._tcp.local.`
//! Instance name: `<node_id>._sandstar-roxwarp._tcp.local.`
//! TXT records: `node_id=<string>` (for future protocol-version
//! advertising; currently only the node id is included).
//!
//! ## Testing
//!
//! mDNS requires real multicast (UDP 5353), which is unavailable on
//! some CI / loopback-only setups. Integration tests that exercise
//! live discovery are gated with `#[ignore]` by default and run
//! manually with `cargo test -- --ignored`. Pure unit tests that don't
//! need the network still run in the default suite.

use std::collections::HashMap;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tracing::{debug, info, warn};

/// Service type we advertise and browse for. A `.local.` suffix is
/// required for per-subnet mDNS.
pub const SERVICE_TYPE: &str = "_sandstar-roxwarp._tcp.local.";

/// Handle returned from [`start_mdns`] — owns the underlying
/// [`ServiceDaemon`] so the caller's lifetime drives the advertiser's
/// lifetime.
pub struct MdnsHandle {
    daemon: ServiceDaemon,
    instance_fullname: String,
}

impl MdnsHandle {
    /// Gracefully unregister this node and stop the daemon.
    pub fn shutdown(self) {
        let _ = self.daemon.unregister(&self.instance_fullname);
        let _ = self.daemon.shutdown();
    }
}

/// A peer discovered via mDNS.
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    /// Peer's self-advertised node id (the instance name).
    pub node_id: String,
    /// Peer's reachable `host:port` address (first IPv4 address
    /// resolved from the mDNS record, combined with the advertised
    /// port).
    pub address: String,
}

/// Start mDNS advertising and browsing.
///
/// * Advertises this node as `<node_id>._sandstar-roxwarp._tcp.local.`
///   on `port`.
/// * Spawns a tokio task that consumes browse events and invokes
///   `on_discover` for each newly-resolved peer (excluding ourself).
///
/// Returns a handle whose `shutdown()` gracefully unregisters. Drop the
/// handle to stop advertising immediately (daemon thread will exit
/// when the channel closes).
pub fn start_mdns(
    node_id: &str,
    port: u16,
    on_discover: impl Fn(DiscoveredPeer) + Send + Sync + 'static,
) -> Result<MdnsHandle, String> {
    let daemon =
        ServiceDaemon::new().map_err(|e| format!("mdns-sd: failed to start daemon: {e}"))?;

    let my_id = node_id.to_string();

    // Advertise self. Host address is resolved by the daemon from the
    // local interfaces, so we don't need to pass an IP.
    let mut props: HashMap<String, String> = HashMap::new();
    props.insert("node_id".to_string(), my_id.clone());

    let host_name = format!("{my_id}.local.");
    let host_ipv4 = String::new(); // daemon will discover from interfaces

    let service = ServiceInfo::new(
        SERVICE_TYPE,
        &my_id,
        &host_name,
        &host_ipv4,
        port,
        Some(props),
    )
    .map_err(|e| format!("mdns-sd: ServiceInfo failed: {e}"))?
    .enable_addr_auto();
    let instance_fullname = service.get_fullname().to_string();

    daemon
        .register(service)
        .map_err(|e| format!("mdns-sd: register failed: {e}"))?;
    info!(node_id = %my_id, port, "roxWarp mDNS: advertising");

    // Browser: produce a channel of ServiceEvents the daemon will push into.
    let rx = daemon
        .browse(SERVICE_TYPE)
        .map_err(|e| format!("mdns-sd: browse failed: {e}"))?;

    let browser_id = my_id.clone();
    tokio::spawn(async move {
        loop {
            // mdns-sd's Receiver is blocking; use recv_timeout in a blocking context.
            let evt = match tokio::task::spawn_blocking({
                let rx = rx.clone();
                move || rx.recv_timeout(Duration::from_secs(10))
            })
            .await
            {
                Ok(Ok(e)) => e,
                Ok(Err(_)) => continue, // timeout — loop again
                Err(_) => break,        // task panicked
            };

            if let ServiceEvent::ServiceResolved(info) = evt {
                // instance name is the node id we advertised.
                let peer_id = info.get_fullname().split('.').next().unwrap_or("").to_string();
                if peer_id.is_empty() || peer_id == browser_id {
                    continue; // ignore ourself
                }
                let addrs = info.get_addresses();
                let Some(addr) = addrs.iter().next() else {
                    debug!(peer = %peer_id, "mDNS: resolved peer has no addresses yet");
                    continue;
                };
                let port = info.get_port();
                let peer = DiscoveredPeer {
                    node_id: peer_id,
                    address: format!("{addr}:{port}"),
                };
                debug!(peer = ?peer, "roxWarp mDNS: discovered peer");
                on_discover(peer);
            } else if let ServiceEvent::ServiceRemoved(_, fullname) = evt {
                debug!(peer = %fullname, "roxWarp mDNS: peer removed");
            }
        }
        warn!("roxWarp mDNS: browse loop exited");
    });

    Ok(MdnsHandle {
        daemon,
        instance_fullname,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_type_is_well_formed() {
        assert!(SERVICE_TYPE.ends_with("._tcp.local."));
        assert!(SERVICE_TYPE.starts_with("_sandstar"));
    }

    /// Sanity check: a fresh ServiceDaemon can be created on this host.
    /// This exercises the mdns-sd init path without actually performing
    /// any multicast I/O (the daemon starts a thread but we shut it
    /// down immediately).
    #[test]
    fn daemon_start_and_shutdown() {
        let daemon = ServiceDaemon::new().expect("start");
        let _ = daemon.shutdown();
    }
}
