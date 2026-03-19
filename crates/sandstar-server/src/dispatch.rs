//! Command dispatch: deserialize IPC commands, execute on engine, send response.

use std::io;
use std::time::Instant;

use sandstar_engine::{Engine, EngineValue};
use sandstar_hal::{HalDiagnostics, HalRead, HalWrite};
use sandstar_ipc::protocol::{read_frame, write_frame};
use sandstar_ipc::types::*;
use tracing::{debug, info, warn};

use crate::config::ServerConfig;
use crate::history::HistoryStore;
use crate::ipc::Stream;
use crate::reload;

/// Result of handling an IPC connection.
pub enum ConnectionResult {
    /// Normal command processed, continue accepting connections.
    Continue,
    /// Shutdown requested by client.
    Shutdown,
    /// PollNow requested — main loop should trigger async poll via spawn_blocking.
    PollNow,
}

/// Handle a single IPC connection: read one command, dispatch, write response.
pub fn handle_connection<H: HalRead + HalWrite + HalDiagnostics>(
    mut stream: Stream,
    engine: &mut Engine<H>,
    config: &ServerConfig,
    start_time: Instant,
    history_store: &HistoryStore,
) -> io::Result<ConnectionResult> {
    let cmd: EngineCommand = match read_frame(&mut stream)? {
        Some(cmd) => cmd,
        None => return Ok(ConnectionResult::Continue), // clean disconnect
    };

    debug!(?cmd, "received command");
    crate::metrics::metrics().ipc_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let result = match &cmd {
        EngineCommand::Shutdown => ConnectionResult::Shutdown,
        EngineCommand::PollNow => ConnectionResult::PollNow,
        _ => ConnectionResult::Continue,
    };
    let response = execute(cmd, engine, config, start_time, history_store);
    write_frame(&mut stream, &response)?;

    Ok(result)
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

        EngineCommand::GetWriteLevels { channel } => {
            match engine.get_write_levels(channel) {
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
            }
        }

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
            None => EngineResponse::Error(
                "no config directory — running in demo mode".to_string(),
            ),
        },

        EngineCommand::GetHistory { channel, since_secs, limit } => {
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
    }
}
