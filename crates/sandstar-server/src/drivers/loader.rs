//! Generic driver loader — collapses the per-driver-type boilerplate that used
//! to live as separate `load_bacnet_drivers` / `load_mqtt_drivers` functions
//! in `rest/mod.rs`.
//!
//! Phase 12.0A of Driver Framework v2. See
//! [`docs/Progress/IMPLEMENTATION_PLAN_DRIVER_FRAMEWORK.md`] for the plan.
//!
//! ## Shape of the duplication we collapsed
//!
//! BACnet and MQTT drivers each had ~200 lines of essentially identical
//! startup glue: parse env var → register drivers → register points → add
//! poll buckets → open_all → spawn tick task → sync_all → write_channel.
//! The only differences were:
//!
//! 1. Env var name (`SANDSTAR_BACNET_CONFIGS` vs `SANDSTAR_MQTT_CONFIGS`)
//! 2. Config struct type (`BacnetConfig` vs `MqttConfig`)
//! 3. Driver constructor (`BacnetDriver::from_config` vs `MqttDriver::from_config`)
//! 4. Log prefix ("BACnet" vs "MQTT")
//! 5. `who` prefix in `write_channel` ("bacnet:" vs "mqtt:")
//!
//! The [`DriverLoader`] trait captures those 5 knobs; [`load_drivers`] does
//! everything else.

use std::collections::HashMap;
use std::time::Duration;

use super::actor::DriverHandle;
use super::async_driver::{AnyDriver, AsyncDriver};
use super::{DriverError, DriverId, DriverPointRef};
use crate::rest::EngineHandle;

/// Per-driver-type specialization knobs used by [`load_drivers`].
///
/// Implement this for each driver type we want to load from an env var
/// config. See [`crate::drivers::bacnet::BacnetLoader`] and
/// [`crate::drivers::mqtt::MqttLoader`] for the two current impls.
pub trait DriverLoader: 'static {
    /// Environment variable holding the JSON config array.
    const ENV_VAR: &'static str;
    /// Lowercase driver type identifier used for the `who` tag in
    /// `write_channel`. Should match [`AsyncDriver::driver_type`].
    /// Examples: `"bacnet"`, `"mqtt"`.
    const DRIVER_TYPE: &'static str;
    /// Display label used in log messages. Example: `"BACnet"`, `"MQTT"`.
    const LABEL: &'static str;

    /// Config struct deserialized from the env var JSON array element.
    type Config: serde::de::DeserializeOwned + Send + Sync + 'static;

    /// Extract the driver id from a parsed config.
    fn config_id(config: &Self::Config) -> String;

    /// Extract the list of Sandstar channel ids this driver will poll.
    fn config_point_ids(config: &Self::Config) -> Vec<u32>;

    /// Construct the concrete driver from its config, boxed as an
    /// `AsyncDriver`. The returned driver is then wrapped in
    /// [`AnyDriver::Async`] and registered with the actor.
    fn build_driver(config: Self::Config) -> Box<dyn AsyncDriver>;
}

/// Write priority used by the tick task for driver-reported values.
///
/// Level 16 is the lowest priority in the Haystack/BACnet priority array —
/// any operator or control-logic write at a lower level always overrides.
/// Kept public so callers can match the constant in tests.
pub const DRIVER_WRITE_LEVEL: u8 = 16;

/// Duration (seconds) after which a written value expires if not refreshed.
/// Deliberately longer than the 5s poll interval so successful polls leave
/// no gap, but short enough to detect a stalled driver.
pub const DRIVER_WRITE_DURATION_SECS: f64 = 30.0;

/// Poll interval used by the tick task. Applied both to the `add_poll_bucket`
/// registration and the `tokio::time::interval` inside the tick task.
pub const DRIVER_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// While points keep failing, the tick task repeats a `warn` summary this
/// often; the per-point errors themselves are logged only when they start
/// or change (R-25, S-49).
pub const FAILURE_REMINDER_INTERVAL: Duration = Duration::from_secs(600);

/// Generic driver loader — reads the env var, parses configs, registers
/// drivers, wires up poll buckets, calls `open_all`, and spawns the periodic
/// tick task that flows `sync_cur` results into engine channels.
///
/// Errors at any stage are logged but do not abort the caller (identical
/// semantics to the previous `load_bacnet_drivers` / `load_mqtt_drivers`).
pub async fn load_drivers<L: DriverLoader>(handle: &DriverHandle, engine_handle: &EngineHandle) {
    let json_str = match std::env::var(L::ENV_VAR) {
        Ok(s) => s,
        Err(_) => return, // Not configured — skip silently.
    };

    let configs: Vec<L::Config> = match serde_json::from_str(&json_str) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                env_var = L::ENV_VAR,
                error = %e,
                "{}: failed to parse JSON",
                L::ENV_VAR
            );
            return;
        }
    };

    let mut registered_any = false;
    // Track each driver's configured points so we can wire them into the
    // poll scheduler after open_all() succeeds.
    let mut driver_points: Vec<(String, Vec<u32>)> = Vec::new();
    for config in configs {
        let id = L::config_id(&config);
        let point_ids = L::config_point_ids(&config);
        let driver = L::build_driver(config);
        let any_driver = AnyDriver::Async(driver);
        match handle.register(any_driver).await {
            Ok(()) => {
                tracing::info!(driver = %id, "{} driver registered", L::LABEL);
                registered_any = true;
                driver_points.push((id, point_ids));
            }
            Err(e) => {
                tracing::error!(
                    driver = %id,
                    error = %e,
                    "failed to register {} driver",
                    L::LABEL
                );
            }
        }
    }

    if !registered_any {
        return;
    }

    // Register each configured point with its driver and add a 5s poll bucket
    // so sync_cur() is invoked periodically. Even for push-based protocols
    // like MQTT, this tick is how cached values flow into engine channels.
    for (driver_id, point_ids) in &driver_points {
        if point_ids.is_empty() {
            continue;
        }
        for pid in point_ids {
            if let Err(e) = handle.register_point(*pid, driver_id).await {
                tracing::warn!(
                    driver = %driver_id,
                    point_id = *pid,
                    error = %e,
                    "{} register_point failed",
                    L::LABEL
                );
            }
        }
        let points: Vec<DriverPointRef> = point_ids
            .iter()
            .map(|&pid| DriverPointRef {
                point_id: pid,
                address: String::new(),
            })
            .collect();
        match handle
            .add_poll_bucket(driver_id, DRIVER_POLL_INTERVAL, points)
            .await
        {
            Ok(()) => tracing::info!(
                driver = %driver_id,
                points = point_ids.len(),
                "{} poll bucket added (5s interval)",
                L::LABEL
            ),
            Err(e) => tracing::warn!(
                driver = %driver_id,
                error = %e,
                "{} add_poll_bucket failed",
                L::LABEL
            ),
        }
    }

    // Open all registered drivers (binds sockets, runs discovery, etc).
    // Non-fatal on failure — individual driver status will reflect any
    // per-driver problems.
    match handle.open_all().await {
        Ok(metas) => {
            tracing::info!(count = metas.len(), "{} drivers opened", L::LABEL);
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to open {} drivers", L::LABEL);
        }
    }

    // Spawn the periodic tick task. Without this, the poll buckets we just
    // added sit idle — the driver actor's loop is command-driven and has
    // no internal scheduler.
    let tick_points: HashMap<DriverId, Vec<DriverPointRef>> = driver_points
        .into_iter()
        .filter(|(_, pids)| !pids.is_empty())
        .map(|(id, pids)| {
            let refs = pids
                .into_iter()
                .map(|pid| DriverPointRef {
                    point_id: pid,
                    address: String::new(),
                })
                .collect();
            (id, refs)
        })
        .collect();
    if tick_points.is_empty() {
        return;
    }

    let handle_tick = handle.clone();
    let engine_tick = engine_handle.clone();
    tokio::spawn(async move {
        let mut tick_log = TickLog::default();
        let mut ticker = tokio::time::interval(DRIVER_POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Discard the immediate first tick so we don't race open_all.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let results = match handle_tick.sync_all(tick_points.clone()).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "{} poll tick: sync_all failed",
                        L::LABEL
                    );
                    continue;
                }
            };
            let counts = deliver_tick(
                &engine_tick,
                L::LABEL,
                L::DRIVER_TYPE,
                &results,
                &mut tick_log,
            )
            .await;
            log_tick_summary(L::LABEL, &counts, &mut tick_log);
        }
    });
    tracing::info!("{} poll tick task spawned (5s interval)", L::LABEL);
}

/// Log the counts of one tick: when they differ from the last tick at
/// `warn` if any point failed, otherwise `info`; `debug` when they
/// repeat; and a `warn` reminder every [`FAILURE_REMINDER_INTERVAL`]
/// while points keep failing.
fn log_tick_summary(label: &str, counts: &TickCounts, log: &mut TickLog) {
    match log.summary(counts, tokio::time::Instant::now()) {
        Summary::Changed if counts.err > 0 || counts.write_err > 0 => tracing::warn!(
            ok = counts.ok,
            err = counts.err,
            write_err = counts.write_err,
            write_skipped = counts.write_skipped,
            "{} poll tick complete",
            label
        ),
        Summary::Changed => tracing::info!(
            ok = counts.ok,
            err = counts.err,
            write_err = counts.write_err,
            write_skipped = counts.write_skipped,
            "{} poll tick complete",
            label
        ),
        Summary::Same => tracing::debug!(
            ok = counts.ok,
            err = counts.err,
            write_err = counts.write_err,
            write_skipped = counts.write_skipped,
            "{} poll tick complete",
            label
        ),
        Summary::Reminder { points, for_secs } => tracing::warn!(
            points,
            for_secs,
            ok = counts.ok,
            err = counts.err,
            write_err = counts.write_err,
            "{} poll: points still failing",
            label
        ),
    }
}

/// How a point's result compares with its previous one.
#[derive(Debug, PartialEq, Eq)]
enum PointChange {
    /// Ok now, and it was ok (or never read) before.
    StillOk,
    /// Ok now after an error.
    Recovered,
    /// An error that is new, or differs from the previous one.
    Failed,
    /// The same error as on the previous tick.
    StillFailing,
}

/// What one tick's summary line should be.
#[derive(Debug, PartialEq, Eq)]
enum Summary {
    /// The counts differ from the previous tick.
    Changed,
    /// The counts repeat.
    Same,
    /// Points have been failing and a reminder is due (whether or not the
    /// counts changed).
    Reminder { points: usize, for_secs: u64 },
}

/// Results of earlier ticks, so that a result repeated every tick is not
/// logged every tick (R-25, S-49).
#[derive(Debug, Default)]
struct TickLog {
    /// Current error per point; absent = the last result was ok.
    failing: HashMap<(DriverId, u32), String>,
    last_counts: Option<TickCounts>,
    /// When points started failing (without a tick free of failures since).
    failing_since: Option<tokio::time::Instant>,
    /// When the last reminder was logged.
    last_reminder: Option<tokio::time::Instant>,
}

impl TickLog {
    /// Record a point's result (`None` = ok) and say how it changed.
    fn point(&mut self, driver: &str, point: u32, error: Option<String>) -> PointChange {
        let key = (driver.to_string(), point);
        match error {
            None => match self.failing.remove(&key) {
                Some(_) => PointChange::Recovered,
                None => PointChange::StillOk,
            },
            Some(e) => match self.failing.insert(key, e.clone()) {
                Some(prev) if prev == e => PointChange::StillFailing,
                _ => PointChange::Failed,
            },
        }
    }

    /// Record a tick's counts and say how its summary should be logged.
    fn summary(&mut self, counts: &TickCounts, now: tokio::time::Instant) -> Summary {
        if self.failing.is_empty() {
            self.failing_since = None;
            self.last_reminder = None;
        } else if self.failing_since.is_none() {
            self.failing_since = Some(now);
        }
        let changed = self.last_counts.as_ref() != Some(counts);
        self.last_counts = Some(counts.clone());
        if let Some(since) = self.failing_since {
            let last = self.last_reminder.unwrap_or(since);
            if now.saturating_duration_since(last) >= FAILURE_REMINDER_INTERVAL {
                self.last_reminder = Some(now);
                return Summary::Reminder {
                    points: self.failing.len(),
                    for_secs: now.saturating_duration_since(since).as_secs(),
                };
            }
        }
        if changed {
            Summary::Changed
        } else {
            Summary::Same
        }
    }
}

/// Counts for one driver poll tick.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct TickCounts {
    /// Points whose `sync_cur` returned a value.
    ok: usize,
    /// Points whose `sync_cur` failed (remote device error).
    err: usize,
    /// Values the engine rejected or did not answer.
    write_err: usize,
    /// Values not sent after an engine timeout in this tick.
    write_skipped: usize,
}

/// Write one tick's `sync_cur` results into engine channels.
///
/// After one engine timeout the remaining writes of the tick are skipped:
/// each would wait another `ENGINE_REPLY_TIMEOUT` (R-28). Skipped writes
/// are counted separately, not as write errors.
///
/// A point's error is logged at `warn` when it starts or changes, and at
/// `debug` while it repeats; a recovery is logged at `info` (S-49).
async fn deliver_tick(
    engine: &EngineHandle,
    label: &str,
    driver_type: &str,
    results: &[(DriverId, u32, Result<f64, DriverError>)],
    log: &mut TickLog,
) -> TickCounts {
    let mut counts = TickCounts::default();
    let mut engine_timed_out = false;
    for (driver_id, point_id, res) in results {
        match res {
            Ok(_) if engine_timed_out => {
                counts.ok += 1;
                counts.write_skipped += 1;
            }
            Ok(v) => {
                counts.ok += 1;
                tracing::debug!(
                    driver = %driver_id,
                    point_id,
                    value = v,
                    "{} sync_cur -> write_channel",
                    label
                );
                let who = format!("{}:{}", driver_type, driver_id);
                match engine
                    .write_channel(
                        *point_id,
                        Some(*v),
                        DRIVER_WRITE_LEVEL,
                        who,
                        DRIVER_WRITE_DURATION_SECS,
                    )
                    .await
                {
                    Ok(()) => {
                        if log.point(driver_id, *point_id, None) == PointChange::Recovered {
                            tracing::info!(
                                driver = %driver_id,
                                point_id,
                                value = v,
                                "{} point recovered",
                                label
                            );
                        }
                    }
                    // The rest of the tick is skipped; the point's state is
                    // left as it was (not known this tick).
                    Err(e) if crate::rest::is_engine_timeout(&e) => {
                        counts.write_err += 1;
                        engine_timed_out = true;
                        tracing::warn!(
                            driver = %driver_id,
                            point_id,
                            error = %e,
                            "{} engine did not answer write_channel; \
                             skipping the remaining writes of this tick",
                            label
                        );
                    }
                    Err(e) => {
                        counts.write_err += 1;
                        let text = format!("write_channel: {e}");
                        if log.point(driver_id, *point_id, Some(text)) == PointChange::Failed {
                            tracing::warn!(
                                driver = %driver_id,
                                point_id,
                                error = %e,
                                "{} engine write_channel failed — \
                                 is point_id a configured virtual channel?",
                                label
                            );
                        } else {
                            tracing::debug!(
                                driver = %driver_id,
                                point_id,
                                error = %e,
                                "{} engine write_channel failed (repeat)",
                                label
                            );
                        }
                    }
                }
            }
            Err(e) => {
                counts.err += 1;
                if log.point(driver_id, *point_id, Some(e.to_string())) == PointChange::Failed {
                    tracing::warn!(
                        driver = %driver_id,
                        point_id,
                        error = %e,
                        "{} sync_cur failed",
                        label
                    );
                } else {
                    tracing::debug!(
                        driver = %driver_id,
                        point_id,
                        error = %e,
                        "{} sync_cur failed (repeat)",
                        label
                    );
                }
            }
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rest::EngineCmd;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Engine that answers writes with `Ok`, or holds them (hung) when
    /// `hang` is set. Returns the handle and a count of writes received.
    fn engine(hang: bool) -> (EngineHandle, Arc<AtomicUsize>) {
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<EngineCmd>(16);
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Some(cmd) = rx.recv().await {
                counter.fetch_add(1, Ordering::SeqCst);
                match cmd {
                    EngineCmd::WriteChannel { reply, .. } if !hang => {
                        let _ = reply.send(Ok(()));
                    }
                    other => held.push(other),
                }
            }
        });
        (EngineHandle::new(tx), seen)
    }

    /// Engine that rejects every write (not a timeout).
    fn engine_rejecting() -> EngineHandle {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<EngineCmd>(16);
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                if let EngineCmd::WriteChannel { reply, .. } = cmd {
                    let _ = reply.send(Err("channel 7 not found".into()));
                }
            }
        });
        EngineHandle::new(tx)
    }

    fn results() -> Vec<(DriverId, u32, Result<f64, DriverError>)> {
        vec![
            ("d".into(), 1, Ok(1.0)),
            ("d".into(), 2, Err(DriverError::Timeout("remote".into()))),
            ("d".into(), 3, Ok(3.0)),
            ("d".into(), 4, Ok(4.0)),
        ]
    }

    #[tokio::test]
    async fn deliver_tick_writes_every_value() {
        let (eh, seen) = engine(false);
        let counts = deliver_tick(&eh, "T", "t", &results(), &mut TickLog::default()).await;
        assert_eq!(
            counts,
            TickCounts {
                ok: 3,
                err: 1,
                write_err: 0,
                write_skipped: 0
            }
        );
        assert_eq!(seen.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn deliver_tick_skips_the_rest_after_an_engine_timeout() {
        let (eh, seen) = engine(true);
        let start = tokio::time::Instant::now();
        let counts = deliver_tick(&eh, "T", "t", &results(), &mut TickLog::default()).await;
        // One engine timeout for the tick, not one per value.
        assert_eq!(start.elapsed(), crate::rest::ENGINE_REPLY_TIMEOUT);
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts,
            TickCounts {
                ok: 3,
                err: 1,
                write_err: 1,
                write_skipped: 2
            }
        );
    }

    #[test]
    fn a_point_error_is_new_once_and_a_recovery_is_seen_once() {
        let mut log = TickLog::default();
        // Never failed: ok is not a recovery.
        assert_eq!(log.point("d", 1, None), PointChange::StillOk);
        assert_eq!(log.point("d", 1, Some("a".into())), PointChange::Failed);
        assert_eq!(
            log.point("d", 1, Some("a".into())),
            PointChange::StillFailing
        );
        // A different error is new again.
        assert_eq!(log.point("d", 1, Some("b".into())), PointChange::Failed);
        // Points and drivers are tracked separately.
        assert_eq!(log.point("d", 2, Some("b".into())), PointChange::Failed);
        assert_eq!(log.point("e", 1, Some("b".into())), PointChange::Failed);
        assert_eq!(log.point("d", 1, None), PointChange::Recovered);
        assert_eq!(log.point("d", 1, None), PointChange::StillOk);
    }

    fn counts(ok: usize, err: usize) -> TickCounts {
        TickCounts {
            ok,
            err,
            ..TickCounts::default()
        }
    }

    #[test]
    fn the_summary_repeats_as_a_reminder_while_points_fail() {
        let mut log = TickLog::default();
        let t0 = tokio::time::Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        log.point("d", 1, Some("down".into()));
        assert_eq!(log.summary(&counts(0, 1), at(0)), Summary::Changed);
        assert_eq!(log.summary(&counts(0, 1), at(5)), Summary::Same);
        assert_eq!(log.summary(&counts(0, 1), at(595)), Summary::Same);
        assert_eq!(
            log.summary(&counts(0, 1), at(600)),
            Summary::Reminder {
                points: 1,
                for_secs: 600
            }
        );
        assert_eq!(log.summary(&counts(0, 1), at(605)), Summary::Same);
        assert_eq!(
            log.summary(&counts(0, 1), at(1200)),
            Summary::Reminder {
                points: 1,
                for_secs: 1200
            }
        );
        // Recovered: the counts change, and no reminder follows.
        log.point("d", 1, None);
        assert_eq!(log.summary(&counts(1, 0), at(1205)), Summary::Changed);
        assert_eq!(log.summary(&counts(1, 0), at(3000)), Summary::Same);
        // A new failure starts its own 10 minutes.
        log.point("d", 1, Some("down".into()));
        assert_eq!(log.summary(&counts(0, 1), at(3005)), Summary::Changed);
        assert_eq!(log.summary(&counts(0, 1), at(3600)), Summary::Same);
        assert!(matches!(
            log.summary(&counts(0, 1), at(3605)),
            Summary::Reminder { for_secs: 600, .. }
        ));
    }

    /// Log output written while the returned guard lives (this thread).
    fn capture_logs() -> (
        Arc<std::sync::Mutex<Vec<u8>>>,
        tracing::subscriber::DefaultGuard,
    ) {
        #[derive(Clone)]
        struct Buf(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = Buf(buf.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .finish();
        (buf, tracing::subscriber::set_default(subscriber))
    }

    #[tokio::test]
    async fn a_repeated_point_error_is_logged_once_until_it_recovers() {
        let (eh, _seen) = engine(false);
        let (buf, guard) = capture_logs();
        let mut log = TickLog::default();
        let failing = vec![("d".into(), 7, Err(DriverError::CommFault("down".into())))];
        for _ in 0..3 {
            deliver_tick(&eh, "T", "t", &failing, &mut log).await;
        }
        let ok = vec![("d".into(), 7, Ok(2.0))];
        for _ in 0..2 {
            deliver_tick(&eh, "T", "t", &ok, &mut log).await;
        }
        drop(guard);
        let out = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
        let lines = |level: &str, msg: &str| {
            out.lines()
                .filter(|l| l.contains(level) && l.contains(msg))
                .count()
        };
        assert_eq!(lines("WARN", "T sync_cur failed"), 1, "{out}");
        assert_eq!(lines("DEBUG", "T sync_cur failed (repeat)"), 2, "{out}");
        assert_eq!(lines("INFO", "T point recovered"), 1, "{out}");
        // Successful reads only at debug.
        assert_eq!(lines("DEBUG", "T sync_cur -> write_channel"), 2, "{out}");
        assert_eq!(lines("INFO", "sync_cur -> write_channel"), 0, "{out}");
    }

    #[tokio::test]
    async fn a_repeated_write_channel_error_is_logged_once() {
        let eh = engine_rejecting();
        let (buf, guard) = capture_logs();
        let mut log = TickLog::default();
        let ok = vec![("d".into(), 7, Ok(2.0))];
        for _ in 0..3 {
            let counts = deliver_tick(&eh, "T", "t", &ok, &mut log).await;
            assert_eq!(counts.write_err, 1);
        }
        drop(guard);
        let out = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
        let warn = out
            .lines()
            .filter(|l| l.contains("WARN") && l.contains("T engine write_channel failed"))
            .count();
        assert_eq!(warn, 1, "{out}");
        assert_eq!(
            out.matches("T engine write_channel failed (repeat)")
                .count(),
            2,
            "{out}"
        );
    }

    #[test]
    fn the_reminder_comes_even_while_the_counts_keep_changing() {
        let mut log = TickLog::default();
        let t0 = tokio::time::Instant::now();
        log.point("d", 1, Some("down".into()));
        // Counts flap every tick (another point comes and goes).
        let mut reminders = 0;
        for i in 0..=240u64 {
            let c = counts((i % 2) as usize, 1);
            if let Summary::Reminder { .. } = log.summary(&c, t0 + Duration::from_secs(i * 5)) {
                reminders += 1;
            }
        }
        // 0..=1200 s: reminders at 600 s and 1200 s.
        assert_eq!(reminders, 2);
    }

    #[tokio::test]
    async fn a_changed_summary_with_failures_is_a_warning() {
        let (buf, guard) = capture_logs();
        let mut log = TickLog::default();
        log_tick_summary("T", &counts(1, 0), &mut log);
        log.point("d", 1, Some("down".into()));
        log_tick_summary("T", &counts(0, 1), &mut log);
        log_tick_summary("T", &counts(0, 1), &mut log);
        drop(guard);
        let out = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
        let at = |level: &str| {
            out.lines()
                .filter(|l| l.contains(level) && l.contains("T poll tick complete"))
                .count()
        };
        assert_eq!((at("INFO"), at("WARN"), at("DEBUG")), (1, 1, 1), "{out}");
    }
}
