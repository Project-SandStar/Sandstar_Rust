//! Phase 9 validation: two-node roxWarp gossip convergence.
//!
//! Phase 9 has ~5,000 LOC of in-tree implementation with 126 unit tests,
//! but before this file existed there was zero end-to-end proof that the
//! hello → welcome → delta → heartbeat flow actually converges two live
//! `DeltaEngine`s over a real WebSocket.
//!
//! Each test spins up node A as an Axum server on an ephemeral localhost
//! port, then spawns the client-side `connect_to_peer` loop for node B
//! pointing at A. After the handshake, a `record_change` on A should
//! propagate to B within a handful of heartbeat ticks.
//!
//! Tests run with plain WebSocket (no mTLS) and JSON debug framing to
//! keep the failure mode inspectable — the protocol's binary MessagePack
//! path is already exercised by 36 unit tests in `protocol::tests`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sandstar_server::roxwarp::{
    cluster::{ClusterConfig, DeltaEngine, PeerConfig, PeerState},
    handler::roxwarp_upgrade,
    RoxWarpState,
};
use tokio::sync::RwLock;

// ── Helpers ────────────────────────────────────────────────

async fn bind_ephemeral() -> (tokio::net::TcpListener, u16) {
    let lst = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let port = lst.local_addr().expect("local_addr").port();
    (lst, port)
}

fn test_config(node_id: &str, port: u16) -> ClusterConfig {
    ClusterConfig {
        node_id: node_id.to_string(),
        port,
        peers: vec![],
        heartbeat_interval_secs: 1,
        anti_entropy_interval_secs: 10,
        feed_interval_secs: 60,
        cert_path: None,
        key_path: None,
        ca_path: None,
    }
}

async fn start_server(listener: tokio::net::TcpListener, state: RoxWarpState) {
    use axum::routing::get;
    let app = axum::Router::new()
        .route("/roxwarp", get(roxwarp_upgrade))
        .with_state(state);
    let _ = axum::serve(listener, app).await;
}

/// Start an outbound peer session (client) from `local_engine` to `peer_addr`.
/// Returns the spawned task handle so the caller can drop it at end-of-test.
fn spawn_client_peer(
    peer_node_id: &str,
    peer_addr: String,
    local_engine: Arc<DeltaEngine>,
    peer_states: Arc<RwLock<HashMap<String, PeerState>>>,
) -> tokio::task::JoinHandle<()> {
    let peer = PeerConfig {
        node_id: peer_node_id.to_string(),
        address: peer_addr,
        enabled: true,
    };
    tokio::spawn(async move {
        sandstar_server::roxwarp::peer::connect_to_peer(
            &peer,
            local_engine,
            peer_states,
            1,    // heartbeat_secs — fast for tests
            10,   // anti_entropy_secs
            None, // no mTLS
            true, // JSON debug framing
        )
        .await;
    })
}

/// Poll `engine` until it has a point with the given `channel`, or timeout.
/// Returns `Some(point)` on success, `None` on timeout.
async fn wait_for_point(
    engine: &DeltaEngine,
    channel: u32,
    timeout: Duration,
) -> Option<sandstar_server::roxwarp::cluster::VersionedPoint> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let (_, points) = engine.full_state().await;
        if let Some(p) = points.into_iter().find(|p| p.channel == channel) {
            return Some(p);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

// ── Tests ──────────────────────────────────────────────────

/// After a handshake completes, a change recorded on node A should
/// propagate to node B via the handler's heartbeat-driven delta push.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_converge_on_single_change() {
    let engine_a = Arc::new(DeltaEngine::new("node-a".into()));
    let engine_b = Arc::new(DeltaEngine::new("node-b".into()));

    // Start node A's server.
    let (lst_a, port_a) = bind_ephemeral().await;
    let state_a = RoxWarpState {
        delta_engine: engine_a.clone(),
        config: test_config("node-a", port_a),
        sox_tree: None,
    };
    let _server_a = tokio::spawn(start_server(lst_a, state_a));

    // Node B connects to A as a client.
    let peer_states_b: Arc<RwLock<HashMap<String, PeerState>>> = Default::default();
    let _client_b = spawn_client_peer(
        "node-a",
        format!("127.0.0.1:{port_a}"),
        engine_b.clone(),
        peer_states_b.clone(),
    );

    // Let the handshake settle.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Record a change on node A. The value should land in node B on the
    // next heartbeat tick (≤ ~1 s).
    let _v = engine_a.record_change(102, 72.5, "°F", "ok").await;

    let p = wait_for_point(&engine_b, 102, Duration::from_secs(5))
        .await
        .expect("node B should converge to node A's channel 102 change");

    assert!(
        (p.value - 72.5).abs() < f64::EPSILON,
        "wrong value on B: {}",
        p.value
    );
    assert_eq!(p.status, "ok");
    assert_eq!(p.unit, "°F");
}

/// Both directions: change on A → B, then change on B → A.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn convergence_is_bidirectional() {
    let engine_a = Arc::new(DeltaEngine::new("node-a".into()));
    let engine_b = Arc::new(DeltaEngine::new("node-b".into()));

    let (lst_a, port_a) = bind_ephemeral().await;
    let state_a = RoxWarpState {
        delta_engine: engine_a.clone(),
        config: test_config("node-a", port_a),
        sox_tree: None,
    };
    let _server_a = tokio::spawn(start_server(lst_a, state_a));

    let peer_states_b: Arc<RwLock<HashMap<String, PeerState>>> = Default::default();
    let _client_b = spawn_client_peer(
        "node-a",
        format!("127.0.0.1:{port_a}"),
        engine_b.clone(),
        peer_states_b.clone(),
    );

    tokio::time::sleep(Duration::from_millis(500)).await;

    // A → B
    engine_a.record_change(200, 21.0, "°C", "ok").await;
    let p_on_b = wait_for_point(&engine_b, 200, Duration::from_secs(5))
        .await
        .expect("A→B: channel 200 missing on B");
    assert!((p_on_b.value - 21.0).abs() < f64::EPSILON);

    // B → A  (tests the outbound client's own delta-push loop)
    engine_b.record_change(300, 55.0, "%RH", "ok").await;
    let p_on_a = wait_for_point(&engine_a, 300, Duration::from_secs(5))
        .await
        .expect("B→A: channel 300 missing on A");
    assert!((p_on_a.value - 55.0).abs() < f64::EPSILON);
    assert_eq!(p_on_a.unit, "%RH");
}

/// A pre-existing change on node A (recorded before B connects) should
/// reach B via the initial `delta_for_peer` push in the welcome sequence,
/// NOT via the later heartbeat loop. This exercises "new peer catches up
/// to existing state".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_peer_receives_backlog_on_handshake() {
    let engine_a = Arc::new(DeltaEngine::new("node-a".into()));
    let engine_b = Arc::new(DeltaEngine::new("node-b".into()));

    // Pre-load A with state BEFORE starting the server.
    engine_a.record_change(500, 99.9, "Pa", "ok").await;
    engine_a.record_change(501, 42.0, "Pa", "ok").await;

    let (lst_a, port_a) = bind_ephemeral().await;
    let state_a = RoxWarpState {
        delta_engine: engine_a.clone(),
        config: test_config("node-a", port_a),
        sox_tree: None,
    };
    let _server_a = tokio::spawn(start_server(lst_a, state_a));

    let peer_states_b: Arc<RwLock<HashMap<String, PeerState>>> = Default::default();
    let _client_b = spawn_client_peer(
        "node-a",
        format!("127.0.0.1:{port_a}"),
        engine_b.clone(),
        peer_states_b.clone(),
    );

    // Even though no record_change happens after B connects, the backlog
    // should land during the handshake's delta_for_peer push (≤ 1 s).
    let p500 = wait_for_point(&engine_b, 500, Duration::from_secs(3))
        .await
        .expect("backlog: channel 500 missing on B");
    let p501 = wait_for_point(&engine_b, 501, Duration::from_secs(3))
        .await
        .expect("backlog: channel 501 missing on B");
    assert!((p500.value - 99.9).abs() < f64::EPSILON);
    assert!((p501.value - 42.0).abs() < f64::EPSILON);
}
