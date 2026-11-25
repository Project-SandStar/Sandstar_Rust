//! Command dispatch: run an IPC command on the engine (main loop side).
//!
//! The connection itself (framing, deadlines) lives in `crate::ipc` (R-28).

use std::time::Instant;

use sandstar_engine::{Engine, EngineValue};
use sandstar_hal::{HalDiagnostics, HalRead, HalWrite};
use sandstar_ipc::types::*;
use tracing::{debug, info, warn};

use crate::config::ServerConfig;
use crate::history::HistoryStore;
use crate::reload;

/// What the main loop must do after an IPC command.
pub enum ConnectionResult {
    /// Normal command processed, continue accepting connections.
    Continue,
    /// Shutdown requested by client.
    Shutdown,
    /// PollNow requested — main loop should trigger async poll via spawn_blocking.
    PollNow,
}

/// Run one IPC command on the engine: what the main loop must do next, and
/// the reply for the client.
pub fn run<H: HalRead + HalWrite + HalDiagnostics>(
    cmd: EngineCommand,
    engine: &mut Engine<H>,
    config: &ServerConfig,
    start_time: Instant,
    history_store: &HistoryStore,
) -> (ConnectionResult, EngineResponse) {
    debug!(?cmd, "received command");
    crate::metrics::metrics()
        .ipc_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let result = match &cmd {
        EngineCommand::Shutdown => ConnectionResult::Shutdown,
        EngineCommand::PollNow => ConnectionResult::PollNow,
        _ => ConnectionResult::Continue,
    };
    (
        result,
        execute(cmd, engine, config, start_time, history_store),
    )
}

/// Execute a command against the engine and produce a response.
fn execute<H: HalRead + HalWrite + HalDiagnostics>(
    cmd: EngineCommand,
    engine: &mut Engine<H>,
    config: &ServerConfig,
    start_time: Instant,
    history_store: &HistoryStore,
) -> EngineResponse {
    match cmd {
        EngineCommand::Shutdown => {
            info!("shutdown requested via IPC");
            EngineResponse::Ok
        }

        EngineCommand::ReadChannel { channel } => match engine.channel_read(channel) {
            Ok(val) => EngineResponse::Value {
                channel,
                status: val.status.as_str().into(),
                raw: val.raw,
                cur: val.cur,
            },
            Err(e) => EngineResponse::Error(e.to_string()),
        },

        EngineCommand::WriteChannel {
            channel,
            value,
            level,
        } => {
            if config.read_only {
                warn!(channel, value, "IPC write rejected (read-only mode)");
                EngineResponse::Error("server is in read-only validation mode".into())
            } else {
                match engine.channel_write_level(channel, level, Some(value), "", 0.0) {
                    Ok(_) => EngineResponse::Ok,
                    Err(e) => EngineResponse::Error(e.to_string()),
                }
            }
        }

        EngineCommand::RelinquishLevel { channel, level } => {
            if config.read_only {
                warn!(channel, level, "IPC relinquish rejected (read-only mode)");
                EngineResponse::Error("server is in read-only validation mode".into())
            } else {
                match engine.channel_write_level(channel, level, None, "", 0.0) {
                    Ok(_) => EngineResponse::Ok,
                    Err(e) => EngineResponse::Error(e.to_string()),
                }
            }
        }

        EngineCommand::GetWriteLevels { channel } => match engine.get_write_levels(channel) {
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
                EngineResponse::WriteLevels(levels)
            }
            Err(e) => EngineResponse::Error(e.to_string()),
        },

        EngineCommand::ConvertValue { channel, raw } => {
            let mut ev = EngineValue::default();
            ev.set_raw(raw);

            match engine.channel_convert(channel, &mut ev) {
                Ok(()) => EngineResponse::Value {
                    channel,
                    status: ev.status.as_str().into(),
                    raw: ev.raw,
                    cur: ev.cur,
                },
                Err(e) => EngineResponse::Error(e.to_string()),
            }
        }

        EngineCommand::ListChannels => {
            let channels: Vec<ChannelInfo> = engine
                .channels
                .iter()
                .map(|(_, ch)| ChannelInfo::from_engine(ch))
                .collect();
            EngineResponse::Channels(channels)
        }

        EngineCommand::ListTables => {
            let mut tables = Vec::new();
            for i in 0..1000 {
                if let Some(item) = engine.tables.get(i) {
                    tables.push(format!("[{}] {} ({})", i, item.tag, item.unit_type));
                }
            }
            EngineResponse::Tables(tables)
        }

        EngineCommand::ListPolls => {
            let polls: Vec<PollInfo> = engine
                .polls
                .iter()
                .map(|(_, item)| PollInfo {
                    channel: item.channel,
                    last_cur: item.last_value.cur,
                    last_status: item.last_value.status.as_str().into(),
                })
                .collect();
            EngineResponse::Polls(polls)
        }

        EngineCommand::Status => {
            let uptime = start_time.elapsed();
            EngineResponse::Status(StatusInfo {
                uptime_secs: uptime.as_secs(),
                channel_count: engine.channels.count(),
                poll_count: engine.polls.count(),
                table_count: engine.tables.count(),
                poll_interval_ms: config.poll_interval_ms,
            })
        }

        EngineCommand::PollNow => {
            // Actual poll runs async in main loop via spawn_blocking.
            // We just acknowledge the request here.
            info!("manual poll requested via IPC (will run async)");
            EngineResponse::Ok
        }

        EngineCommand::ReloadConfig => match &config.config_dir {
            Some(config_dir) => match reload::reload_config(engine, config_dir) {
                Ok(summary) => {
                    info!(%summary, "config reloaded via IPC");
                    EngineResponse::Ok
                }
                Err(e) => EngineResponse::Error(format!("reload failed: {}", e)),
            },
            None => EngineResponse::Error("no config directory — running in demo mode".to_string()),
        },

        EngineCommand::GetHistory {
            channel,
            since_secs,
            limit,
        } => {
            let points = history_store.query(channel, since_secs, limit);
            let entries = points
                .into_iter()
                .map(|p| HistoryEntry {
                    ts: p.ts,
                    cur: p.cur,
                    raw: p.raw,
                    status: p.status.as_str().into(),
                })
                .collect();
            EngineResponse::History(entries)
        }

        EngineCommand::Diagnostics => {
            let uptime = start_time.elapsed();
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

            EngineResponse::Diagnostics(DiagnosticsInfo {
                uptime_secs: uptime.as_secs(),
                poll_count: m.poll_count.load(std::sync::atomic::Ordering::Relaxed),
                last_poll_duration_ms: last_us / 1000,
                max_poll_duration_ms: max_us / 1000,
                poll_overrun_count: m
                    .poll_overrun_count
                    .load(std::sync::atomic::Ordering::Relaxed),
                poll_interval_ms: config.poll_interval_ms,
                channels_total: engine.channels.count(),
                channels_fault,
                channels_down,
                i2c_backoff_active,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryPoint;
    use sandstar_engine::channel::{Channel, ChannelDirection, ChannelType};
    use sandstar_engine::value::ValueConv;
    use sandstar_engine::EngineStatus;
    use sandstar_hal::mock::MockHal;

    const IN_CH: u32 = 100;
    const OUT_CH: u32 = 200;
    const MISSING_CH: u32 = 999;

    fn make_engine() -> Engine<MockHal> {
        let mut engine = Engine::new(MockHal::new());
        // The output is a virtual channel: it takes writes without a HAL.
        for (id, ty, dir) in [
            (IN_CH, ChannelType::Analog, ChannelDirection::In),
            (OUT_CH, ChannelType::VirtualAnalog, ChannelDirection::Out),
        ] {
            let ch = Channel::new(
                id,
                ty,
                dir,
                0,
                id,
                false,
                ValueConv::default(),
                &format!("ch{}", id),
            );
            engine.channels.add(ch).unwrap();
            engine.channels.get_mut(id).unwrap().enabled = true;
        }
        engine.polls.add(IN_CH).unwrap();
        engine
    }

    fn make_config(read_only: bool) -> ServerConfig {
        ServerConfig {
            socket_path: String::new(),
            poll_interval_ms: 1000,
            config_dir: None,
            read_only,
            auth_store: crate::auth::AuthStore::new(),
            auth_token: None,
            rate_limit: 0,
        }
    }

    fn send(
        cmd: EngineCommand,
        engine: &mut Engine<MockHal>,
        config: &ServerConfig,
    ) -> (ConnectionResult, EngineResponse) {
        let history = HistoryStore::new(16);
        run(cmd, engine, config, Instant::now(), &history)
    }

    /// Level 8 of the priority array, or `None` if nothing is written there.
    fn level8(engine: &Engine<MockHal>, channel: u32) -> Option<f64> {
        engine
            .get_write_levels(channel)
            .unwrap()
            .and_then(|pa| pa.levels()[7].value)
    }

    fn error_text(resp: &EngineResponse) -> &str {
        match resp {
            EngineResponse::Error(msg) => msg,
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn shutdown_and_poll_now_tell_the_main_loop_what_to_do() {
        let mut engine = make_engine();
        let config = make_config(false);

        let (result, resp) = send(EngineCommand::Shutdown, &mut engine, &config);
        assert!(matches!(result, ConnectionResult::Shutdown));
        assert!(matches!(resp, EngineResponse::Ok));

        let (result, resp) = send(EngineCommand::PollNow, &mut engine, &config);
        assert!(matches!(result, ConnectionResult::PollNow));
        assert!(matches!(resp, EngineResponse::Ok));
    }

    #[test]
    fn ordinary_commands_keep_the_main_loop_running() {
        let mut engine = make_engine();
        let config = make_config(false);
        for cmd in [
            EngineCommand::Status,
            EngineCommand::ListChannels,
            EngineCommand::ReloadConfig,
            EngineCommand::ReadChannel {
                channel: MISSING_CH,
            },
        ] {
            let (result, _) = send(cmd, &mut engine, &config);
            assert!(matches!(result, ConnectionResult::Continue));
        }
    }

    #[test]
    fn write_then_relinquish_changes_the_priority_array() {
        let mut engine = make_engine();
        let config = make_config(false);

        let (_, resp) = send(
            EngineCommand::WriteChannel {
                channel: OUT_CH,
                value: 42.5,
                level: 8,
            },
            &mut engine,
            &config,
        );
        assert!(matches!(resp, EngineResponse::Ok), "{:?}", resp);
        assert_eq!(level8(&engine, OUT_CH), Some(42.5));

        let (_, resp) = send(
            EngineCommand::RelinquishLevel {
                channel: OUT_CH,
                level: 8,
            },
            &mut engine,
            &config,
        );
        assert!(matches!(resp, EngineResponse::Ok), "{:?}", resp);
        assert_eq!(level8(&engine, OUT_CH), None);
    }

    #[test]
    fn read_only_mode_rejects_write_and_leaves_the_engine_untouched() {
        let mut engine = make_engine();
        let config = make_config(true);

        let (result, resp) = send(
            EngineCommand::WriteChannel {
                channel: OUT_CH,
                value: 1.0,
                level: 8,
            },
            &mut engine,
            &config,
        );
        assert!(matches!(result, ConnectionResult::Continue));
        assert!(error_text(&resp).contains("read-only"));
        assert!(engine.get_write_levels(OUT_CH).unwrap().is_none());
    }

    #[test]
    fn read_only_mode_rejects_relinquish_and_keeps_the_written_level() {
        let mut engine = make_engine();
        engine
            .channel_write_level(OUT_CH, 8, Some(7.0), "test", 0.0)
            .unwrap();
        let config = make_config(true);

        let (_, resp) = send(
            EngineCommand::RelinquishLevel {
                channel: OUT_CH,
                level: 8,
            },
            &mut engine,
            &config,
        );
        assert!(error_text(&resp).contains("read-only"));
        assert_eq!(level8(&engine, OUT_CH), Some(7.0));
    }

    #[test]
    fn non_finite_write_is_rejected_and_not_stored() {
        let config = make_config(false);
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut engine = make_engine();
            let (_, resp) = send(
                EngineCommand::WriteChannel {
                    channel: OUT_CH,
                    value: bad,
                    level: 8,
                },
                &mut engine,
                &config,
            );
            error_text(&resp);
            assert_eq!(level8(&engine, OUT_CH), None, "value {bad} was stored");
        }
    }

    #[test]
    fn write_level_outside_1_to_17_is_rejected() {
        let config = make_config(false);
        for level in [0u8, 18, 255] {
            let mut engine = make_engine();
            let (_, resp) = send(
                EngineCommand::WriteChannel {
                    channel: OUT_CH,
                    value: 1.0,
                    level,
                },
                &mut engine,
                &config,
            );
            error_text(&resp);
            assert!(engine.get_write_levels(OUT_CH).unwrap().is_none());

            let (_, resp) = send(
                EngineCommand::RelinquishLevel {
                    channel: OUT_CH,
                    level,
                },
                &mut engine,
                &config,
            );
            error_text(&resp);
        }
    }

    #[test]
    fn missing_channel_is_an_error_for_every_channel_command() {
        let mut engine = make_engine();
        let config = make_config(false);
        let cmds = [
            EngineCommand::ReadChannel {
                channel: MISSING_CH,
            },
            EngineCommand::WriteChannel {
                channel: MISSING_CH,
                value: 1.0,
                level: 8,
            },
            EngineCommand::RelinquishLevel {
                channel: MISSING_CH,
                level: 8,
            },
            EngineCommand::GetWriteLevels {
                channel: MISSING_CH,
            },
            EngineCommand::ConvertValue {
                channel: MISSING_CH,
                raw: 1.0,
            },
        ];
        for cmd in cmds {
            let (_, resp) = send(cmd, &mut engine, &config);
            error_text(&resp);
        }
        assert_eq!(engine.channels.count(), 2);
    }

    #[test]
    fn write_levels_lists_17_rows_before_and_after_a_write() {
        let mut engine = make_engine();
        let config = make_config(false);

        let (_, resp) = send(
            EngineCommand::GetWriteLevels { channel: OUT_CH },
            &mut engine,
            &config,
        );
        let EngineResponse::WriteLevels(levels) = resp else {
            panic!("expected WriteLevels");
        };
        assert_eq!(levels.len(), 17);
        assert!(levels.iter().all(|l| l.val.is_none()));
        assert_eq!(levels[0].level, 1);
        assert_eq!(levels[16].level, 17);

        engine
            .channel_write_level(OUT_CH, 3, Some(9.0), "op", 0.0)
            .unwrap();
        let (_, resp) = send(
            EngineCommand::GetWriteLevels { channel: OUT_CH },
            &mut engine,
            &config,
        );
        let EngineResponse::WriteLevels(levels) = resp else {
            panic!("expected WriteLevels");
        };
        assert_eq!(levels[2].val, Some(9.0));
        assert_eq!(levels[2].who, "op");
        assert_eq!(levels.iter().filter(|l| l.val.is_some()).count(), 1);
    }

    #[test]
    fn read_channel_returns_the_hal_value() {
        let mut engine = make_engine();
        engine.hal.set_analog(0, IN_CH, Ok(1234.0));
        let config = make_config(false);

        let (_, resp) = send(
            EngineCommand::ReadChannel { channel: IN_CH },
            &mut engine,
            &config,
        );
        let EngineResponse::Value {
            channel, raw, cur, ..
        } = resp
        else {
            panic!("expected Value");
        };
        assert_eq!(channel, IN_CH);
        assert_eq!(raw, 1234.0);
        assert!(cur.is_finite());
    }

    #[test]
    fn convert_value_echoes_channel_and_raw() {
        let mut engine = make_engine();
        let config = make_config(false);

        let (_, resp) = send(
            EngineCommand::ConvertValue {
                channel: IN_CH,
                raw: 500.0,
            },
            &mut engine,
            &config,
        );
        let EngineResponse::Value {
            channel, raw, cur, ..
        } = resp
        else {
            panic!("expected Value");
        };
        assert_eq!(channel, IN_CH);
        assert_eq!(raw, 500.0);
        assert!(cur.is_finite());
    }

    #[test]
    fn lists_and_status_reflect_the_engine() {
        let mut engine = make_engine();
        let config = make_config(false);

        let (_, resp) = send(EngineCommand::ListChannels, &mut engine, &config);
        let EngineResponse::Channels(channels) = resp else {
            panic!("expected Channels");
        };
        assert_eq!(channels.len(), 2);

        let (_, resp) = send(EngineCommand::ListPolls, &mut engine, &config);
        let EngineResponse::Polls(polls) = resp else {
            panic!("expected Polls");
        };
        assert_eq!(polls.len(), 1);
        assert_eq!(polls[0].channel, IN_CH);

        let (_, resp) = send(EngineCommand::ListTables, &mut engine, &config);
        let EngineResponse::Tables(tables) = resp else {
            panic!("expected Tables");
        };
        assert!(tables.is_empty());

        let (_, resp) = send(EngineCommand::Status, &mut engine, &config);
        let EngineResponse::Status(status) = resp else {
            panic!("expected Status");
        };
        assert_eq!(status.channel_count, 2);
        assert_eq!(status.poll_count, 1);
        assert_eq!(status.table_count, 0);
        assert_eq!(status.poll_interval_ms, 1000);
    }

    #[test]
    fn diagnostics_counts_fault_and_down_channels() {
        let mut engine = make_engine();
        engine.channels.get_mut(IN_CH).unwrap().value.status = EngineStatus::Fault;
        engine.channels.get_mut(OUT_CH).unwrap().value.status = EngineStatus::Down;
        let config = make_config(false);

        let (_, resp) = send(EngineCommand::Diagnostics, &mut engine, &config);
        let EngineResponse::Diagnostics(d) = resp else {
            panic!("expected Diagnostics");
        };
        assert_eq!(d.channels_total, 2);
        assert_eq!(d.channels_fault, 1);
        assert_eq!(d.channels_down, 1);
        assert_eq!(d.poll_interval_ms, 1000);
    }

    #[test]
    fn reload_without_config_dir_is_an_error() {
        let mut engine = make_engine();
        let config = make_config(false);
        let (_, resp) = send(EngineCommand::ReloadConfig, &mut engine, &config);
        assert!(error_text(&resp).contains("no config directory"));
        assert_eq!(engine.channels.count(), 2);
    }

    #[test]
    fn history_honours_channel_since_and_limit() {
        let mut engine = make_engine();
        let config = make_config(false);
        let mut history = HistoryStore::new(16);
        for ts in [10u64, 20, 30, 40] {
            history.record(
                IN_CH,
                HistoryPoint {
                    ts,
                    cur: ts as f64,
                    raw: 0.0,
                    status: EngineStatus::Ok,
                },
            );
        }
        let mut query = |channel, since_secs, limit| {
            let (_, resp) = run(
                EngineCommand::GetHistory {
                    channel,
                    since_secs,
                    limit,
                },
                &mut engine,
                &config,
                Instant::now(),
                &history,
            );
            match resp {
                EngineResponse::History(entries) => entries,
                other => panic!("expected History, got {:?}", other),
            }
        };

        assert_eq!(query(IN_CH, 0, 100).len(), 4);
        let since = query(IN_CH, 25, 100);
        assert_eq!(since.len(), 2);
        assert!(since.iter().all(|e| e.ts >= 25));
        assert_eq!(query(IN_CH, 0, 2).len(), 2);
        assert!(query(MISSING_CH, 0, 100).is_empty());
    }
}
