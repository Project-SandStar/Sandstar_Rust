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

use std::collections::HashMap;
use std::pin::pin;
#[cfg(feature = "svm")]
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use sandstar_engine::{Engine, Notification};
use sandstar_hal::{HalControl, HalDiagnostics, HalRead, HalWrite};
use sandstar_server::{args, cmd_handler, config, control, dispatch, history, ipc, loader, logging, metrics, pid, reload, rest, sd_notify, signal, tls, watchdog};
use sandstar_server::control::ControlRunner;
use sandstar_server::history::HistoryPoint;
#[cfg(feature = "svm")]
use sandstar_svm::{ChannelInfo, ChannelSnapshot, SvmRunner};
use tokio::sync::mpsc;
#[cfg(feature = "svm")]
use tracing::debug;
use tracing::{error, info, warn};

// Compile-time assertions: types moved into spawn_blocking must be Send.
fn _assert_send<T: Send>() {}
#[allow(dead_code)]
fn _check_engine_send() { _assert_send::<Engine<Hal>>(); }
#[allow(dead_code)]
fn _check_control_runner_send() { _assert_send::<control::ControlRunner>(); }

/// Results returned from the blocking poll thread.
type PollResult = (Engine<Hal>, ControlRunner, Vec<Notification>, Duration);

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

#[cfg(all(feature = "mock-hal", not(feature = "linux-hal"), not(feature = "simulator-hal")))]
type Hal = sandstar_hal::mock::MockHal;

#[cfg(feature = "linux-hal")]
type Hal = sandstar_hal_linux::LinuxHal;

#[cfg(all(feature = "simulator-hal", not(feature = "mock-hal"), not(feature = "linux-hal")))]
type Hal = sandstar_hal::simulator::SimulatorHal;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 0a. Install panic hook — logs location before abort (release uses panic="abort")
    std::panic::set_hook(Box::new(|info| {
        let location = info.location().map(|l| format!("{}:{}", l.file(), l.line()));
        let msg = info.payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("<no message>");
        eprintln!("PANIC at {}: {}", location.as_deref().unwrap_or("unknown"), msg);
    }));

    // 0. Ignore SIGPIPE to prevent crashes on broken IPC/TCP connections.
    //    Matches the C engine's signal(SIGPIPE, SIG_IGN) behavior.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

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
    let _log_guard = logging::init(&args.log_level, args.log_file.as_deref());

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
                Ok(n) => info!(count = n, "tables loaded"),
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
    info!(channels = channel_count, polls = poll_count, "engine initialized");

    // 5a. Load control configuration (PID loops, sequencers)
    let control_runner = if args.no_control || config.read_only {
        if config.read_only {
            info!("control engine disabled (read-only mode)");
        }
        ControlRunner::new()
    } else {
        let control_path = args.control_config.clone()
            .or_else(|| config.config_dir.as_ref().map(|d| d.join("control.toml")));
        match control_path {
            Some(path) if path.exists() => {
                match ControlRunner::load(&path) {
                    Ok(runner) => {
                        info!(loops = runner.loop_count(), "control engine loaded");
                        runner
                    }
                    Err(e) => {
                        error!(error = %e, "failed to load control config, running without control");
                        ControlRunner::new()
                    }
                }
            }
            _ => {
                info!("no control.toml found, control engine disabled");
                ControlRunner::new()
            }
        }
    };

    // 5b. Set up history store (in-memory ring buffer, 1000 points per channel)
    let mut history_store = history::HistoryStore::new(1000);

    // 5c. Set up Sedona VM (optional, requires `svm` feature)
    #[cfg(feature = "svm")]
    let svm_snapshot = Arc::new(RwLock::new(ChannelSnapshot::new()));
    #[cfg(feature = "svm")]
    let svm_write_queue = Arc::new(Mutex::new(Vec::new()));
    #[cfg(feature = "svm")]
    let svm_tag_write_queue: Arc<Mutex<Vec<sandstar_svm::SvmTagWrite>>> =
        Arc::new(Mutex::new(Vec::new()));
    #[cfg(feature = "svm")]
    let mut svm_runner: Option<SvmRunner> = None;
    #[cfg(feature = "svm")]
    if args.sedona {
        let scode_path = args.scode_path.clone().unwrap_or_else(|| {
            // Default: look for kits.scode next to config dir
            args.config_dir
                .as_ref()
                .map(|d| d.join("kits.scode"))
                .unwrap_or_else(|| "kits.scode".into())
        });
        sandstar_svm::set_engine_bridge(svm_snapshot.clone());
        sandstar_svm::set_write_queue(svm_write_queue.clone());
        sandstar_svm::set_tag_write_queue(svm_tag_write_queue.clone());

        let mut runner = SvmRunner::new(&scode_path);
        match runner.start() {
            Ok(()) => {
                info!(path = %scode_path.display(), "Sedona VM started");
                svm_runner = Some(runner);
            }
            Err(e) => {
                error!(err = %e, "failed to start Sedona VM (continuing without SVM)");
            }
        }
    }
    #[cfg(not(feature = "svm"))]
    if args.sedona {
        error!("--sedona flag requires the `svm` feature (rebuild with --features svm)");
        return Err("SVM feature not enabled — rebuild with --features svm".into());
    }

    // 6. Set up poll interval
    let mut poll_timer =
        tokio::time::interval(Duration::from_millis(config.poll_interval_ms));
    poll_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 6b. Set up watch expiry timer (cleans stale watches every 60 seconds)
    let mut watch_expiry_timer = tokio::time::interval(Duration::from_secs(60));
    watch_expiry_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 6c. Set up watchdog timer (kicks /dev/watchdog + GPIO60 every 500ms)
    let mut watchdog = if config.read_only {
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

    // 8a. Set up REST API (Axum HTTP server, with optional TLS)
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCmd>(64);
    if !args.no_rest {
        let handle = EngineHandle::new(cmd_tx.clone());
        let auth_state = sandstar_server::auth::AuthState::new(config.auth_store.clone());
        #[allow(unused_mut)]
        let mut app = rest::router_with_auth(
            handle,
            auth_state,
            config.auth_token.clone(),
            config.rate_limit,
        );

        // Merge simulator REST endpoints when built with simulator-hal
        #[cfg(feature = "simulator-hal")]
        {
            app = app.merge(rest::sim::router(sim_state.clone()));
        }

        let addr: std::net::SocketAddr = format!("{}:{}", args.http_bind, args.http_port)
            .parse()
            .map_err(|e| format!("invalid bind address '{}': {}", args.http_bind, e))?;

        #[cfg(feature = "tls")]
        if tls_enabled {
            let cert_path = args.tls_cert.as_ref().expect("validated by tls_enabled");
            let key_path = args.tls_key.as_ref().expect("validated by tls_enabled");
            tls::validate_tls_files(cert_path, key_path)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            let tls_config = tls::load_rustls_config(cert_path, key_path).await
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

    // 8b. Start SOX/DASP server (optional, for Sedona Application Editor)
    let _sox_handle = if args.sox {
        let sox_engine_handle = rest::EngineHandle::new(cmd_tx.clone());
        info!(port = args.sox_port, "SOX/DASP server starting");
        Some(sandstar_server::sox::spawn_sox_server(
            args.sox_port,
            args.sox_user.clone(),
            args.sox_pass.clone(),
            sox_engine_handle,
            args.manifests_dir.clone(),
        ))
    } else {
        None
    };

    // 8c. Notify systemd that the service is ready (Type=notify)
    sd_notify::ready();

    // 9. Watch subscription state (lives in main loop scope)
    let mut watches: HashMap<String, cmd_handler::WatchState> = HashMap::new();
    let mut watch_counter: u64 = 0;

    // 10. Non-blocking poll: engine + control_runner wrapped in Option for spawn_blocking swap
    let mut engine: Option<Engine<Hal>> = Some(engine);
    let mut control_runner: Option<ControlRunner> = Some(control_runner);
    let (poll_result_tx, mut poll_result_rx) = mpsc::channel::<PollResult>(1);
    let mut poll_in_progress = false;

    // 11. Set up signals
    let mut shutdown = pin!(tokio::signal::ctrl_c());
    let mut hup = signal::HupSignal::new()?;
    let mut ipc_shutdown = false;

    // 12. Main event loop
    info!("engine running — press Ctrl+C to stop");

    loop {
        tokio::select! {
            // --- Poll timer tick (only when engine is available) ---
            _ = poll_timer.tick(), if !poll_in_progress && engine.is_some() => {
                let mut eng = engine.take().expect("select! guard ensures engine.is_some()");
                let mut ctrl = control_runner.take().unwrap_or_default();
                poll_in_progress = true;
                let tx = poll_result_tx.clone();

                tokio::task::spawn_blocking(move || {
                    let poll_start = Instant::now();
                    let notifications = eng.poll_update();
                    // Run control loops after poll (PID + sequencers).
                    ctrl.execute(&mut eng, Instant::now());
                    let elapsed = poll_start.elapsed();
                    let _ = tx.blocking_send((eng, ctrl, notifications, elapsed));
                });
            }

            // --- Poll results returned from blocking thread ---
            Some((eng, ctrl, notifications, elapsed)) = poll_result_rx.recv() => {
                engine = Some(eng);
                control_runner = Some(ctrl);
                poll_in_progress = false;

                // Record poll metrics
                let elapsed_us = elapsed.as_micros() as u64;
                metrics::metrics().poll_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                metrics::metrics().poll_duration_us_last.store(elapsed_us, std::sync::atomic::Ordering::Relaxed);
                metrics::metrics().poll_duration_us_max.fetch_max(elapsed_us, std::sync::atomic::Ordering::Relaxed);

                // Poll cycle overrun detection: warn if cycle took >80% of interval
                let overrun_threshold_ms = (config.poll_interval_ms * 80) / 100;
                let elapsed_ms = elapsed.as_millis() as u64;
                if elapsed_ms > overrun_threshold_ms {
                    metrics::metrics().poll_overrun_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    warn!(
                        duration_ms = elapsed_ms,
                        limit_ms = config.poll_interval_ms,
                        "poll cycle overrun: took {}ms (limit {}ms)",
                        elapsed_ms,
                        config.poll_interval_ms,
                    );
                }

                // Capture all polled values into history ring buffers
                if let Some(ref eng) = engine {
                    let now_unix = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let mut hal_err_count = 0u64;
                    for (&ch_id, item) in eng.polls.iter() {
                        if item.last_value.status == sandstar_engine::EngineStatus::Down {
                            hal_err_count += 1;
                        }
                        history_store.record(ch_id, HistoryPoint {
                            ts: now_unix,
                            cur: item.last_value.cur,
                            raw: item.last_value.raw,
                            status: item.last_value.status,
                        });
                        metrics::metrics().history_points.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if hal_err_count > 0 {
                        metrics::metrics().hal_errors.fetch_add(hal_err_count, std::sync::atomic::Ordering::Relaxed);
                    }
                }

                // Update SVM channel snapshot after each poll
                #[cfg(feature = "svm")]
                if svm_runner.is_some() {
                    if let Some(ref eng) = engine {
                        let channels: Vec<ChannelInfo> = eng.channels.iter().map(|(_, ch)| {
                            // Populate write priority data from priority array
                            let (write_level, write_levels) = match &ch.priority_array {
                                Some(pa) => {
                                    let levels = pa.levels();
                                    let mut wl = [None; 17];
                                    for (i, entry) in levels.iter().enumerate() {
                                        wl[i] = entry.value;
                                    }
                                    let effective_level = levels.iter()
                                        .position(|e| e.value.is_some())
                                        .map(|i| (i + 1) as u8)
                                        .unwrap_or(17);
                                    (effective_level, wl)
                                }
                                None => (17u8, [None; 17]),
                            };
                            // Convert channel tags to TagValue enum
                            let tags: std::collections::HashMap<String, sandstar_svm::TagValue> =
                                ch.tags.iter().map(|(k, v)| {
                                    let tv = if v == "M" {
                                        sandstar_svm::TagValue::Marker
                                    } else if v == "true" || v == "false" {
                                        sandstar_svm::TagValue::Bool(v == "true")
                                    } else if let Ok(n) = v.parse::<f64>() {
                                        sandstar_svm::TagValue::Number(n)
                                    } else {
                                        sandstar_svm::TagValue::Str(v.clone())
                                    };
                                    (k.clone(), tv)
                                }).collect();
                            ChannelInfo {
                                channel: ch.id,
                                cur: ch.value.cur,
                                raw: ch.value.raw,
                                status_ok: ch.value.status == sandstar_engine::EngineStatus::Ok,
                                enabled: ch.enabled,
                                label: ch.label.clone(),
                                channel_in: ch.channel_in.map(|id| id as i32).unwrap_or(-1),
                                write_level,
                                write_levels,
                                tags,
                            }
                        }).collect();
                        if let Ok(mut snap) = svm_snapshot.write() {
                            snap.update(channels);
                        }
                    }

                    // Process SVM writes
                    if !config.read_only {
                        let writes = sandstar_svm::drain_writes();
                        if let Some(ref mut eng) = engine {
                            for w in writes {
                                if let Err(e) = eng.channel_write_level(w.channel, 8, Some(w.value), "sedona", 0.0) {
                                    debug!(channel = w.channel, err = %e, "SVM write failed");
                                }
                            }
                        }
                    }

                    // Process SVM tag writes (sedonaId, sedonaType)
                    let tag_writes = sandstar_svm::drain_tag_writes();
                    if !tag_writes.is_empty() {
                        if let Some(ref mut eng) = engine {
                            for tw in tag_writes {
                                if let Some(ch) = eng.channels.get_mut(tw.channel) {
                                    ch.tags.insert(tw.tag, tw.value);
                                }
                            }
                        }
                    }
                }

                if !notifications.is_empty() {
                    info!(
                        count = notifications.len(),
                        elapsed_us = elapsed.as_micros() as u64,
                        "poll cycle complete"
                    );
                }

                if elapsed > Duration::from_secs(1) {
                    warn!(
                        elapsed_ms = elapsed.as_millis() as u64,
                        "poll cycle exceeded 1s — consider reducing poll load"
                    );
                }
            }

            // --- REST API command (only when engine is available) ---
            Some(cmd) = cmd_rx.recv(), if engine.is_some() => {
                // PollNow is handled here (not in handle_rest_cmd) so it can
                // use spawn_blocking like the timer — avoids blocking the
                // main loop during I2C reads.
                if let EngineCmd::PollNow { reply } = cmd {
                    if poll_in_progress {
                        let _ = reply.send(Err("poll already in progress".into()));
                    } else {
                        let mut eng = engine.take().expect("select! guard ensures engine.is_some()");
                        let mut ctrl = control_runner.take().unwrap_or_default();
                        poll_in_progress = true;
                        let tx = poll_result_tx.clone();

                        tokio::task::spawn_blocking(move || {
                            let poll_start = Instant::now();
                            let notifications = eng.poll_update();
                            ctrl.execute(&mut eng, Instant::now());
                            let elapsed = poll_start.elapsed();
                            let _ = tx.blocking_send((eng, ctrl, notifications, elapsed));
                        });
                        let _ = reply.send(Ok("poll triggered".into()));
                    }
                } else {
                    let mut ctx = cmd_handler::CmdContext {
                        config: &config,
                        start_time,
                        watches: &mut watches,
                        watch_counter: &mut watch_counter,
                        history_store: &history_store,
                    };
                    handle_rest_cmd(
                        cmd,
                        engine.as_mut().expect("select! guard ensures engine.is_some()"),
                        &mut ctx,
                    );
                }
            }

            // --- Incoming IPC connection (only when engine is available) ---
            result = ipc::accept(&listener), if engine.is_some() => {
                match result {
                    Ok((stream, _read_buf, _write_buf)) => {
                        let conn_result = dispatch::handle_connection(
                            stream,
                            engine.as_mut().expect("select! guard ensures engine.is_some()"),
                            &config,
                            start_time,
                            &history_store,
                        );
                        match conn_result {
                            Ok(dispatch::ConnectionResult::Shutdown) => {
                                info!("shutdown requested via IPC");
                                ipc_shutdown = true;
                                break;
                            }
                            Ok(dispatch::ConnectionResult::PollNow) => {
                                if !poll_in_progress {
                                    let mut eng = engine.take().expect("select! guard ensures engine.is_some()");
                                    let mut ctrl = control_runner.take().unwrap_or_default();
                                    poll_in_progress = true;
                                    let tx = poll_result_tx.clone();
                                    tokio::task::spawn_blocking(move || {
                                        let poll_start = Instant::now();
                                        let notifications = eng.poll_update();
                                        ctrl.execute(&mut eng, Instant::now());
                                        let elapsed = poll_start.elapsed();
                                        let _ = tx.blocking_send((eng, ctrl, notifications, elapsed));
                                    });
                                }
                            }
                            Ok(dispatch::ConnectionResult::Continue) => {}
                            Err(e) => {
                                error!(err = %e, "IPC connection error");
                            }
                        }
                    }
                    Err(e) => {
                        error!(err = %e, "failed to accept IPC connection");
                    }
                }
            }

            // --- SIGHUP: config reload ---
            _ = hup.recv() => {
                if let Some(ref mut eng) = engine {
                    if let Some(ref dir) = config.config_dir {
                        info!("SIGHUP received — reloading configuration");
                        match reload::reload_config(eng, dir) {
                            Ok(summary) => info!(%summary, "config reload complete"),
                            Err(e) => error!(err = %e, "config reload failed"),
                        }

                        // Reload control.toml if present
                        if !args.no_control && !config.read_only {
                            let control_path = args.control_config.clone()
                                .unwrap_or_else(|| dir.join("control.toml"));
                            if control_path.exists() {
                                match ControlRunner::load(&control_path) {
                                    Ok(new_runner) => {
                                        info!(loops = new_runner.loop_count(), "control config reloaded");
                                        control_runner = Some(new_runner);
                                    }
                                    Err(e) => {
                                        error!(error = %e, "failed to reload control config (keeping previous)");
                                    }
                                }
                            }
                        }
                    } else {
                        warn!("SIGHUP ignored — no config dir (demo mode)");
                    }
                } else {
                    info!("SIGHUP received — deferring reload (poll in progress)");
                }
            }

            // --- Watch expiry timer (every 60s, clean stale subscriptions) ---
            _ = watch_expiry_timer.tick() => {
                cmd_handler::expire_stale_watches(&mut watches);
            }

            // --- Watchdog timer (always fires, even during blocking poll) ---
            _ = watchdog_timer.tick() => {
                watchdog.kick();
                sd_notify::watchdog();
            }

            // --- Shutdown signal (Ctrl+C / SIGTERM) — always works ---
            _ = &mut shutdown => {
                info!("shutdown signal received (Ctrl+C)");
                break;
            }
        }
    }

    // 13. Graceful shutdown
    // If poll is in progress, wait up to 5s for it to finish
    if poll_in_progress {
        match tokio::time::timeout(Duration::from_secs(5), poll_result_rx.recv()).await {
            Ok(Some((eng, _ctrl, _, _))) => {
                engine = Some(eng);
                // control_runner not needed after shutdown; drop it.
            }
            Ok(None) => {
                warn!("poll channel closed during shutdown");
            }
            Err(_) => {
                warn!("poll completion timed out (5s) — shutting down anyway");
            }
        }
    }

    // Stop Sedona VM if running
    #[cfg(feature = "svm")]
    if let Some(ref mut runner) = svm_runner {
        runner.stop();
    }

    let uptime = start_time.elapsed();
    let reason = if ipc_shutdown { "IPC command" } else { "signal" };
    info!(uptime_secs = uptime.as_secs(), reason, "engine stopped");
    if let Some(ref mut eng) = engine {
        if let Err(e) = eng.hal.shutdown() {
            warn!(err = %e, "HAL shutdown error");
        }
    }
    watchdog.close();
    ipc::cleanup(&config.socket_path);
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
            "REST API listening on http://{} (no rate limit)",
            local_addr,
        );
    }
    tokio::spawn(async move {
        if let Err(e) = axum::serve(tcp_listener, app).await {
            tracing::error!(err = %e, "REST server error");
        }
    });
    Ok(())
}

/// Process a single REST API command — delegates to shared handler.
fn handle_rest_cmd<H: HalRead + HalWrite + HalDiagnostics>(
    cmd: EngineCmd,
    engine: &mut Engine<H>,
    ctx: &mut cmd_handler::CmdContext<'_>,
) {
    cmd_handler::handle_engine_cmd(cmd, engine, ctx);
}
