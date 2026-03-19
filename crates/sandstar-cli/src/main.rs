//! Sandstar CLI — command-line tools for the Sandstar engine.
//!
//! Replaces 12 separate C CLI binaries with a single binary + subcommands.
//!
//! # Usage
//!
//! ```text
//! sandstar-cli read 1113
//! sandstar-cli write 2001 1.0
//! sandstar-cli channels
//! sandstar-cli status
//! sandstar-cli poll
//! sandstar-cli shutdown
//! ```

mod client;

use clap::{Parser, Subcommand};
use sandstar_ipc::types::{EngineCommand, EngineResponse};
use sandstar_server::sax_converter;
use serde_json::json;

#[derive(Parser)]
#[command(name = "sandstar-cli", about = "Sandstar engine CLI tools")]
struct Cli {
    /// Engine server address (Unix socket path or host:port).
    #[arg(
        short,
        long,
        default_value_t = default_socket_path(),
        global = true
    )]
    socket: String,

    /// Output response as JSON instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

fn default_socket_path() -> String {
    if cfg!(unix) {
        "/tmp/sandstar-engine.sock".to_string()
    } else {
        "127.0.0.1:9813".to_string()
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Read a channel value.
    Read {
        /// Channel number (e.g., 1113).
        channel: u32,
    },

    /// Write a value to an output channel at a priority level.
    Write {
        /// Channel number (e.g., 2001).
        channel: u32,
        /// Value to write.
        value: f64,
        /// Priority level 1-17 (default 17).
        #[arg(long, default_value_t = 17)]
        level: u8,
    },

    /// Relinquish a priority level (set to null).
    Relinquish {
        /// Channel number.
        channel: u32,
        /// Priority level to relinquish (1-17).
        level: u8,
    },

    /// Show the 17-level priority array for a channel.
    WriteLevels {
        /// Channel number.
        channel: u32,
    },

    /// Convert a raw value without writing to hardware.
    Convert {
        /// Channel number.
        channel: u32,
        /// Raw ADC value.
        raw: f64,
    },

    /// List all configured channels.
    Channels,

    /// List all loaded lookup tables.
    Tables,

    /// List all polled channels.
    Polls,

    /// Show engine status.
    Status,

    /// Trigger a poll cycle immediately.
    Poll,

    /// Shut down the engine server.
    Shutdown,

    /// Reload configuration from disk (database.zinc, tables).
    Reload,

    /// Query channel value history from the ring buffer.
    History {
        /// Channel number.
        channel: u32,
        /// Return points from the last N seconds (default: 3600 = 1 hour).
        #[arg(long, default_value_t = 3600)]
        since: u64,
        /// Maximum number of points to return (default: 100).
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },

    /// Convert a Sedona .sax file to control.toml format.
    ConvertSax {
        /// Path to the .sax XML file.
        sax_file: String,
        /// Output path (default: stdout).
        #[arg(short, long)]
        output: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();

    // ConvertSax is a local command — no IPC needed.
    if let Commands::ConvertSax { sax_file, output } = &cli.command {
        match sax_converter::convert_sax_to_toml(sax_file) {
            Ok(result) => {
                // Print warnings to stderr.
                for w in &result.warnings {
                    eprintln!("{w}");
                }
                // Write TOML to file or stdout.
                if let Some(out_path) = output {
                    if let Err(e) = std::fs::write(out_path, &result.toml) {
                        eprintln!("error writing {out_path}: {e}");
                        std::process::exit(1);
                    }
                    eprintln!("Wrote {}", out_path);
                } else {
                    print!("{}", result.toml);
                }
                return;
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    let cmd = match &cli.command {
        Commands::Read { channel } => EngineCommand::ReadChannel { channel: *channel },
        Commands::Write {
            channel,
            value,
            level,
        } => EngineCommand::WriteChannel {
            channel: *channel,
            value: *value,
            level: *level,
        },
        Commands::Relinquish { channel, level } => EngineCommand::RelinquishLevel {
            channel: *channel,
            level: *level,
        },
        Commands::WriteLevels { channel } => EngineCommand::GetWriteLevels {
            channel: *channel,
        },
        Commands::Convert { channel, raw } => EngineCommand::ConvertValue {
            channel: *channel,
            raw: *raw,
        },
        Commands::Channels => EngineCommand::ListChannels,
        Commands::Tables => EngineCommand::ListTables,
        Commands::Polls => EngineCommand::ListPolls,
        Commands::Status => EngineCommand::Status,
        Commands::Poll => EngineCommand::PollNow,
        Commands::Shutdown => EngineCommand::Shutdown,
        Commands::Reload => EngineCommand::ReloadConfig,
        Commands::History { channel, since, limit } => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            EngineCommand::GetHistory {
                channel: *channel,
                since_secs: now.saturating_sub(*since),
                limit: *limit,
            }
        }
        Commands::ConvertSax { .. } => unreachable!("handled above"),
    };

    match client::send_command(&cli.socket, &cmd) {
        Ok(response) => print_response(response, cli.json),
        Err(e) => {
            if cli.json {
                let err = json!({"ok": false, "error": format!("{e}")});
                println!("{}", serde_json::to_string_pretty(&err).unwrap());
            } else {
                eprintln!("error: {}", e);
                eprintln!("  is the engine server running?");
                eprintln!("  socket: {}", cli.socket);
            }
            std::process::exit(1);
        }
    }
}

fn print_response(response: EngineResponse, as_json: bool) {
    if as_json {
        print_json(response);
    } else {
        print_text(response);
    }
}

fn print_json(response: EngineResponse) {
    let output = match response {
        EngineResponse::Ok => json!({"ok": true}),
        EngineResponse::Value { channel, status, raw, cur } => {
            json!({"channel": channel, "status": status, "raw": raw, "cur": cur})
        }
        EngineResponse::Channels(ch) => serde_json::to_value(&ch).unwrap(),
        EngineResponse::Tables(t) => json!({"tables": t}),
        EngineResponse::Polls(p) => serde_json::to_value(&p).unwrap(),
        EngineResponse::Status(s) => serde_json::to_value(&s).unwrap(),
        EngineResponse::WriteLevels(levels) => serde_json::to_value(&levels).unwrap(),
        EngineResponse::History(entries) => serde_json::to_value(&entries).unwrap(),
        EngineResponse::Error(msg) => {
            let err = json!({"ok": false, "error": msg});
            println!("{}", serde_json::to_string_pretty(&err).unwrap());
            std::process::exit(1);
        }
    };
    println!("{}", serde_json::to_string_pretty(&output).unwrap());
}

fn print_text(response: EngineResponse) {
    match response {
        EngineResponse::Ok => {
            println!("ok");
        }

        EngineResponse::Value {
            channel,
            status,
            raw,
            cur,
        } => {
            println!("channel: {}", channel);
            println!("status:  {}", status);
            println!("raw:     {:.4}", raw);
            println!("cur:     {:.4}", cur);
        }

        EngineResponse::Channels(channels) => {
            if channels.is_empty() {
                println!("(no channels configured)");
                return;
            }
            println!(
                "{:<8} {:<8} {:<8} {:<5} {:<10} {:<12} LABEL",
                "ID", "TYPE", "DIR", "EN", "STATUS", "CUR"
            );
            println!("{}", "-".repeat(70));
            for ch in channels {
                println!(
                    "{:<8} {:<8} {:<8} {:<5} {:<10} {:<12.4} {}",
                    ch.id,
                    ch.channel_type,
                    ch.direction,
                    if ch.enabled { "yes" } else { "no" },
                    ch.status,
                    ch.cur,
                    ch.label,
                );
            }
        }

        EngineResponse::Tables(tables) => {
            if tables.is_empty() {
                println!("(no tables loaded)");
                return;
            }
            for t in tables {
                println!("{}", t);
            }
        }

        EngineResponse::Polls(polls) => {
            if polls.is_empty() {
                println!("(no polls configured)");
                return;
            }
            println!("{:<8} {:<10} {:<12}", "CH", "STATUS", "LAST CUR");
            println!("{}", "-".repeat(32));
            for p in polls {
                println!("{:<8} {:<10} {:<12.4}", p.channel, p.last_status, p.last_cur);
            }
        }

        EngineResponse::Status(info) => {
            println!("Sandstar Engine Status");
            println!("  uptime:      {}s", info.uptime_secs);
            println!("  channels:    {}", info.channel_count);
            println!("  polls:       {}", info.poll_count);
            println!("  tables:      {}", info.table_count);
            println!("  poll interval: {}ms", info.poll_interval_ms);
        }

        EngineResponse::WriteLevels(levels) => {
            println!(
                "{:<6} {:<12} {:<12} WHO",
                "LVL", "LEVEL DIS", "VALUE"
            );
            println!("{}", "-".repeat(44));
            for l in levels {
                let val_str = match l.val {
                    Some(v) => format!("{:.4}", v),
                    None => "null".to_string(),
                };
                println!("{:<6} {:<12} {:<12} {}", l.level, l.level_dis, val_str, l.who);
            }
        }

        EngineResponse::History(entries) => {
            if entries.is_empty() {
                println!("(no history data)");
                return;
            }
            println!("{:<20} {:<12} {:<12} STATUS", "TIMESTAMP", "CUR", "RAW");
            println!("{}", "-".repeat(56));
            for e in entries {
                println!("{:<20} {:<12.4} {:<12.4} {}", e.ts, e.cur, e.raw, e.status);
            }
        }

        EngineResponse::Error(msg) => {
            eprintln!("engine error: {}", msg);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn try_parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn test_status_subcommand() {
        let cli = try_parse(&["sandstar-cli", "status"]).unwrap();
        assert!(matches!(cli.command, Commands::Status));
    }

    #[test]
    fn test_channels_subcommand() {
        let cli = try_parse(&["sandstar-cli", "channels"]).unwrap();
        assert!(matches!(cli.command, Commands::Channels));
    }

    #[test]
    fn test_polls_subcommand() {
        let cli = try_parse(&["sandstar-cli", "polls"]).unwrap();
        assert!(matches!(cli.command, Commands::Polls));
    }

    #[test]
    fn test_tables_subcommand() {
        let cli = try_parse(&["sandstar-cli", "tables"]).unwrap();
        assert!(matches!(cli.command, Commands::Tables));
    }

    #[test]
    fn test_read_subcommand() {
        let cli = try_parse(&["sandstar-cli", "read", "1113"]).unwrap();
        match cli.command {
            Commands::Read { channel } => assert_eq!(channel, 1113),
            _ => panic!("expected Read command"),
        }
    }

    #[test]
    fn test_write_subcommand_default_level() {
        let cli = try_parse(&["sandstar-cli", "write", "2001", "72.5"]).unwrap();
        match cli.command {
            Commands::Write { channel, value, level } => {
                assert_eq!(channel, 2001);
                assert!((value - 72.5).abs() < f64::EPSILON);
                assert_eq!(level, 17); // default
            }
            _ => panic!("expected Write command"),
        }
    }

    #[test]
    fn test_write_subcommand_custom_level() {
        let cli = try_parse(&["sandstar-cli", "write", "2001", "72.5", "--level", "8"]).unwrap();
        match cli.command {
            Commands::Write { channel, value, level } => {
                assert_eq!(channel, 2001);
                assert!((value - 72.5).abs() < f64::EPSILON);
                assert_eq!(level, 8);
            }
            _ => panic!("expected Write command"),
        }
    }

    #[test]
    fn test_json_flag() {
        let cli = try_parse(&["sandstar-cli", "--json", "status"]).unwrap();
        assert!(cli.json);
        assert!(matches!(cli.command, Commands::Status));
    }

    #[test]
    fn test_history_subcommand_defaults() {
        let cli = try_parse(&["sandstar-cli", "history", "1113"]).unwrap();
        match cli.command {
            Commands::History { channel, since, limit } => {
                assert_eq!(channel, 1113);
                assert_eq!(since, 3600); // default
                assert_eq!(limit, 100);  // default
            }
            _ => panic!("expected History command"),
        }
    }

    #[test]
    fn test_history_subcommand_custom_flags() {
        let cli = try_parse(&[
            "sandstar-cli", "history", "1113", "--since", "7200", "--limit", "50",
        ]).unwrap();
        match cli.command {
            Commands::History { channel, since, limit } => {
                assert_eq!(channel, 1113);
                assert_eq!(since, 7200);
                assert_eq!(limit, 50);
            }
            _ => panic!("expected History command"),
        }
    }

    #[test]
    fn test_convert_sax_subcommand() {
        let cli = try_parse(&["sandstar-cli", "convert-sax", "app.sax"]).unwrap();
        match cli.command {
            Commands::ConvertSax { sax_file, output } => {
                assert_eq!(sax_file, "app.sax");
                assert!(output.is_none());
            }
            _ => panic!("expected ConvertSax command"),
        }
    }

    #[test]
    fn test_convert_sax_with_output() {
        let cli = try_parse(&[
            "sandstar-cli", "convert-sax", "app.sax", "-o", "control.toml",
        ]).unwrap();
        match cli.command {
            Commands::ConvertSax { sax_file, output } => {
                assert_eq!(sax_file, "app.sax");
                assert_eq!(output.as_deref(), Some("control.toml"));
            }
            _ => panic!("expected ConvertSax command"),
        }
    }

    #[test]
    fn test_default_socket_address() {
        let cli = try_parse(&["sandstar-cli", "status"]).unwrap();
        let expected = if cfg!(unix) {
            "/tmp/sandstar-engine.sock"
        } else {
            "127.0.0.1:9813"
        };
        assert_eq!(cli.socket, expected);
    }

    #[test]
    fn test_custom_socket() {
        let cli = try_parse(&["sandstar-cli", "-s", "127.0.0.1:5000", "status"]).unwrap();
        assert_eq!(cli.socket, "127.0.0.1:5000");
    }

    #[test]
    fn test_shutdown_subcommand() {
        let cli = try_parse(&["sandstar-cli", "shutdown"]).unwrap();
        assert!(matches!(cli.command, Commands::Shutdown));
    }

    #[test]
    fn test_reload_subcommand() {
        let cli = try_parse(&["sandstar-cli", "reload"]).unwrap();
        assert!(matches!(cli.command, Commands::Reload));
    }

    #[test]
    fn test_poll_subcommand() {
        let cli = try_parse(&["sandstar-cli", "poll"]).unwrap();
        assert!(matches!(cli.command, Commands::Poll));
    }

    #[test]
    fn test_relinquish_subcommand() {
        let cli = try_parse(&["sandstar-cli", "relinquish", "2001", "8"]).unwrap();
        match cli.command {
            Commands::Relinquish { channel, level } => {
                assert_eq!(channel, 2001);
                assert_eq!(level, 8);
            }
            _ => panic!("expected Relinquish command"),
        }
    }

    #[test]
    fn test_write_levels_subcommand() {
        let cli = try_parse(&["sandstar-cli", "write-levels", "1113"]).unwrap();
        match cli.command {
            Commands::WriteLevels { channel } => assert_eq!(channel, 1113),
            _ => panic!("expected WriteLevels command"),
        }
    }
}
