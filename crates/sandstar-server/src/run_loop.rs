//! The engine's main event loop and its shutdown steps.
//!
//! Moved out of `main()` so the loop that runs on the device is the loop
//! the tests run (R-33, R-36). `main()` builds the parts, hands in the
//! process signals, and keeps what cannot be tested: `process::exit`.
//!
//! The engine has one owner, this loop. A poll takes the engine to a
//! blocking thread and brings it back; while it is away the branches that
//! need it are switched off (`if engine.is_some()`).

use std::collections::HashMap;
use std::path::PathBuf;
#[cfg(feature = "svm")]
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sandstar_engine::{Engine, Notification};
use sandstar_hal::{HalControl, HalDiagnostics, HalRead, HalWrite};
#[cfg(feature = "svm")]
use sandstar_svm::{ChannelInfo, ChannelSnapshot, RustSvmRunner};
use tokio::sync::mpsc;
#[cfg(feature = "svm")]
use tracing::debug;
use tracing::{error, info, warn};

use crate::config::ServerConfig;
use crate::control::ControlRunner;
use crate::history::{HistoryPoint, HistoryStore};
use crate::rest::EngineCmd;
use crate::watchdog::Watchdog;
use crate::{alerts, cmd_handler, dispatch, ipc, metrics, reload, sd_notify};

/// What a poll brings back from its blocking thread.
pub type PollResult<H> = (Engine<H>, ControlRunner, Vec<Notification>, Duration);

/// A process signal the loop acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Reload the configuration.
    Hup,
    /// Stop (Ctrl+C).
    Sigint,
    /// Stop (systemd).
    Sigterm,
}

/// Where the loop gets its signals from: the process in `main()`, a
/// channel in tests.
pub trait Signals {
    /// The next signal. Must be cancel safe: the loop drops this future
    /// whenever another branch is ready first.
    fn next(&mut self) -> impl std::future::Future<Output = Signal>;
}

/// Everything the loop owns while it runs.
pub struct LoopParts<H> {
    pub engine: Engine<H>,
    pub control_runner: ControlRunner,
    pub cmd_rx: mpsc::Receiver<EngineCmd>,
    pub ipc_rx: mpsc::Receiver<ipc::IpcRequest>,
    pub poll_timer: tokio::time::Interval,
    pub watch_expiry_timer: tokio::time::Interval,
    pub watchdog_timer: tokio::time::Interval,
    pub watchdog: Watchdog,
    pub history_store: HistoryStore,
    pub alert_manager: alerts::SharedAlertManager,
    pub start_time: Instant,
    /// `--no-control`: SIGHUP does not reload control.toml.
    pub no_control: bool,
    /// `--control-config`: control.toml path, else `<config dir>/control.toml`.
    pub control_config: Option<PathBuf>,
    #[cfg(feature = "svm")]
    pub svm_runner: Option<RustSvmRunner>,
    #[cfg(feature = "svm")]
    pub svm_snapshot: Arc<RwLock<ChannelSnapshot>>,
}

/// What the loop leaves behind for [`shutdown`].
pub struct LoopEnd<H> {
    /// `None` while a poll still has the engine.
    pub engine: Option<Engine<H>>,
    pub poll_in_progress: bool,
    pub poll_result_rx: mpsc::Receiver<PollResult<H>>,
    /// Held, not used, until shutdown is through, as they were while the
    /// loop lived in `main()`: with a sender alive, a poll task that dies
    /// without sending leaves the shutdown waiting its 5 s (and reporting
    /// the poll as stuck) instead of seeing a closed channel; with the
    /// receivers alive, commands that arrive during shutdown stay queued.
    pub poll_result_tx: mpsc::Sender<PollResult<H>>,
    pub cmd_rx: mpsc::Receiver<EngineCmd>,
    pub ipc_rx: mpsc::Receiver<ipc::IpcRequest>,
    pub watchdog: Watchdog,
    pub start_time: Instant,
    /// The loop ended on an IPC `Shutdown` command.
    pub ipc_shutdown: bool,
    /// The signal that ended the loop, if one did.
    pub stop_signal: Option<&'static str>,
    #[cfg(feature = "svm")]
    pub svm_runner: Option<RustSvmRunner>,
}

/// Run the event loop until a stop signal or an IPC `Shutdown`.
pub async fn run<H, S>(parts: LoopParts<H>, config: &ServerConfig, signals: &mut S) -> LoopEnd<H>
where
    H: HalRead + HalWrite + HalDiagnostics + HalControl + Send + 'static,
    S: Signals,
{
    let LoopParts {
        engine,
        control_runner,
        mut cmd_rx,
        mut ipc_rx,
        mut poll_timer,
        mut watch_expiry_timer,
        mut watchdog_timer,
        mut watchdog,
        mut history_store,
        alert_manager,
        start_time,
        no_control,
        control_config,
        #[cfg(feature = "svm")]
        mut svm_runner,
        #[cfg(feature = "svm")]
        svm_snapshot,
    } = parts;

    // Watch subscription state (lives in main loop scope)
    let mut watches: HashMap<String, cmd_handler::WatchState> = HashMap::new();
    let mut watch_counter: u64 = 0;

    // Non-blocking poll: engine + control_runner wrapped in Option for spawn_blocking swap
    let mut engine: Option<Engine<H>> = Some(engine);
    let mut control_runner: Option<ControlRunner> = Some(control_runner);
    let (poll_result_tx, mut poll_result_rx) = mpsc::channel::<PollResult<H>>(1);
    let mut poll_in_progress = false;

    let mut ipc_shutdown = false;
    let mut stop_signal: Option<&'static str> = None;

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

                    // Check channel statuses for alert transitions
                    let snapshots: Vec<alerts::ChannelSnapshot> = eng
                        .channels
                        .iter()
                        .filter(|(_, ch)| ch.enabled)
                        .map(|(_, ch)| alerts::ChannelSnapshot {
                            id: ch.id,
                            name: ch.label.clone(),
                            status: ch.value.status.as_str().to_string(),
                            cur: ch.value.cur,
                        })
                        .collect();
                    if let Ok(mut mgr) = alert_manager.lock() {
                        mgr.check_channels(&snapshots);
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

                    // Resume pure Rust VM for one app cycle
                    #[cfg(feature = "svm")]
                    if let Some(ref mut runner) = svm_runner {
                        if let Err(e) = runner.resume() {
                            match e {
                                sandstar_svm::vm_error::VmError::Stopped => {
                                    info!("Sedona VM stopped");
                                    svm_runner = None;
                                }
                                _ => {
                                    debug!(err = %e, "SVM resume error");
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
                        config,
                        start_time,
                        watches: &mut watches,
                        watch_counter: &mut watch_counter,
                        history_store: &history_store,
                    };
                    cmd_handler::handle_engine_cmd(
                        cmd,
                        engine.as_mut().expect("select! guard ensures engine.is_some()"),
                        &mut ctx,
                    );
                }
            }

            // --- IPC command (only when engine is available) ---
            Some(req) = ipc_rx.recv(), if engine.is_some() => {
                let name = ipc::command_name(&req.cmd);
                if req.abandoned() {
                    // The client was told "not run" (deadline passed in the queue).
                    warn!(cmd = %name, "IPC: client gave up while the command was queued; not run");
                    continue;
                }
                let (conn_result, response) = dispatch::run(
                    req.cmd,
                    engine.as_mut().expect("select! guard ensures engine.is_some()"),
                    config,
                    start_time,
                    &history_store,
                );
                if req.reply.send(response).is_err() {
                    warn!(cmd = %name, "IPC: command ran, but the client gave up before the reply");
                }
                match conn_result {
                    dispatch::ConnectionResult::Shutdown => {
                        info!("shutdown requested via IPC");
                        // Let the reply reach the client before the runtime goes.
                        if !matches!(
                            tokio::time::timeout(ipc::SHUTDOWN_REPLY_WAIT, req.written).await,
                            Ok(Ok(()))
                        ) {
                            warn!("IPC: shutdown reply not confirmed as written");
                        }
                        ipc_shutdown = true;
                        break;
                    }
                    dispatch::ConnectionResult::PollNow => {
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
                    dispatch::ConnectionResult::Continue => {}
                }
            }

            // --- Signals: SIGHUP reloads; SIGINT and SIGTERM always stop ---
            signal = signals.next() => match signal {
                // --- SIGHUP: config reload ---
                Signal::Hup => {
                if let Some(ref mut eng) = engine {
                    if let Some(ref dir) = config.config_dir {
                        info!("SIGHUP received — reloading configuration");
                        match reload::reload_config(eng, dir) {
                            Ok(summary) => info!(%summary, "config reload complete"),
                            Err(e) => error!(err = %e, "config reload failed"),
                        }

                        // Reload control.toml if present
                        if !no_control && !config.read_only {
                            let control_path = control_config.clone()
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
                Signal::Sigint => {
                    info!("shutdown signal received (SIGINT / Ctrl+C)");
                    stop_signal = Some("SIGINT");
                    break;
                }
                Signal::Sigterm => {
                    info!("shutdown signal received (SIGTERM)");
                    stop_signal = Some("SIGTERM");
                    break;
                }
            },

            // --- Watch expiry timer (every 60s, clean stale subscriptions) ---
            _ = watch_expiry_timer.tick() => {
                cmd_handler::expire_stale_watches(&mut watches);
            }

            // --- Watchdog timer (always fires, even during blocking poll) ---
            _ = watchdog_timer.tick() => {
                watchdog.kick();
                sd_notify::watchdog();
            }

        }
    }

    LoopEnd {
        engine,
        poll_in_progress,
        poll_result_rx,
        poll_result_tx,
        cmd_rx,
        ipc_rx,
        watchdog,
        start_time,
        ipc_shutdown,
        stop_signal,
        #[cfg(feature = "svm")]
        svm_runner,
    }
}

/// The shutdown steps after the loop.
///
/// Returns true when a poll never came back: the caller must then leave
/// with `process::exit`, because dropping the runtime would wait for the
/// blocking task without limit.
pub async fn shutdown<H>(end: LoopEnd<H>, config: &ServerConfig) -> bool
where
    H: HalRead + HalWrite + HalDiagnostics + HalControl + Send + 'static,
{
    let LoopEnd {
        mut engine,
        poll_in_progress,
        mut poll_result_rx,
        poll_result_tx: _poll_result_tx,
        cmd_rx: _cmd_rx,
        ipc_rx: _ipc_rx,
        mut watchdog,
        start_time,
        ipc_shutdown,
        stop_signal,
        #[cfg(feature = "svm")]
        mut svm_runner,
    } = end;

    // 13. Graceful shutdown
    // Tell systemd we are stopping (matters for an IPC shutdown, where systemd
    // did not ask for it).
    sd_notify::notify("STOPPING=1");
    // A poll that never comes back keeps a blocking task the runtime would
    // wait for forever on exit; then we leave with process::exit below.
    let mut poll_stuck = false;
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
                poll_stuck = true;
            }
        }
    }
    // The loop no longer feeds the watchdogs; the poll wait took up to 5 s.
    watchdog.kick();
    sd_notify::watchdog();

    // Stop Sedona VM if running
    #[cfg(feature = "svm")]
    if let Some(ref mut runner) = svm_runner {
        runner.stop();
    }

    let uptime = start_time.elapsed();
    let reason = if ipc_shutdown {
        "IPC command"
    } else {
        stop_signal.unwrap_or("signal")
    };
    info!(uptime_secs = uptime.as_secs(), reason, "engine stopped");
    if let Some(ref mut eng) = engine {
        if let Err(e) = eng.hal.shutdown() {
            warn!(err = %e, "HAL shutdown error");
        }
    } else {
        warn!("HAL shutdown skipped: the engine did not come back from its poll");
    }
    watchdog.close();
    ipc::cleanup(&config.socket_path);
    poll_stuck
}
