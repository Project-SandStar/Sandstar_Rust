//! Sandstar Engine Server
//!
//! Single-threaded tokio runtime that runs the engine poll loop
//! and accepts IPC commands over a local socket.
//!
//! # Architecture
//!
//! ```text
//!              +---> Poll Timer (tokio::time::interval)
//!              |
//! Main Loop ---+---> IPC Commands (local socket listener)
//!              |
//!              +---> SIGHUP (config reload, Unix only)
//!              |
//!              +---> Shutdown Signal (Ctrl+C / SIGTERM)
//! ```

use std::future::Future;
use std::pin::Pin;
#[cfg(not(feature = "svm"))]
use std::sync::{Arc, Mutex};
#[cfg(feature = "svm")]
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use clap::Parser;
use sandstar_engine::Engine;
use sandstar_hal::HalControl;
use sandstar_server::control::ControlRunner;
use sandstar_server::{
    alerts, args, config, control, history, ipc, loader, logging, pid, rest, run_loop, sd_notify,
    signal, tls, watchdog,
};
#[cfg(feature = "svm")]
use sandstar_svm::{ChannelSnapshot, RustSvmRunner};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

// Compile-time assertions: types moved into spawn_blocking must be Send.
fn _assert_send<T: Send>() {}
#[allow(dead_code)]
fn _check_engine_send() {
    _assert_send::<Engine<Hal>>();
}
#[allow(dead_code)]
fn _check_control_runner_send() {
    _assert_send::<control::ControlRunner>();
}

/// Results returned from the blocking poll thread.
use args::ServerArgs;
use config::ServerConfig;
use rest::{EngineCmd, EngineHandle};

// Feature-gated HAL selection: exactly one of mock-hal, linux-hal, or simulator-hal must be enabled.
#[cfg(all(feature = "mock-hal", feature = "linux-hal"))]
compile_error!("Cannot enable both `mock-hal` and `linux-hal` features — pick one");

#[cfg(all(feature = "mock-hal", feature = "simulator-hal"))]
compile_error!("Cannot enable both `mock-hal` and `simulator-hal` features — pick one");

#[cfg(all(feature = "linux-hal", feature = "simulator-hal"))]
compile_error!("Cannot enable both `linux-hal` and `simulator-hal` features — pick one");

#[cfg(not(any(feature = "mock-hal", feature = "linux-hal", feature = "simulator-hal")))]
compile_error!("Must enable one of: `mock-hal`, `linux-hal`, `simulator-hal`");

#[cfg(all(
    feature = "mock-hal",
    not(feature = "linux-hal"),
    not(feature = "simulator-hal")
))]
type Hal = sandstar_hal::mock::MockHal;

#[cfg(feature = "linux-hal")]
type Hal = sandstar_hal_linux::LinuxHal;

#[cfg(all(
    feature = "simulator-hal",
    not(feature = "mock-hal"),
    not(feature = "linux-hal")
))]
type Hal = sandstar_hal::simulator::SimulatorHal;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 0a. Install panic hook — logs location before abort (release uses panic="abort")
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()));
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("<no message>");
        eprintln!(
            "PANIC at {}: {}",
            location.as_deref().unwrap_or("unknown"),
            msg
        );
    }));

    // 0. Ignore SIGPIPE to prevent crashes on broken IPC/TCP connections.
    //    Matches the C engine's signal(SIGPIPE, SIG_IGN) behavior.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    // 0a. Listen for SIGTERM before anything needs cleaning up (PID file,
    //     /dev/watchdog, IPC socket). systemd stops the service with SIGTERM;
    //     without a listener the process died at once and skipped the
    //     shutdown (R-13). A SIGTERM during startup stays pending and the first
    //     loop turn shuts down. Extra signals during shutdown are ignored;
    //     systemd escalates to SIGKILL after TimeoutStopSec.
    let term = signal::TermSignal::new()?;

    // 0b. Enable core dumps for post-mortem debugging (Unix only).
    #[cfg(unix)]
    {
        // Set RLIMIT_CORE to unlimited so the kernel writes a core file on crash.
        unsafe {
            let rlim = libc::rlimit {
                rlim_cur: libc::RLIM_INFINITY,
                rlim_max: libc::RLIM_INFINITY,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &rlim) == 0 {
                eprintln!("core dumps enabled (RLIMIT_CORE=unlimited)");
            } else {
                eprintln!("warning: failed to set RLIMIT_CORE");
            }
        }
    }

    // 1. Parse CLI args (merges CLI > env > defaults)
    let args = ServerArgs::parse();

    // 2. Initialize logging (stderr + optional file)
    let _log_guard = logging::init(
        &args.log_level,
        args.log_file.as_deref(),
        args.log_max_files,
    );

    // Log core dump status now that tracing is available.
    #[cfg(unix)]
    {
        if let Ok(pattern) = std::fs::read_to_string("/proc/sys/kernel/core_pattern") {
            info!(pattern = %pattern.trim(), "core dump pattern");
        }
    }

    // 3. Build config from args
    let config = ServerConfig::from_args(&args);
    if config.read_only {
        info!("*** READ-ONLY VALIDATION MODE — writes and watchdog disabled ***");
    }
    info!(
        poll_ms = config.poll_interval_ms,
        socket = %config.socket_path,
        read_only = config.read_only,
        "sandstar engine server starting"
    );

    // 4. Create PID file (prevents duplicate instances)
    let _pid_guard = if !args.no_pid_file {
        if let Some(ref pid_path) = args.pid_file {
            match pid::PidFile::create(pid_path) {
                Ok(guard) => Some(guard),
                Err(e) => {
                    error!(err = %e, path = %pid_path.display(), "failed to create PID file");
                    return Err(e.into());
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    // 5. Create HAL and engine
    #[cfg(feature = "simulator-hal")]
    let sim_state = sandstar_hal::simulator::new_shared_state();

    #[cfg(not(feature = "simulator-hal"))]
    let mut hal = Hal::new();
    #[cfg(feature = "simulator-hal")]
    let mut hal = sandstar_hal::simulator::SimulatorHal::new(sim_state.clone());
    if let Err(e) = hal.init() {
        warn!(err = %e, "HAL init encountered errors (continuing with degraded hardware)");
    }

    // Validate hardware subsystems
    let validation = hal.validate();
    for probe in &validation.subsystems {
        if probe.available {
            info!(subsystem = %probe.name, "{}", probe.message);
        } else {
            warn!(subsystem = %probe.name, "{}", probe.message);
        }
    }

    let mut engine = Engine::new(hal);

    // Load configuration: real files if config_dir is set, else demo mode
    if let Some(ref config_dir) = config.config_dir {
        info!(dir = %config_dir.display(), "loading real configuration");

        let points_path = config_dir.join("points.csv");
        let tables_csv = config_dir.join("tables.csv");

        // Table data files: try multiple locations
        let table_dir = [
            config_dir.join("../usr/local/config"),
            config_dir.join("config"),
            config_dir.clone(),
        ]
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| config_dir.clone());

        if points_path.exists() {
            match loader::load_points(&mut engine, &points_path) {
                Ok(n) => info!(count = n, "channels loaded"),
                Err(e) => error!(err = %e, "failed to load points.csv"),
            }
        } else {
            warn!(path = %points_path.display(), "points.csv not found");
        }

        if tables_csv.exists() {
            match loader::load_tables(&mut engine.tables, &tables_csv, &table_dir) {
                Ok(report) => {
                    info!(count = report.loaded, "tables loaded");
                    // The service still starts: the watchdog and the other
                    // channels do not wait for one table (R-63). A channel
                    // that needs a missing table has no conversion table.
                    for skipped in &report.skipped {
                        error!(
                            table = %skipped.name,
                            line = skipped.line,
                            reason = %skipped.reason,
                            "table not loaded at start-up"
                        );
                    }
                }
                Err(e) => error!(err = %e, "failed to load tables.csv"),
            }
        } else {
            warn!(path = %tables_csv.display(), "tables.csv not found");
        }

        // Load database.zinc: channel config, conversion params, flow tuning, filters
        let database_path = config_dir.join("database.zinc");
        if database_path.exists() {
            match loader::load_database(&mut engine, &database_path) {
                Ok(n) => info!(count = n, "database.zinc channels configured"),
                Err(e) => {
                    error!(err = %e, "failed to load database.zinc");
                    // Fallback: poll all input channels
                    loader::setup_polls(&mut engine);
                }
            }
        } else {
            warn!(path = %database_path.display(), "database.zinc not found");
            // Without database.zinc, poll all input channels
            loader::setup_polls(&mut engine);
        }
    } else {
        #[cfg(any(feature = "mock-hal", feature = "simulator-hal"))]
        {
            info!("no config dir — using demo mode");
            config::load_demo_channels(&mut engine);
        }
        #[cfg(not(any(feature = "mock-hal", feature = "simulator-hal")))]
        {
            error!("no config directory specified — hardware mode requires --config-dir");
            return Err("config directory required for hardware mode".into());
        }
    }

    // Cross-validate channels against available hardware
    let hw_warnings = loader::validate_channels_vs_hardware(&engine, &validation);
    for w in &hw_warnings {
        warn!("{}", w);
    }

    let start_time = Instant::now();
    let channel_count = engine.channels.count();
    let poll_count = engine.polls.count();
    info!(
        channels = channel_count,
        polls = poll_count,
        "engine initialized"
    );

    // 5a. Load control configuration (PID loops, sequencers)
    let control_runner = if args.no_control || config.read_only {
        if config.read_only {
            info!("control engine disabled (read-only mode)");
        }
        ControlRunner::new()
    } else {
        let control_path = args
            .control_config
            .clone()
            .or_else(|| config.config_dir.as_ref().map(|d| d.join("control.toml")));
        match control_path {
            Some(path) if path.exists() => match ControlRunner::load(&path) {
                Ok(runner) => {
                    info!(loops = runner.loop_count(), "control engine loaded");
                    runner
                }
                Err(e) => {
                    error!(error = %e, "failed to load control config, running without control");
                    ControlRunner::new()
                }
            },
            _ => {
                info!("no control.toml found, control engine disabled");
                ControlRunner::new()
            }
        }
    };

    // 5b. Set up history store (in-memory ring buffer, 1000 points per channel)
    let history_store = history::HistoryStore::new(1000);

    // 5b2. Set up alert manager for channel fault/down notifications
    let alerts_config_path = std::env::var("SANDSTAR_ALERTS_CONFIG").unwrap_or_else(|_| {
        config
            .config_dir
            .as_ref()
            .map(|d| d.join("alerts.json").to_string_lossy().into_owned())
            .unwrap_or_else(|| "alerts.json".to_string())
    });
    let alert_manager: alerts::SharedAlertManager = Arc::new(Mutex::new({
        let mut mgr = alerts::AlertManager::load(std::path::Path::new(&alerts_config_path));
        mgr.set_device_info(format!(
            "Sandstar Engine v{} ({}:{})",
            env!("CARGO_PKG_VERSION"),
            args.http_bind,
            args.http_port,
        ));
        mgr
    }));

    // 5c. Set up Sedona VM (optional, requires `svm` feature)
    #[cfg(feature = "svm")]
    let svm_snapshot = Arc::new(RwLock::new(ChannelSnapshot::new()));
    #[cfg(feature = "svm")]
    let svm_write_queue = Arc::new(Mutex::new(Vec::new()));
    #[cfg(feature = "svm")]
    let svm_tag_write_queue: Arc<Mutex<Vec<sandstar_svm::SvmTagWrite>>> =
        Arc::new(Mutex::new(Vec::new()));
    // Pure Rust SVM runner (synchronous, no background thread)
    #[cfg(feature = "svm")]
    let mut svm_runner: Option<RustSvmRunner> = None;
    #[cfg(feature = "svm")]
    if args.sedona {
        let scode_path = args.scode_path.clone().unwrap_or_else(|| {
            args.config_dir
                .as_ref()
                .map(|d| d.join("kits.scode"))
                .unwrap_or_else(|| "kits.scode".into())
        });
        sandstar_svm::set_engine_bridge(svm_snapshot.clone());
        sandstar_svm::set_write_queue(svm_write_queue.clone());
        sandstar_svm::set_tag_write_queue(svm_tag_write_queue.clone());

        let mut runner = RustSvmRunner::new(&scode_path);
        match runner.start() {
            Ok(()) => {
                info!(path = %scode_path.display(), "Pure Rust Sedona VM started");
                svm_runner = Some(runner);
            }
            Err(e) => {
                error!(err = %e, "failed to start pure Rust Sedona VM (continuing without SVM)");
            }
        }
    }
    #[cfg(not(feature = "svm"))]
    if args.sedona {
        error!("--sedona flag requires the `svm` feature (rebuild with --features svm)");
        return Err("SVM feature not enabled — rebuild with --features svm".into());
    }

    // 6. Set up poll interval
    let mut poll_timer = tokio::time::interval(Duration::from_millis(config.poll_interval_ms));
    poll_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 6b. Set up watch expiry timer (cleans stale watches every 60 seconds)
    let mut watch_expiry_timer = tokio::time::interval(Duration::from_secs(60));
    watch_expiry_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 6c. Set up watchdog timer (kicks /dev/watchdog + GPIO60 every 500ms)
    let watchdog = if config.read_only {
        watchdog::Watchdog::disabled()
    } else {
        watchdog::Watchdog::new()
    };
    let mut watchdog_timer = tokio::time::interval(Duration::from_millis(500));
    watchdog_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 7. Set up IPC listener
    let listener = ipc::create_listener(&config.socket_path).await?;
    info!(socket = %config.socket_path, "IPC listener ready");

    // 8. Validate TLS arguments (before starting REST API)
    let tls_enabled = tls::validate_tls_args(&args.tls_cert, &args.tls_key)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    #[cfg(not(feature = "tls"))]
    if tls_enabled {
        error!("TLS flags provided but the `tls` feature is not compiled in");
        error!("rebuild with: cargo build --features tls");
        return Err("TLS feature not enabled — rebuild with --features tls".into());
    }

    // 8. Create shared DynSlotStore (dynamic tags for components).
    let dyn_store: sandstar_server::sox::DynSlotStoreHandle = {
        use sandstar_server::sox::dyn_slots::DynSlotStore;
        let mut store = DynSlotStore::with_defaults();
        let config_dir = std::env::var("SANDSTAR_CONFIG_DIR")
            .unwrap_or_else(|_| "/home/eacio/sandstar/etc/config".to_string());
        let path = format!("{config_dir}/dyn_slots.json");
        match store.load(&path) {
            Ok(0) => {}
            Ok(n) => info!(total_tags = n, "dyn_slots: loaded dynamic tags"),
            Err(e) => warn!("dyn_slots: failed to load from {path}: {e}"),
        }
        std::sync::Arc::new(std::sync::RwLock::new(store))
    };

    // 8a. Set up REST API (Axum HTTP server, with optional TLS)
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCmd>(64);

    // 8b. Start SOX/DASP server BEFORE REST so we can share the tree with REST endpoints.
    let _sox_handles = if args.sox {
        let sox_engine_handle = rest::EngineHandle::new(cmd_tx.clone());
        info!(port = args.sox_port, "SOX/DASP server starting");
        Some(sandstar_server::sox::spawn_sox_server(
            args.sox_port,
            args.sox_user.clone(),
            args.sox_pass.clone(),
            sox_engine_handle,
            args.manifests_dir.clone(),
            Some(dyn_store.clone()),
        ))
    } else {
        None
    };

    // Build SOX API state for the DDC visual editor REST endpoints.
    // When the SOX UDP server is running, reuse its shared tree.
    // Otherwise, create a standalone tree + manifest DB so the editor
    // works even without the `--sox` flag.
    let sox_api_state = if let Some(ref h) = _sox_handles {
        Some(rest::sox_api::SoxApiState {
            tree: h.tree.clone(),
            manifest_db: h.manifest_db.clone(),
            dyn_store: Some(dyn_store.clone()),
        })
    } else {
        use sandstar_server::sox::sox_handlers::{ComponentTree, ManifestDb};
        let dir = args.manifests_dir.clone().unwrap_or_default();
        let manifest_db = std::sync::Arc::new(ManifestDb::load(&dir));
        let tree = std::sync::Arc::new(std::sync::RwLock::new(ComponentTree::new_with_manifest(
            manifest_db.clone(),
        )));
        Some(rest::sox_api::SoxApiState {
            tree,
            manifest_db,
            dyn_store: Some(dyn_store.clone()),
        })
    };

    // 8b2. Set up roxWarp cluster (optional, requires --cluster flag)
    let roxwarp_state: Option<sandstar_server::roxwarp::RoxWarpState> = if args.cluster {
        use sandstar_server::roxwarp::cluster::DeltaEngine;
        use sandstar_server::roxwarp::{ClusterConfig, ClusterManager, RoxWarpState};

        let mut cluster_config = if let Some(ref config_path) = args.cluster_config {
            ClusterConfig::load(config_path)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
        } else {
            ClusterConfig::default()
        };

        // Override node_id if provided via CLI
        if let Some(ref nid) = args.node_id {
            cluster_config.node_id = nid.clone();
        }

        // Override port from CLI
        cluster_config.port = args.cluster_port;
        // Plain WS without peer auth only on explicit request (R-54, S-36).
        cluster_config.allow_plain = args.cluster_insecure;

        // Set mTLS paths from CLI args
        if args.cluster_cert.is_some() || args.cluster_key.is_some() || args.cluster_ca.is_some() {
            sandstar_server::roxwarp::mtls::validate_mtls_args(
                &args.cluster_cert,
                &args.cluster_key,
                &args.cluster_ca,
            )
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            cluster_config.cert_path = args.cluster_cert.clone();
            cluster_config.key_path = args.cluster_key.clone();
            cluster_config.ca_path = args.cluster_ca.clone();
        }

        let delta_engine = Arc::new(DeltaEngine::new(cluster_config.node_id.clone()));
        let cluster_handle = EngineHandle::new(cmd_tx.clone());
        let manager =
            ClusterManager::new(cluster_config.clone(), delta_engine.clone(), cluster_handle);

        info!(
            node_id = %cluster_config.node_id,
            port = cluster_config.port,
            peers = cluster_config.peers.len(),
            // Paths only; whether mTLS builds is logged by `roxwarp::transport`.
            mtls_paths = cluster_config.cert_path.is_some(),
            "roxWarp cluster mode enabled"
        );

        // Spawn the cluster manager (runs feed loop + outbound peer connections)
        tokio::spawn(async move {
            manager.run().await;
        });

        let rw_state = RoxWarpState {
            delta_engine,
            config: cluster_config.clone(),
            sox_tree: sox_api_state.as_ref().map(|s| s.tree.clone()),
        };

        // Separate roxWarp listener on the cluster port (default 7443).
        // Fail-closed (R-54, S-36): mTLS, or plain WS only with
        // --cluster-insecure; a broken mTLS setup keeps the port closed.
        // `server_transport` logs why the listener is off.
        let cluster_port = cluster_config.port;
        let transport = sandstar_server::roxwarp::transport::server_transport(&cluster_config);
        let rw_state_for_listener = rw_state.clone();
        tokio::spawn(async move {
            use axum::routing::get;
            use axum::Router;
            use sandstar_server::roxwarp::transport::Transport;

            // Outside the REST auth layer (R-2): peers authenticate with mTLS
            // on this port; plain WS (no peer auth) only with --cluster-insecure.
            let cluster_app = Router::new()
                .route(
                    "/roxwarp",
                    get(sandstar_server::roxwarp::handler::roxwarp_upgrade),
                )
                .with_state(rw_state_for_listener);

            let bind_addr = std::net::SocketAddr::from(([0, 0, 0, 0], cluster_port));

            match transport {
                Transport::Mtls(server_tls_config) => {
                    info!(port = cluster_port, "roxWarp: starting mTLS listener");
                    let rustls_config =
                        axum_server::tls_rustls::RustlsConfig::from_config(server_tls_config);
                    if let Err(e) = axum_server::bind_rustls(bind_addr, rustls_config)
                        .serve(cluster_app.into_make_service())
                        .await
                    {
                        error!(port = cluster_port, error = %e, "roxWarp: mTLS listener failed");
                    }
                }
                Transport::Plain => {
                    let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
                        Ok(l) => l,
                        Err(e) => {
                            error!(port = cluster_port, error = %e, "roxWarp: failed to bind cluster port");
                            return;
                        }
                    };
                    info!(
                        port = cluster_port,
                        "roxWarp: listening (plain WS, --cluster-insecure)"
                    );
                    if let Err(e) = axum::serve(listener, cluster_app).await {
                        error!(port = cluster_port, error = %e, "roxWarp: plain WS listener failed");
                    }
                }
                Transport::Off => {
                    info!(port = cluster_port, "roxWarp: cluster listener not started");
                }
            }
        });

        Some(rw_state)
    } else {
        None
    };

    if !args.no_rest {
        let handle = EngineHandle::new(cmd_tx.clone());
        let auth_state = sandstar_server::auth::AuthState::new(config.auth_store.clone());
        // Core routes without the global layers; every router below is
        // merged first and `with_global_layers` wraps them all (R-53, S-35).
        let mut app = rest::api_routes(
            handle,
            auth_state.clone(),
            config.auth_token.clone(),
            sox_api_state,
        );

        // Merge dynamic tags REST endpoints
        app = app.merge(rest::tags::router(dyn_store.clone(), auth_state.clone()));

        // Merge alert management REST endpoints
        app = app.merge(alerts::alert_router(
            alert_manager.clone(),
            auth_state.clone(),
        ));

        // Merge roxWarp cluster endpoints (WebSocket + status API)
        if let Some(ref rw_state) = roxwarp_state {
            app = app.merge(rest::roxwarp_router(rw_state.clone(), auth_state.clone()));
        } else {
            // Always provide /api/cluster/status even when clustering is off
            app = app.merge(rest::cluster_disabled_router());
        }

        // Merge simulator REST endpoints when built with simulator-hal
        #[cfg(feature = "simulator-hal")]
        {
            app = app.merge(rest::sim::router(sim_state.clone(), auth_state.clone()));
        }

        // Request counter, rate limit, body limit and CORS. Last step: a
        // router merged after this line is outside those layers.
        let app = rest::with_global_layers(app, config.rate_limit);

        let addr: std::net::SocketAddr =
            format!("{}:{}", args.http_bind, args.http_port)
                .parse()
                .map_err(|e| format!("invalid bind address '{}': {}", args.http_bind, e))?;

        #[cfg(feature = "tls")]
        if tls_enabled {
            let cert_path = args.tls_cert.as_ref().expect("validated by tls_enabled");
            let key_path = args.tls_key.as_ref().expect("validated by tls_enabled");
            tls::validate_tls_files(cert_path, key_path)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            let tls_config = tls::load_rustls_config(cert_path, key_path)
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            info!(
                addr = %addr,
                cert = %cert_path.display(),
                "REST API listening on https://{} (TLS enabled)",
                addr,
            );
            tokio::spawn(async move {
                if let Err(e) = axum_server::bind_rustls(addr, tls_config)
                    .serve(app.into_make_service())
                    .await
                {
                    tracing::error!(err = %e, "HTTPS server error");
                }
            });
        } else {
            start_http_server(addr, app, config.rate_limit).await?;
        }

        #[cfg(not(feature = "tls"))]
        {
            start_http_server(addr, app, config.rate_limit).await?;
        }
    }

    // 8c. Notify systemd that the service is ready (Type=notify)
    sd_notify::ready();

    // 9. The event loop owns the engine from here on (run_loop.rs).
    // IPC connections are served on their own tasks; only the command reaches
    // the loop (R-28).
    // Signals first, as before: a failed SIGHUP registration ends start-up
    // before the IPC accept task exists.
    let mut signals = ProcessSignals {
        ctrl_c: Box::pin(tokio::signal::ctrl_c()),
        term,
        hup: signal::HupSignal::new()?,
    };
    let (ipc_tx, ipc_rx) = mpsc::channel::<ipc::IpcRequest>(ipc::MAX_CONNECTIONS);
    let ipc_slots = Arc::new(tokio::sync::Semaphore::new(ipc::MAX_CONNECTIONS));
    let _ipc_accept = ipc::spawn_accept_loop(listener, ipc_tx, ipc_slots);
    let parts = run_loop::LoopParts {
        engine,
        control_runner,
        cmd_rx,
        ipc_rx,
        poll_timer,
        watch_expiry_timer,
        watchdog_timer,
        watchdog,
        history_store,
        alert_manager,
        start_time,
        no_control: args.no_control,
        control_config: args.control_config.clone(),
        #[cfg(feature = "svm")]
        svm_runner,
        #[cfg(feature = "svm")]
        svm_snapshot,
    };

    // 12. Main event loop
    info!("engine running — press Ctrl+C to stop");
    let end = run_loop::run(parts, &config, &mut signals).await;

    // 13. Graceful shutdown
    let poll_stuck = run_loop::shutdown(end, &config).await;
    if poll_stuck {
        // Dropping the runtime would wait for the stuck poll without limit
        // (tokio: Runtime drop blocks on spawn_blocking tasks), until systemd
        // sends SIGKILL. Clean up and leave now.
        warn!("exiting without waiting for the stuck poll");
        drop(_pid_guard);
        drop(_log_guard);
        std::process::exit(0);
    }
    // _pid_guard dropped here -> PID file removed
    // _log_guard dropped here -> log file flushed

    Ok(())
}

/// Start the HTTP (non-TLS) server, logging bind address and rate limit info.
async fn start_http_server(
    addr: std::net::SocketAddr,
    app: axum::Router,
    rate_limit: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let tcp_listener = tokio::net::TcpListener::bind(addr).await?;
    let local_addr = tcp_listener.local_addr()?;
    if rate_limit > 0 {
        info!(
            port = local_addr.port(),
            rate_limit,
            "REST API listening on http://{} (rate limit: {} req/s)",
            local_addr,
            rate_limit,
        );
    } else {
        info!(
            port = local_addr.port(),
            "REST API listening on http://{} (no rate limit)", local_addr,
        );
    }
    tokio::spawn(async move {
        if let Err(e) = axum::serve(tcp_listener, app).await {
            tracing::error!(err = %e, "REST server error");
        }
    });
    Ok(())
}

/// The process signals the event loop acts on.
struct ProcessSignals {
    /// One future for the life of the process: it is polled across loop
    /// turns until Ctrl+C arrives, and not after that (the loop stops).
    ctrl_c: Pin<Box<dyn Future<Output = std::io::Result<()>>>>,
    term: signal::TermSignal,
    hup: signal::HupSignal,
}

impl run_loop::Signals for ProcessSignals {
    async fn next(&mut self) -> run_loop::Signal {
        tokio::select! {
            _ = &mut self.ctrl_c => run_loop::Signal::Sigint,
            _ = self.term.recv() => run_loop::Signal::Sigterm,
            _ = self.hup.recv() => run_loop::Signal::Hup,
        }
    }
}
