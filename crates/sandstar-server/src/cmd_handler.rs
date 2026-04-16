//! Shared REST command handler.
//!
//! Processes [`EngineCmd`] variants against the engine. Used by both
//! the production server (`main.rs`) and the integration test harness.

use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use sandstar_engine::Engine;
use sandstar_hal::{HalDiagnostics, HalRead, HalWrite};
use tracing::{debug, warn};

use crate::config::ServerConfig;
use crate::history::HistoryStore;
use crate::reload;
use crate::rest::{ChannelValue, EngineCmd, WatchResponse};

/// Watch subscription state.
pub struct WatchState {
    pub display_name: String,
    pub channels: Vec<u32>,
    pub last_values: HashMap<u32, (f64, String)>,
    pub last_activity: Instant,
}

/// Maximum watch inactivity before expiration (1 hour).
const WATCH_LEASE_SECS: u64 = 3600;

/// Maximum number of concurrent watch subscriptions.
const MAX_WATCHES: usize = 64;

/// Maximum channels allowed in a single watch subscription.
/// Prevents a malicious client from subscribing to millions of IDs.
const MAX_CHANNELS_PER_WATCH: usize = 256;

/// Context needed to process engine commands.
pub struct CmdContext<'a> {
    pub config: &'a ServerConfig,
    pub start_time: Instant,
    pub watches: &'a mut HashMap<String, WatchState>,
    pub watch_counter: &'a mut u64,
    pub history_store: &'a HistoryStore,
}

/// Read a single channel through the engine pipeline.
pub fn read_channel_value<H: HalRead + HalWrite + HalDiagnostics>(
    engine: &mut Engine<H>,
    channel: u32,
) -> Result<ChannelValue, String> {
    match engine.channel_read(channel) {
        Ok(val) => Ok(ChannelValue {
            channel,
            status: val.status.as_str().into(),
            raw: val.raw,
            cur: val.cur,
        }),
        Err(e) => Err(e.to_string()),
    }
}

/// Expire watches that have been inactive longer than the lease.
pub fn expire_stale_watches(watches: &mut HashMap<String, WatchState>) {
    let now = Instant::now();
    watches.retain(|id, w| {
        let age = now.duration_since(w.last_activity).as_secs();
        if age > WATCH_LEASE_SECS {
            debug!(watch_id = %id, age_secs = age, "watch expired (inactive)");
            crate::metrics::metrics()
                .watch_active
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            false
        } else {
            true
        }
    });
}

/// Process a single REST API command against the engine.
///
/// Handles all `EngineCmd` variants. For `PollNow`, runs a synchronous
/// poll (suitable for tests; the production server routes PollNow
/// through `spawn_blocking` instead and should pass `unreachable!()`).
pub fn handle_engine_cmd<H: HalRead + HalWrite + HalDiagnostics>(
    cmd: EngineCmd,
    engine: &mut Engine<H>,
    ctx: &mut CmdContext<'_>,
) {
    match cmd {
        EngineCmd::Status { reply } => {
            let uptime = ctx.start_time.elapsed();
            let _ = reply.send(sandstar_ipc::types::StatusInfo {
                uptime_secs: uptime.as_secs(),
                channel_count: engine.channels.count(),
                poll_count: engine.polls.count(),
                table_count: engine.tables.count(),
                poll_interval_ms: ctx.config.poll_interval_ms,
            });
        }
        EngineCmd::ListChannels { reply } => {
            let channels = engine
                .channels
                .iter()
                .map(|(_, ch)| sandstar_ipc::types::ChannelInfo::from_engine(ch))
                .collect();
            let _ = reply.send(channels);
        }
        EngineCmd::ListPolls { reply } => {
            let polls = engine
                .polls
                .iter()
                .map(|(_, item)| sandstar_ipc::types::PollInfo {
                    channel: item.channel,
                    last_cur: item.last_value.cur,
                    last_status: item.last_value.status.as_str().into(),
                })
                .collect();
            let _ = reply.send(polls);
        }
        EngineCmd::ListTables { reply } => {
            let mut tables = Vec::new();
            for i in 0..1000 {
                if let Some(item) = engine.tables.get(i) {
                    tables.push(format!("[{}] {} ({})", i, item.tag, item.unit_type));
                }
            }
            let _ = reply.send(tables);
        }
        EngineCmd::ReadChannel { channel, reply } => {
            let result = read_channel_value(engine, channel);
            let _ = reply.send(result);
        }
        EngineCmd::WriteChannel {
            channel,
            value,
            level,
            who,
            duration,
            reply,
        } => {
            if ctx.config.read_only {
                let _ = reply.send(Err("server is in read-only validation mode".into()));
            } else {
                let result = engine
                    .channel_write_level(channel, level, value, &who, duration)
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
        }
        EngineCmd::GetWriteLevels { channel, reply } => {
            use sandstar_ipc::types::WriteLevelInfo;
            let result = match engine.get_write_levels(channel) {
                Ok(pa_opt) => {
                    let levels: Vec<WriteLevelInfo> = (0..17)
                        .map(|i| {
                            let (val, who) = match &pa_opt {
                                Some(pa) => {
                                    let wl = &pa.levels()[i];
                                    (wl.value, wl.who.clone())
                                }
                                None => (None, String::new()),
                            };
                            WriteLevelInfo {
                                level: (i + 1) as u8,
                                level_dis: format!("Level {}", i + 1),
                                val,
                                who,
                            }
                        })
                        .collect();
                    Ok(levels)
                }
                Err(e) => Err(e.to_string()),
            };
            let _ = reply.send(result);
        }
        EngineCmd::PollNow { reply } => {
            // Synchronous poll — used by tests. Production server routes
            // PollNow through spawn_blocking in the main select! loop.
            let notifications = engine.poll_update();
            let _ = reply.send(Ok(format!(
                "poll complete: {} notifications",
                notifications.len()
            )));
        }
        EngineCmd::ReloadConfig { reply } => {
            let result = match &ctx.config.config_dir {
                Some(dir) => reload::reload_config(engine, dir)
                    .map(|s| s.to_string())
                    .map_err(|e| format!("reload failed: {}", e)),
                None => Err("no config directory — running in demo mode".into()),
            };
            let _ = reply.send(result);
        }
        EngineCmd::AboutInfo { reply } => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let boot = now.saturating_sub(ctx.start_time.elapsed().as_secs());
            let _ = reply.send((boot, now));
        }

        // ── Watch commands ──────────────────────────────────
        EngineCmd::WatchSub {
            watch_id,
            display_name,
            channels,
            reply,
        } => {
            // Expire stale watches on every WatchSub
            expire_stale_watches(ctx.watches);

            // Guard: reject subscriptions with too many channels
            if channels.len() > MAX_CHANNELS_PER_WATCH {
                warn!(
                    count = channels.len(),
                    max = MAX_CHANNELS_PER_WATCH,
                    "watch subscription rejected: too many channels"
                );
                let _ = reply.send(Err(format!(
                    "too many channels ({}, max {})",
                    channels.len(),
                    MAX_CHANNELS_PER_WATCH
                )));
                return;
            }

            // Filter out channel IDs that don't exist in the engine
            let channels: Vec<u32> = channels
                .into_iter()
                .filter(|&ch_id| engine.channels.get(ch_id).is_some())
                .collect();

            let wid = match watch_id {
                Some(id) if ctx.watches.contains_key(&id) => id,
                _ if ctx.watches.len() >= MAX_WATCHES => {
                    let _ = reply.send(Err(format!("watch limit reached (max {})", MAX_WATCHES)));
                    return;
                }
                _ => {
                    *ctx.watch_counter += 1;
                    let id = format!("w-{:x}", *ctx.watch_counter);
                    ctx.watches.insert(
                        id.clone(),
                        WatchState {
                            display_name: display_name.unwrap_or_else(|| id.clone()),
                            channels: Vec::new(),
                            last_values: HashMap::new(),
                            last_activity: Instant::now(),
                        },
                    );
                    debug!(watch_id = %id, "created new watch");
                    crate::metrics::metrics()
                        .watch_active
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    id
                }
            };

            if let Some(watch) = ctx.watches.get_mut(&wid) {
                watch.last_activity = Instant::now();
                let mut rows = Vec::new();
                for ch_id in channels {
                    if !watch.channels.contains(&ch_id) {
                        watch.channels.push(ch_id);
                    }
                    if let Ok(val) = read_channel_value(engine, ch_id) {
                        watch
                            .last_values
                            .insert(ch_id, (val.cur, val.status.clone()));
                        rows.push(val);
                    }
                }

                let _ = reply.send(Ok(WatchResponse {
                    watch_id: wid,
                    lease: 3600,
                    rows,
                }));
            } else {
                let _ = reply.send(Err(format!("watch {} not found", wid)));
            }
        }
        EngineCmd::WatchUnsub {
            watch_id,
            close,
            channels,
            reply,
        } => {
            if close {
                if ctx.watches.remove(&watch_id).is_some() {
                    debug!(watch_id = %watch_id, "watch closed");
                    crate::metrics::metrics()
                        .watch_active
                        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    let _ = reply.send(Ok(()));
                } else {
                    let _ = reply.send(Err(format!("watch {} not found", watch_id)));
                }
            } else if let Some(watch) = ctx.watches.get_mut(&watch_id) {
                watch.channels.retain(|id| !channels.contains(id));
                for ch_id in &channels {
                    watch.last_values.remove(ch_id);
                }
                let _ = reply.send(Ok(()));
            } else {
                let _ = reply.send(Err(format!("watch {} not found", watch_id)));
            }
        }
        EngineCmd::WatchPoll {
            watch_id,
            refresh,
            reply,
        } => {
            if let Some(watch) = ctx.watches.get_mut(&watch_id) {
                watch.last_activity = Instant::now();
                let mut rows = Vec::new();
                for &ch_id in &watch.channels.clone() {
                    if let Ok(val) = read_channel_value(engine, ch_id) {
                        let changed = if refresh {
                            true
                        } else {
                            match watch.last_values.get(&ch_id) {
                                None => true,
                                Some((prev_cur, prev_status)) => {
                                    (val.cur - prev_cur).abs() > f64::EPSILON
                                        || val.status != *prev_status
                                }
                            }
                        };
                        if changed {
                            watch
                                .last_values
                                .insert(ch_id, (val.cur, val.status.clone()));
                            rows.push(val);
                        }
                    }
                }
                let _ = reply.send(Ok(WatchResponse {
                    watch_id,
                    lease: 3600,
                    rows,
                }));
            } else {
                let _ = reply.send(Err(format!("watch {} not found", watch_id)));
            }
        }
        EngineCmd::GetHistory {
            channel,
            since_unix,
            limit,
            reply,
        } => {
            let points = ctx.history_store.query(channel, since_unix, limit);
            let _ = reply.send(points);
        }
        EngineCmd::Diagnostics { reply } => {
            let uptime = ctx.start_time.elapsed();
            let m = crate::metrics::metrics();

            let channels_fault = engine
                .channels
                .iter()
                .filter(|(_, ch)| ch.value.status == sandstar_engine::EngineStatus::Fault)
                .count();
            let channels_down = engine
                .channels
                .iter()
                .filter(|(_, ch)| ch.value.status == sandstar_engine::EngineStatus::Down)
                .count();
            let i2c_backoff_active = engine.i2c_backoff.len();

            let last_us = m
                .poll_duration_us_last
                .load(std::sync::atomic::Ordering::Relaxed);
            let max_us = m
                .poll_duration_us_max
                .load(std::sync::atomic::Ordering::Relaxed);

            let _ = reply.send(sandstar_ipc::types::DiagnosticsInfo {
                uptime_secs: uptime.as_secs(),
                poll_count: m.poll_count.load(std::sync::atomic::Ordering::Relaxed),
                last_poll_duration_ms: last_us / 1000,
                max_poll_duration_ms: max_us / 1000,
                poll_overrun_count: m
                    .poll_overrun_count
                    .load(std::sync::atomic::Ordering::Relaxed),
                poll_interval_ms: ctx.config.poll_interval_ms,
                channels_total: engine.channels.count(),
                channels_fault,
                channels_down,
                i2c_backoff_active,
            });
        }
        EngineCmd::Shutdown => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandstar_engine::channel::{Channel, ChannelDirection, ChannelType};
    use sandstar_engine::value::ValueConv;
    use sandstar_engine::Engine;
    use sandstar_hal::mock::MockHal;
    use tokio::sync::oneshot;

    fn make_engine() -> Engine<MockHal> {
        let hal = MockHal::new();
        let mut engine = Engine::new(hal);
        for id in [1113, 1200, 612] {
            let ch = Channel::new(
                id,
                ChannelType::Analog,
                ChannelDirection::In,
                0,
                id,
                false,
                ValueConv::default(),
                &format!("ch{}", id),
            );
            let _ = engine.channels.add(ch);
            let _ = engine.polls.add(id);
        }
        engine
    }

    fn make_config() -> ServerConfig {
        ServerConfig {
            socket_path: String::new(),
            poll_interval_ms: 1000,
            config_dir: None,
            read_only: false,
            auth_store: crate::auth::AuthStore::new(),
            auth_token: None,
            rate_limit: 0,
        }
    }

    fn make_ctx<'a>(
        config: &'a ServerConfig,
        watches: &'a mut HashMap<String, WatchState>,
        counter: &'a mut u64,
        history: &'a HistoryStore,
    ) -> CmdContext<'a> {
        CmdContext {
            config,
            start_time: Instant::now(),
            watches,
            watch_counter: counter,
            history_store: history,
        }
    }

    // Skipped on Windows: Instant::now() on Windows is close to boot and
    // checked_sub(large) returns None. The expire_stale_watches logic itself
    // is platform-independent; this test just can't construct the old Instant.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn test_expire_stale_watches() {
        let mut watches = HashMap::new();
        watches.insert(
            "w-fresh".into(),
            WatchState {
                display_name: "fresh".into(),
                channels: vec![],
                last_values: HashMap::new(),
                last_activity: Instant::now(),
            },
        );

        expire_stale_watches(&mut watches);
        assert_eq!(watches.len(), 1, "fresh watch should not expire");

        let old_instant = Instant::now()
            .checked_sub(std::time::Duration::from_secs(WATCH_LEASE_SECS + 100))
            .unwrap();
        watches.insert(
            "w-stale".into(),
            WatchState {
                display_name: "stale".into(),
                channels: vec![],
                last_values: HashMap::new(),
                last_activity: old_instant,
            },
        );
        assert_eq!(watches.len(), 2);

        expire_stale_watches(&mut watches);
        assert_eq!(watches.len(), 1, "stale watch should be expired");
        assert!(watches.contains_key("w-fresh"));
    }

    #[test]
    fn test_max_watches_limit() {
        let mut engine = make_engine();
        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;

        for i in 0..MAX_WATCHES {
            let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);
            let (reply, rx) = oneshot::channel();
            handle_engine_cmd(
                EngineCmd::WatchSub {
                    watch_id: None,
                    display_name: Some(format!("w{}", i)),
                    channels: vec![1113],
                    reply,
                },
                &mut engine,
                &mut ctx,
            );
            let result = rx.blocking_recv().unwrap();
            assert!(result.is_ok(), "watch {} should succeed", i);
        }
        assert_eq!(watches.len(), MAX_WATCHES);

        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);
        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::WatchSub {
                watch_id: None,
                display_name: Some("overflow".into()),
                channels: vec![1113],
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let result = rx.blocking_recv().unwrap();
        assert!(result.is_err(), "65th watch should be rejected");
        assert!(result.unwrap_err().contains("watch limit"));
    }

    #[test]
    fn test_max_channels_per_watch() {
        let mut engine = make_engine();
        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;

        let too_many: Vec<u32> = (0..=MAX_CHANNELS_PER_WATCH as u32).collect();
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);
        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::WatchSub {
                watch_id: None,
                display_name: None,
                channels: too_many,
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let result = rx.blocking_recv().unwrap();
        assert!(result.is_err(), "257 channels should be rejected");
        assert!(result.unwrap_err().contains("too many channels"));
    }

    #[test]
    fn test_watch_lifecycle() {
        let mut engine = make_engine();
        engine.hal.set_analog(0, 1113, Ok(100.0));
        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;

        // Subscribe
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);
        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::WatchSub {
                watch_id: None,
                display_name: Some("test".into()),
                channels: vec![1113],
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let resp = rx.blocking_recv().unwrap().unwrap();
        let wid = resp.watch_id.clone();
        assert_eq!(resp.rows.len(), 1);

        // Poll with no changes — empty rows
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);
        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::WatchPoll {
                watch_id: wid.clone(),
                refresh: false,
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let resp = rx.blocking_recv().unwrap().unwrap();
        assert_eq!(resp.rows.len(), 0, "no changes should yield empty rows");

        // Change value and poll again
        engine.hal.set_analog(0, 1113, Ok(200.0));
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);
        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::WatchPoll {
                watch_id: wid.clone(),
                refresh: false,
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let resp = rx.blocking_recv().unwrap().unwrap();
        assert!(resp.rows.len() >= 1, "changed value should appear");

        // Unsubscribe (close)
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);
        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::WatchUnsub {
                watch_id: wid.clone(),
                close: true,
                channels: vec![],
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let result = rx.blocking_recv().unwrap();
        assert!(result.is_ok());
        assert!(!watches.contains_key(&wid));
    }

    #[test]
    fn test_read_only_write_rejection() {
        let mut engine = make_engine();
        let mut config = make_config();
        config.read_only = true;
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::WriteChannel {
                channel: 1113,
                value: Some(72.0),
                level: 8,
                who: "test".into(),
                duration: 0.0,
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let result = rx.blocking_recv().unwrap();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("read-only"));
    }

    #[test]
    fn test_status_info() {
        let mut engine = make_engine();
        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(EngineCmd::Status { reply }, &mut engine, &mut ctx);
        let info = rx.blocking_recv().unwrap();
        assert_eq!(info.channel_count, 3);
        assert_eq!(info.poll_count, 3);
        assert_eq!(info.poll_interval_ms, 1000);
    }

    #[test]
    fn test_read_nonexistent_channel() {
        let mut engine = make_engine();
        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::ReadChannel {
                channel: 9999,
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let result = rx.blocking_recv().unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn test_watch_poll_refresh() {
        let mut engine = make_engine();
        engine.hal.set_analog(0, 1113, Ok(100.0));
        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;

        // Subscribe
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);
        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::WatchSub {
                watch_id: None,
                display_name: None,
                channels: vec![1113],
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let resp = rx.blocking_recv().unwrap().unwrap();
        let wid = resp.watch_id.clone();

        // Normal poll — no changes, empty rows
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);
        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::WatchPoll {
                watch_id: wid.clone(),
                refresh: false,
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let resp = rx.blocking_recv().unwrap().unwrap();
        assert_eq!(resp.rows.len(), 0);

        // Refresh poll — returns all values even if unchanged
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);
        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::WatchPoll {
                watch_id: wid.clone(),
                refresh: true,
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let resp = rx.blocking_recv().unwrap().unwrap();
        assert_eq!(resp.rows.len(), 1, "refresh should return all values");
    }

    #[test]
    fn test_list_channels() {
        let mut engine = make_engine();
        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(EngineCmd::ListChannels { reply }, &mut engine, &mut ctx);
        let channels = rx.blocking_recv().unwrap();
        assert_eq!(channels.len(), 3);
        let ids: Vec<u32> = channels.iter().map(|c| c.id).collect();
        assert!(ids.contains(&1113));
        assert!(ids.contains(&1200));
        assert!(ids.contains(&612));
    }

    #[test]
    fn test_list_polls() {
        let mut engine = make_engine();
        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(EngineCmd::ListPolls { reply }, &mut engine, &mut ctx);
        let polls = rx.blocking_recv().unwrap();
        assert_eq!(polls.len(), 3);
        let poll_channels: Vec<u32> = polls.iter().map(|p| p.channel).collect();
        assert!(poll_channels.contains(&1113));
        assert!(poll_channels.contains(&1200));
        assert!(poll_channels.contains(&612));
    }

    #[test]
    fn test_list_tables() {
        let mut engine = make_engine();
        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(EngineCmd::ListTables { reply }, &mut engine, &mut ctx);
        let tables = rx.blocking_recv().unwrap();
        // Demo engine has no tables loaded
        assert!(tables.is_empty());
    }

    #[test]
    fn test_write_channel_success() {
        let mut engine = make_engine();
        // Add a virtual output channel for writing (VirtualAnalog supports writes without HAL)
        let ch_out = Channel::new(
            2001,
            ChannelType::VirtualAnalog,
            ChannelDirection::Out,
            0,
            2001,
            false,
            ValueConv::default(),
            "vao1",
        );
        let _ = engine.channels.add(ch_out);

        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        // Write to output channel 2001 at level 8
        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::WriteChannel {
                channel: 2001,
                value: Some(72.0),
                level: 8,
                who: "test-user".into(),
                duration: 0.0,
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let result = rx.blocking_recv().unwrap();
        assert!(
            result.is_ok(),
            "write should succeed on non-read-only server: {:?}",
            result.err()
        );

        // Verify the write stuck by reading write levels
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);
        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::GetWriteLevels {
                channel: 2001,
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let levels = rx.blocking_recv().unwrap().unwrap();
        // Level 8 is index 7
        assert_eq!(levels[7].val, Some(72.0));
        assert_eq!(levels[7].who, "test-user");
    }

    #[test]
    fn test_write_channel_nonexistent() {
        let mut engine = make_engine();
        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::WriteChannel {
                channel: 9999,
                value: Some(72.0),
                level: 8,
                who: "test".into(),
                duration: 0.0,
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let result = rx.blocking_recv().unwrap();
        assert!(result.is_err(), "write to nonexistent channel should fail");
    }

    #[test]
    fn test_get_write_levels_no_writes() {
        let mut engine = make_engine();
        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::GetWriteLevels {
                channel: 1113,
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let levels = rx.blocking_recv().unwrap().unwrap();
        assert_eq!(levels.len(), 17);
        // All levels should be null/empty when no writes have been made
        for level in &levels {
            assert_eq!(level.val, None);
            assert!(level.who.is_empty());
        }
    }

    #[test]
    fn test_poll_now() {
        let mut engine = make_engine();
        engine.hal.set_analog(0, 1113, Ok(55.0));
        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(EngineCmd::PollNow { reply }, &mut engine, &mut ctx);
        let result = rx.blocking_recv().unwrap();
        assert!(result.is_ok());
        let msg = result.unwrap();
        assert!(
            msg.contains("poll complete"),
            "should report poll complete: {}",
            msg
        );
    }

    #[test]
    fn test_reload_config_demo_mode() {
        let mut engine = make_engine();
        let config = make_config(); // config_dir is None (demo mode)
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(EngineCmd::ReloadConfig { reply }, &mut engine, &mut ctx);
        let result = rx.blocking_recv().unwrap();
        assert!(result.is_err(), "reload should fail in demo mode");
        assert!(result.unwrap_err().contains("demo mode"));
    }

    #[test]
    fn test_about_info() {
        let mut engine = make_engine();
        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(EngineCmd::AboutInfo { reply }, &mut engine, &mut ctx);
        let (boot, now) = rx.blocking_recv().unwrap();
        // boot should be <= now (boot is now - uptime)
        assert!(
            boot <= now,
            "boot time {} should be <= current time {}",
            boot,
            now
        );
        // Both should be nonzero (we're past 1970)
        assert!(now > 0, "current time should be nonzero");
    }

    #[test]
    fn test_get_history() {
        let mut engine = make_engine();
        let config = make_config();
        let mut history = HistoryStore::new(100);
        // Record some history
        use crate::history::HistoryPoint;
        use sandstar_engine::EngineStatus;
        for i in 0..5 {
            history.record(
                1113,
                HistoryPoint {
                    ts: 1000 + i,
                    cur: 72.0 + i as f64,
                    raw: 2048.0,
                    status: EngineStatus::Ok,
                },
            );
        }
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(
            EngineCmd::GetHistory {
                channel: 1113,
                since_unix: 0,
                limit: 100,
                reply,
            },
            &mut engine,
            &mut ctx,
        );
        let points = rx.blocking_recv().unwrap();
        assert_eq!(points.len(), 5);
        assert_eq!(points[0].ts, 1000);
        assert_eq!(points[4].cur, 76.0);
    }

    #[test]
    fn test_diagnostics() {
        let mut engine = make_engine();
        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(EngineCmd::Diagnostics { reply }, &mut engine, &mut ctx);
        let info = rx.blocking_recv().unwrap();
        assert_eq!(info.channels_total, 3);
        assert_eq!(info.channels_fault, 0);
        assert_eq!(info.channels_down, 0);
        assert_eq!(info.i2c_backoff_active, 0);
        assert_eq!(info.poll_interval_ms, 1000);
    }

    #[test]
    fn test_diagnostics_with_faults() {
        let mut engine = make_engine();
        // Set one channel to Down status
        if let Some(ch) = engine.channels.get_mut(612) {
            ch.value.status = sandstar_engine::EngineStatus::Down;
        }
        // Set another to Fault status
        if let Some(ch) = engine.channels.get_mut(1200) {
            ch.value.status = sandstar_engine::EngineStatus::Fault;
        }

        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(EngineCmd::Diagnostics { reply }, &mut engine, &mut ctx);
        let info = rx.blocking_recv().unwrap();
        assert_eq!(info.channels_total, 3);
        assert_eq!(info.channels_fault, 1, "one channel should be Fault");
        assert_eq!(info.channels_down, 1, "one channel should be Down");
    }

    #[test]
    fn test_diagnostics_with_i2c_backoff() {
        let mut engine = make_engine();
        // Insert a fake I2C backoff entry
        engine.i2c_backoff.insert(
            (2, 0x40),
            sandstar_engine::engine::I2cBackoffState {
                cooldown: 60,
                consecutive_failures: 1,
            },
        );

        let config = make_config();
        let history = HistoryStore::new(100);
        let mut watches = HashMap::new();
        let mut counter = 0u64;
        let mut ctx = make_ctx(&config, &mut watches, &mut counter, &history);

        let (reply, rx) = oneshot::channel();
        handle_engine_cmd(EngineCmd::Diagnostics { reply }, &mut engine, &mut ctx);
        let info = rx.blocking_recv().unwrap();
        assert_eq!(
            info.i2c_backoff_active, 1,
            "should have one I2C sensor in backoff"
        );
    }
}
