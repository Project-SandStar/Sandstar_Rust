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

/// Reload the configuration directory, and `control.toml` with it, on SIGHUP.
/// A failure is logged and leaves the previous state in place.
fn reload_on_sighup<H>(
    engine: &mut Engine<H>,
    control_runner: &mut Option<ControlRunner>,
    config: &ServerConfig,
    no_control: bool,
    control_config: &Option<std::path::PathBuf>,
) where
    H: HalRead + HalWrite + HalDiagnostics + HalControl,
{
    let Some(ref dir) = config.config_dir else {
        warn!("SIGHUP ignored — no config dir (demo mode)");
        return;
    };
    info!("reloading configuration");
    let mut control = control_target(control_runner, no_control, control_config);
    match reload::reload_all(engine, dir, config.read_only, control.as_mut()) {
        Ok(summary) => info!(%summary, "config reload complete"),
        Err(e) => error!(err = %e, "config reload failed"),
    }
}

/// The control runner as a reload target. `None` while a poll has the runner.
fn control_target<'a>(
    control_runner: &'a mut Option<ControlRunner>,
    no_control: bool,
    control_config: &'a Option<std::path::PathBuf>,
) -> Option<reload::ControlTarget<'a>> {
    control_runner.as_mut().map(|runner| reload::ControlTarget {
        runner,
        no_control,
        path: control_config.as_deref(),
    })
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
    // A SIGHUP arrived while a poll had the engine.
    let mut reload_pending = false;

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

                // A SIGHUP that arrived during this poll: reload now.
                if reload_pending {
                    reload_pending = false;
                    if let Some(ref mut eng) = engine {
                        info!("running the reload deferred by the poll");
                        reload_on_sighup(eng, &mut control_runner, config, no_control, &control_config);
                    }
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
                        control: control_target(&mut control_runner, no_control, &control_config),
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
                let mut control = control_target(&mut control_runner, no_control, &control_config);
                let (conn_result, response) = dispatch::run(
                    req.cmd,
                    engine.as_mut().expect("select! guard ensures engine.is_some()"),
                    config,
                    start_time,
                    &history_store,
                    control.as_mut(),
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
                        info!("SIGHUP received");
                        reload_on_sighup(eng, &mut control_runner, config, no_control, &control_config);
                    } else {
                        // The engine is away with the poll; the reload runs
                        // when the poll hands it back.
                        info!("SIGHUP received — deferring reload (poll in progress)");
                        reload_pending = true;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use sandstar_engine::channel::{Channel, ChannelDirection, ChannelType};
    use sandstar_engine::value::ValueConv;
    use sandstar_hal::mock::MockHal;
    use sandstar_ipc::types::{EngineCommand, EngineResponse};
    use tokio::sync::oneshot;

    const CH: u32 = 1100;

    /// Signals from a channel. A closed channel means "no more signals".
    struct TestSignals(mpsc::Receiver<Signal>);

    impl Signals for TestSignals {
        async fn next(&mut self) -> Signal {
            match self.0.recv().await {
                Some(signal) => signal,
                None => std::future::pending().await,
            }
        }
    }

    fn config(config_dir: Option<PathBuf>, poll_interval_ms: u64) -> ServerConfig {
        ServerConfig {
            socket_path: String::new(),
            poll_interval_ms,
            config_dir,
            read_only: false,
            auth_store: crate::auth::AuthStore::new(),
            auth_token: None,
            rate_limit: 0,
        }
    }

    /// An engine with one polled analog input that reads `raw`.
    fn engine(raw: f64) -> Engine<MockHal> {
        let mut engine = Engine::new(MockHal::new());
        let ch = Channel::new(
            CH,
            ChannelType::Analog,
            ChannelDirection::In,
            0,
            0,
            false,
            ValueConv::default(),
            "in",
        );
        engine.channels.add(ch).unwrap();
        engine.channels.get_mut(CH).unwrap().enabled = true;
        engine.polls.add(CH).unwrap();
        engine.hal.set_analog(0, 0, Ok(raw));
        engine
    }

    /// The handles a test drives the loop with.
    struct Driver {
        cmd: mpsc::Sender<EngineCmd>,
        ipc: mpsc::Sender<ipc::IpcRequest>,
        signals: mpsc::Sender<Signal>,
    }

    fn parts(engine: Engine<MockHal>, poll_ms: u64) -> (LoopParts<MockHal>, Driver, TestSignals) {
        parts_with(engine, poll_ms, true)
    }

    fn parts_with(
        engine: Engine<MockHal>,
        poll_ms: u64,
        no_control: bool,
    ) -> (LoopParts<MockHal>, Driver, TestSignals) {
        let (cmd, cmd_rx) = mpsc::channel(64);
        let (ipc_tx, ipc_rx) = mpsc::channel(8);
        let (signals, signal_rx) = mpsc::channel(8);
        let timer = |ms: u64| {
            let mut t = tokio::time::interval(Duration::from_millis(ms));
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            t
        };
        let parts = LoopParts {
            engine,
            control_runner: ControlRunner::default(),
            cmd_rx,
            ipc_rx,
            poll_timer: timer(poll_ms),
            watch_expiry_timer: timer(60_000),
            watchdog_timer: timer(500),
            watchdog: Watchdog::disabled(),
            history_store: HistoryStore::new(100),
            alert_manager: Arc::new(Mutex::new(alerts::AlertManager::new(
                alerts::AlertConfig::default(),
            ))),
            start_time: Instant::now(),
            no_control,
            control_config: None,
            #[cfg(feature = "svm")]
            svm_runner: None,
            #[cfg(feature = "svm")]
            svm_snapshot: Arc::new(RwLock::new(ChannelSnapshot::new())),
        };
        let driver = Driver {
            cmd,
            ipc: ipc_tx,
            signals,
        };
        (parts, driver, TestSignals(signal_rx))
    }

    impl Driver {
        async fn status(&self) -> sandstar_ipc::types::StatusInfo {
            let (reply, rx) = oneshot::channel();
            self.cmd.send(EngineCmd::Status { reply }).await.unwrap();
            rx.await.expect("the loop dropped a Status command")
        }

        async fn tables(&self) -> Vec<String> {
            let (reply, rx) = oneshot::channel();
            self.cmd
                .send(EngineCmd::ListTables { reply })
                .await
                .unwrap();
            rx.await.expect("the loop dropped a ListTables command")
        }

        async fn reload(&self) -> Result<String, String> {
            let (reply, rx) = oneshot::channel();
            self.cmd
                .send(EngineCmd::ReloadConfig { reply })
                .await
                .unwrap();
            rx.await.expect("the loop dropped a ReloadConfig command")
        }

        async fn polls(&self) -> Vec<sandstar_ipc::types::PollInfo> {
            let (reply, rx) = oneshot::channel();
            self.cmd.send(EngineCmd::ListPolls { reply }).await.unwrap();
            rx.await.expect("the loop dropped a ListPolls command")
        }

        /// Wait until a poll has stored a value for the channel.
        async fn wait_for_a_poll(&self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let polls = self.polls().await;
                if polls
                    .iter()
                    .any(|p| p.channel == CH && p.last_status != "Unknown")
                {
                    return;
                }
                assert!(Instant::now() < deadline, "no poll within 5 s: {polls:?}");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        async fn ipc(&self, cmd: EngineCommand) -> EngineResponse {
            let (reply, rx) = oneshot::channel();
            let (written_tx, written) = oneshot::channel();
            self.ipc
                .send(ipc::IpcRequest {
                    cmd,
                    reply,
                    written,
                })
                .await
                .unwrap();
            let response = rx.await.expect("the loop dropped an IPC command");
            // The connection task reports the reply as written.
            let _ = written_tx.send(());
            response
        }
    }

    /// Run the loop next to `drive`; the loop's end state comes back.
    async fn run_with<F, Fut>(
        engine: Engine<MockHal>,
        config: &ServerConfig,
        drive: F,
    ) -> LoopEnd<MockHal>
    where
        F: FnOnce(Driver) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        run_parts(parts(engine, config.poll_interval_ms), config, drive).await
    }

    async fn run_parts<F, Fut>(
        (parts, driver, mut signals): (LoopParts<MockHal>, Driver, TestSignals),
        config: &ServerConfig,
        drive: F,
    ) -> LoopEnd<MockHal>
    where
        F: FnOnce(Driver) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let looped = run(parts, config, &mut signals);
        let (end, ()) = tokio::time::timeout(Duration::from_secs(20), async {
            tokio::join!(looped, drive(driver))
        })
        .await
        .expect("the loop did not stop");
        end
    }

    #[tokio::test]
    async fn a_poll_tick_reads_the_channel_and_the_engine_comes_back() {
        let config = config(None, 10);
        let end = run_with(engine(2048.0), &config, |d| async move {
            d.wait_for_a_poll().await;
            let polls = d.polls().await;
            let poll = polls.iter().find(|p| p.channel == CH).unwrap();
            assert_eq!(poll.last_status, "Ok", "{poll:?}");
            assert_eq!(poll.last_cur, 2048.0);
            d.signals.send(Signal::Sigterm).await.unwrap();
        })
        .await;

        assert_eq!(end.stop_signal, Some("SIGTERM"));
        assert!(!end.ipc_shutdown);
        // The engine is either back or still out with a poll that started
        // just before the stop; shutdown brings it back in that case.
        assert!(end.engine.is_some() || end.poll_in_progress);
        assert!(!shutdown(end, &config).await);
    }

    #[tokio::test]
    async fn every_command_is_answered_while_polls_keep_taking_the_engine() {
        // 1 ms polls: the engine is away with a blocking task most of the time.
        let config = config(None, 1);
        let end = run_with(engine(100.0), &config, |d| async move {
            d.wait_for_a_poll().await;
            for _ in 0..200 {
                let status = d.status().await;
                assert_eq!(status.channel_count, 1);
                assert_eq!(status.poll_count, 1);
            }
            d.signals.send(Signal::Sigint).await.unwrap();
        })
        .await;
        assert_eq!(end.stop_signal, Some("SIGINT"));
    }

    #[tokio::test]
    async fn poll_now_over_rest_and_ipc_is_answered() {
        // No timer poll after the first one: the interval is long.
        let config = config(None, 60_000);
        run_with(engine(7.0), &config, |d| async move {
            d.wait_for_a_poll().await;
            let (reply, rx) = oneshot::channel();
            d.cmd.send(EngineCmd::PollNow { reply }).await.unwrap();
            assert_eq!(rx.await.unwrap(), Ok("poll triggered".to_string()));
            // The engine is back before the next command is served.
            assert_eq!(d.status().await.poll_count, 1);
            let response = d.ipc(EngineCommand::PollNow).await;
            assert!(
                !matches!(response, EngineResponse::Error(_)),
                "{response:?}"
            );
            assert_eq!(d.status().await.poll_count, 1);
            d.signals.send(Signal::Sigterm).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn an_ipc_shutdown_ends_the_loop_after_its_reply() {
        let config = config(None, 60_000);
        let end = run_with(engine(1.0), &config, |d| async move {
            d.wait_for_a_poll().await;
            let response = d.ipc(EngineCommand::Shutdown).await;
            assert!(
                !matches!(response, EngineResponse::Error(_)),
                "{response:?}"
            );
        })
        .await;

        assert!(end.ipc_shutdown);
        assert_eq!(end.stop_signal, None);
        assert!(end.engine.is_some());
    }

    /// The client gave up while the command was queued: it must not run.
    /// A `Shutdown` that ran would end the loop with `ipc_shutdown`.
    #[tokio::test]
    async fn an_ipc_command_whose_client_gave_up_is_not_run() {
        let config = config(None, 60_000);
        let end = run_with(engine(1.0), &config, |d| async move {
            d.wait_for_a_poll().await;
            let (reply, rx) = oneshot::channel();
            let (_written_tx, written) = oneshot::channel();
            drop(rx);
            d.ipc
                .send(ipc::IpcRequest {
                    cmd: EngineCommand::Shutdown,
                    reply,
                    written,
                })
                .await
                .unwrap();
            // The loop is still there and still answers.
            assert_eq!(d.status().await.channel_count, 1);
            assert!(matches!(
                d.ipc(EngineCommand::Status).await,
                EngineResponse::Status(_)
            ));
            d.signals.send(Signal::Sigterm).await.unwrap();
        })
        .await;

        assert!(!end.ipc_shutdown);
        assert_eq!(end.stop_signal, Some("SIGTERM"));
    }

    fn write_tables(dir: &std::path::Path) {
        std::fs::write(
            dir.join("tables.csv"),
            "name,description,path,unit_type,f_min,f_max,c_min,c_max,k_min,k_max,tag\n\
             T,Test,t.txt,temp,-40,303,-40,150,233,423,testTherm\n",
        )
        .unwrap();
        std::fs::write(dir.join("t.txt"), "100\n200\n300\n").unwrap();
    }

    #[tokio::test]
    async fn sighup_reloads_the_configuration() {
        let dir = tempfile::tempdir().unwrap();
        write_tables(dir.path());
        let config = config(Some(dir.path().to_path_buf()), 60_000);
        run_with(engine(1.0), &config, |d| async move {
            d.wait_for_a_poll().await;
            assert!(d.tables().await.is_empty());
            d.signals.send(Signal::Hup).await.unwrap();
            // Commands queue behind the signal; the reload is done by the
            // time one is answered, or shortly after.
            let deadline = Instant::now() + Duration::from_secs(5);
            while d.tables().await.len() != 1 {
                assert!(Instant::now() < deadline, "SIGHUP did not load the table");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            d.signals.send(Signal::Sigterm).await.unwrap();
        })
        .await;
    }

    /// A SIGHUP that arrives while a poll has the engine is not lost: the
    /// reload runs when the poll hands the engine back.
    #[tokio::test]
    async fn sighup_during_a_poll_reloads_when_the_poll_returns() {
        let dir = tempfile::tempdir().unwrap();
        write_tables(dir.path());
        let config = config(Some(dir.path().to_path_buf()), 60_000);
        let engine = engine(1.0);
        // The first poll starts within a few ms and holds the engine for 500 ms.
        engine.hal.set_read_delay(Duration::from_millis(500));
        run_with(engine, &config, |d| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let sent = Instant::now();
            d.signals.send(Signal::Hup).await.unwrap();
            // Answered only once the poll has returned the engine.
            let tables = d.tables().await;
            assert!(
                sent.elapsed() >= Duration::from_millis(200),
                "the signal did not arrive during the poll: {:?}",
                sent.elapsed()
            );
            assert_eq!(tables.len(), 1, "the deferred reload did not run");
            assert_eq!(d.status().await.poll_count, 1);
            d.signals.send(Signal::Sigterm).await.unwrap();
        })
        .await;
    }

    /// REST, IPC and SIGHUP reload the same things, control.toml included,
    /// and a reload of an unchanged control.toml leaves the runner alone
    /// (R-29, S-64).
    #[tokio::test]
    async fn every_reload_path_reloads_the_control_file() {
        use crate::reload::test_support::{control_toml, write_config_dir};

        let dir = tempfile::tempdir().unwrap();
        let control = dir.path().join("control.toml");
        std::fs::write(&control, control_toml(1.0)).unwrap();
        let config = config(Some(dir.path().to_path_buf()), 60_000);
        let dir_path = dir.path().to_path_buf();
        let parts = parts_with(engine(1.0), 60_000, false);
        run_parts(parts, &config, |d| async move {
            d.wait_for_a_poll().await;
            write_config_dir(&dir_path);

            // REST: the loop started without control loops, the file has one.
            let text = d.reload().await.unwrap();
            assert!(
                text.ends_with("; control: reloaded 1 loops, 0 components"),
                "{text}"
            );
            let text = d.reload().await.unwrap();
            assert!(text.ends_with("; control: unchanged"), "{text}");

            // IPC: a changed file is loaded; REST then finds it unchanged.
            std::fs::write(&control, control_toml(2.0)).unwrap();
            assert!(matches!(
                d.ipc(EngineCommand::ReloadConfig).await,
                EngineResponse::Ok
            ));
            let text = d.reload().await.unwrap();
            assert!(text.ends_with("; control: unchanged"), "{text}");

            // SIGHUP: the same. Its reload is done once the second table shows.
            std::fs::write(&control, control_toml(3.0)).unwrap();
            std::fs::write(
                dir_path.join("tables.csv"),
                "name,description,path,unit_type,f_min,f_max,c_min,c_max,k_min,k_max,tag\n\
                 T,Test,t.txt,temp,-40,303,-40,150,233,423,testTherm\n\
                 U,Test,t.txt,temp,-40,303,-40,150,233,423,otherTherm\n",
            )
            .unwrap();
            d.signals.send(Signal::Hup).await.unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while d.tables().await.len() != 2 {
                assert!(Instant::now() < deadline, "SIGHUP did not reload");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let text = d.reload().await.unwrap();
            assert!(text.ends_with("; control: unchanged"), "{text}");
            d.signals.send(Signal::Sigterm).await.unwrap();
        })
        .await;
    }

    /// A reload that fails changes nothing and the loop carries on.
    #[tokio::test]
    async fn sighup_with_a_bad_configuration_keeps_the_state_and_the_loop() {
        let good = tempfile::tempdir().unwrap();
        write_tables(good.path());
        let mut engine = engine(1.0);
        crate::reload::reload_config(&mut engine, good.path()).unwrap();
        assert_eq!(engine.tables.count(), 1);

        let bad = tempfile::tempdir().unwrap();
        // Unreadable: a directory where the file should be.
        std::fs::create_dir(bad.path().join("tables.csv")).unwrap();
        let config = config(Some(bad.path().to_path_buf()), 60_000);
        let end = run_with(engine, &config, |d| async move {
            d.wait_for_a_poll().await;
            d.signals.send(Signal::Hup).await.unwrap();
            for _ in 0..20 {
                assert_eq!(d.tables().await.len(), 1);
                assert_eq!(d.status().await.poll_count, 1);
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            d.signals.send(Signal::Sigterm).await.unwrap();
        })
        .await;
        assert_eq!(end.stop_signal, Some("SIGTERM"));
    }

    #[tokio::test]
    async fn sighup_without_a_config_dir_is_ignored() {
        let config = config(None, 60_000);
        run_with(engine(1.0), &config, |d| async move {
            d.wait_for_a_poll().await;
            d.signals.send(Signal::Hup).await.unwrap();
            assert_eq!(d.status().await.channel_count, 1);
            d.signals.send(Signal::Sigterm).await.unwrap();
        })
        .await;
    }

    // ── shutdown ──

    fn end_with(
        engine: Option<Engine<MockHal>>,
        poll_in_progress: bool,
    ) -> (LoopEnd<MockHal>, mpsc::Sender<PollResult<MockHal>>) {
        let (tx, poll_result_rx) = mpsc::channel(1);
        let (_cmd_tx, cmd_rx) = mpsc::channel(1);
        let (_ipc_tx, ipc_rx) = mpsc::channel(1);
        let end = LoopEnd {
            engine,
            poll_in_progress,
            poll_result_rx,
            poll_result_tx: tx.clone(),
            cmd_rx,
            ipc_rx,
            watchdog: Watchdog::disabled(),
            start_time: Instant::now(),
            ipc_shutdown: false,
            stop_signal: Some("SIGTERM"),
            #[cfg(feature = "svm")]
            svm_runner: None,
        };
        (end, tx)
    }

    #[tokio::test]
    async fn shutdown_with_the_engine_at_hand_is_not_stuck() {
        let config = config(None, 1000);
        let (end, _tx) = end_with(Some(engine(1.0)), false);
        assert!(!shutdown(end, &config).await);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_waits_for_a_poll_that_comes_back() {
        let config = config(None, 1000);
        let (end, tx) = end_with(None, true);
        let late = async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            tx.send((
                engine(1.0),
                ControlRunner::default(),
                Vec::new(),
                Duration::from_secs(3),
            ))
            .await
            .unwrap();
        };
        let started = tokio::time::Instant::now();
        let (stuck, ()) = tokio::join!(shutdown(end, &config), late);
        assert!(!stuck);
        // It did wait for the poll (3 s), and not for the whole 5 s.
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_secs(3) && waited < Duration::from_secs(4),
            "{waited:?}"
        );
    }

    /// The poll never returns: after 5 s shutdown reports it, so `main` can
    /// leave with `process::exit` instead of waiting for the blocking task.
    #[tokio::test(start_paused = true)]
    async fn shutdown_gives_up_on_a_poll_after_five_seconds() {
        let config = config(None, 1000);
        let (end, _tx) = end_with(None, true);
        let started = tokio::time::Instant::now();
        assert!(shutdown(end, &config).await);
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_secs(5) && waited < Duration::from_secs(6),
            "{waited:?}"
        );
    }

    /// The blocking task died without sending the engine back. The loop's
    /// own sender keeps the channel open, so this is a stuck poll (5 s and
    /// `process::exit` in `main`), not a closed channel.
    #[tokio::test(start_paused = true)]
    async fn shutdown_treats_a_poll_task_that_died_as_stuck() {
        let config = config(None, 1000);
        let (end, tx) = end_with(None, true);
        drop(tx);
        assert!(shutdown(end, &config).await);
    }

    /// What the loop hands to shutdown keeps the channels as `main()` used to.
    #[tokio::test]
    async fn the_loop_end_keeps_the_command_channels_open() {
        let config = config(None, 60_000);
        let (parts, driver, mut signals) = parts(engine(1.0), 60_000);
        driver.signals.send(Signal::Sigterm).await.unwrap();
        let end = run(parts, &config, &mut signals).await;
        assert!(!driver.cmd.is_closed());
        assert!(!driver.ipc.is_closed());
        assert!(!end.poll_result_tx.is_closed());
        drop(end);
        assert!(driver.cmd.is_closed());
    }
}
