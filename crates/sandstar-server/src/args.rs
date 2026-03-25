//! Server CLI argument parsing.
//!
//! Uses `clap` with derive API. Supports CLI args, env vars, and defaults.
//! Priority: CLI arg > env var > default value.

use std::path::PathBuf;

use clap::Parser;

/// Sandstar Engine Server — embedded IoT control engine.
#[derive(Parser, Debug)]
#[command(name = "sandstar-engine-server", version, about)]
pub struct ServerArgs {
    /// Configuration directory containing points.csv, tables.csv, database.zinc.
    #[arg(long, short = 'c', env = "SANDSTAR_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,

    /// PID file path. Prevents multiple instances.
    #[arg(long, env = "SANDSTAR_PID_FILE")]
    pub pid_file: Option<PathBuf>,

    /// Disable PID file creation entirely.
    #[arg(long, default_value_t = false)]
    pub no_pid_file: bool,

    /// IPC socket path (Unix) or TCP address (Windows).
    #[arg(long, short = 's', env = "SANDSTAR_SOCKET", default_value_t = default_socket())]
    pub socket: String,

    /// Poll interval in milliseconds.
    #[arg(long, short = 'p', env = "SANDSTAR_POLL_INTERVAL_MS", default_value_t = 1000)]
    pub poll_interval_ms: u64,

    /// Log level filter (e.g., "info", "debug", "sandstar_engine=debug,info").
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    pub log_level: String,

    /// Log file path. Logs are written to this file in addition to stderr.
    #[arg(long, env = "SANDSTAR_LOG_FILE")]
    pub log_file: Option<PathBuf>,

    /// HTTP port for the Haystack REST API (0 = auto-assign).
    #[arg(long, env = "SANDSTAR_HTTP_PORT", default_value_t = 8085)]
    pub http_port: u16,

    /// HTTP bind address (default 127.0.0.1 for loopback; use 0.0.0.0 for all interfaces).
    #[arg(long, env = "SANDSTAR_HTTP_BIND", default_value = "127.0.0.1")]
    pub http_bind: String,

    /// Bearer token for protecting mutating (POST) endpoints.
    /// If not set, all endpoints are open. Set via env or CLI.
    /// Can coexist with SCRAM auth (--auth-user/--auth-pass).
    #[arg(long, env = "SANDSTAR_AUTH_TOKEN")]
    pub auth_token: Option<String>,

    /// Username for SCRAM-SHA-256 authentication. Requires --auth-pass.
    #[arg(long, env = "SANDSTAR_AUTH_USER")]
    pub auth_user: Option<String>,

    /// Password for SCRAM-SHA-256 authentication. Requires --auth-user.
    #[arg(long, env = "SANDSTAR_AUTH_PASS")]
    pub auth_pass: Option<String>,

    /// Maximum REST API requests per second (0 = unlimited).
    #[arg(long, env = "SANDSTAR_RATE_LIMIT", default_value_t = 100)]
    pub rate_limit: u64,

    /// Disable the REST API entirely.
    #[arg(long, default_value_t = false)]
    pub no_rest: bool,

    /// Read-only validation mode: disables watchdog, rejects output writes.
    /// Use for safe side-by-side operation with the C engine.
    #[arg(long, default_value_t = false)]
    pub read_only: bool,

    /// Enable the Sedona Virtual Machine (SVM).
    #[arg(long, default_value_t = false)]
    pub sedona: bool,

    /// Path to Sedona scode image file (kits.scode).
    /// Required when --sedona is enabled.
    #[arg(long, env = "SANDSTAR_SCODE_PATH")]
    pub scode_path: Option<PathBuf>,

    /// Disable the control engine (PID loops, sequencers).
    #[arg(long, default_value_t = false)]
    pub no_control: bool,

    /// Path to control configuration file (default: <config_dir>/control.toml).
    #[arg(long, env = "SANDSTAR_CONTROL_CONFIG")]
    pub control_config: Option<PathBuf>,

    /// TLS certificate file path (PEM format). Enables HTTPS when set.
    /// Requires the `tls` feature to be compiled in.
    #[arg(long, env = "SANDSTAR_TLS_CERT")]
    pub tls_cert: Option<PathBuf>,

    /// TLS private key file path (PEM format). Required when --tls-cert is set.
    #[arg(long, env = "SANDSTAR_TLS_KEY")]
    pub tls_key: Option<PathBuf>,

    /// Enable SOX protocol server for Sedona Application Editor.
    #[arg(long, default_value_t = false)]
    pub sox: bool,

    /// SOX/DASP UDP port (default 1876).
    #[arg(long, default_value_t = 1876)]
    pub sox_port: u16,

    /// SOX username (or env SANDSTAR_SOX_USER, default "admin").
    #[arg(long, env = "SANDSTAR_SOX_USER", default_value = "admin")]
    pub sox_user: String,

    /// SOX password (or env SANDSTAR_SOX_PASS, default "admin").
    #[arg(long, env = "SANDSTAR_SOX_PASS", default_value = "admin")]
    pub sox_pass: String,

    /// Path to kit manifest XML directory for SOX component schemas.
    /// On BeagleBone: /home/eacio/sandstar/etc/manifests
    /// If not set, uses the default path.
    #[arg(long, env = "SANDSTAR_MANIFESTS_DIR")]
    pub manifests_dir: Option<String>,
}

fn default_socket() -> String {
    if cfg!(unix) {
        "/tmp/sandstar-engine.sock".to_string()
    } else {
        "127.0.0.1:9813".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_args() {
        // Parse with no args (all defaults)
        let args = ServerArgs::parse_from(["sandstar-engine-server"]);
        assert!(args.config_dir.is_none());
        assert!(args.pid_file.is_none());
        assert!(!args.no_pid_file);
        assert_eq!(args.poll_interval_ms, 1000);
        assert_eq!(args.log_level, "info");
        assert!(args.log_file.is_none());
    }

    #[test]
    fn test_explicit_args() {
        let args = ServerArgs::parse_from([
            "sandstar-engine-server",
            "--config-dir",
            "/etc/sandstar",
            "--poll-interval-ms",
            "500",
            "--log-level",
            "debug",
            "--no-pid-file",
        ]);
        assert_eq!(
            args.config_dir,
            Some(PathBuf::from("/etc/sandstar"))
        );
        assert_eq!(args.poll_interval_ms, 500);
        assert_eq!(args.log_level, "debug");
        assert!(args.no_pid_file);
    }

    #[test]
    fn test_read_only_flag() {
        let args = ServerArgs::parse_from(["sandstar-engine-server", "--read-only"]);
        assert!(args.read_only);

        let args = ServerArgs::parse_from(["sandstar-engine-server"]);
        assert!(!args.read_only);
    }

    #[test]
    fn test_bind_address_default() {
        let args = ServerArgs::parse_from(["sandstar-engine-server"]);
        assert_eq!(args.http_bind, "127.0.0.1");
        assert!(args.auth_token.is_none());
    }

    #[test]
    fn test_auth_token_flag() {
        let args = ServerArgs::parse_from([
            "sandstar-engine-server",
            "--auth-token",
            "my-secret",
            "--http-bind",
            "0.0.0.0",
        ]);
        assert_eq!(args.auth_token, Some("my-secret".to_string()));
        assert_eq!(args.http_bind, "0.0.0.0");
    }

    #[test]
    fn test_short_flags() {
        let args = ServerArgs::parse_from([
            "sandstar-engine-server",
            "-c",
            "/etc/sandstar",
            "-p",
            "250",
        ]);
        assert_eq!(
            args.config_dir,
            Some(PathBuf::from("/etc/sandstar"))
        );
        assert_eq!(args.poll_interval_ms, 250);
    }

    #[test]
    fn test_no_control_flag() {
        let args = ServerArgs::parse_from(["sandstar-engine-server", "--no-control"]);
        assert!(args.no_control);
        assert!(args.control_config.is_none());

        let args = ServerArgs::parse_from(["sandstar-engine-server"]);
        assert!(!args.no_control);
    }

    #[test]
    fn test_control_config_path() {
        let args = ServerArgs::parse_from([
            "sandstar-engine-server",
            "--control-config",
            "/etc/sandstar/control.toml",
        ]);
        assert_eq!(
            args.control_config,
            Some(PathBuf::from("/etc/sandstar/control.toml"))
        );
    }

    #[test]
    fn test_rate_limit_default() {
        let args = ServerArgs::parse_from(["sandstar-engine-server"]);
        assert_eq!(args.rate_limit, 100);
    }

    #[test]
    fn test_rate_limit_custom() {
        let args = ServerArgs::parse_from([
            "sandstar-engine-server",
            "--rate-limit",
            "50",
        ]);
        assert_eq!(args.rate_limit, 50);
    }

    #[test]
    fn test_rate_limit_unlimited() {
        let args = ServerArgs::parse_from([
            "sandstar-engine-server",
            "--rate-limit",
            "0",
        ]);
        assert_eq!(args.rate_limit, 0);
    }

    #[test]
    fn test_tls_cert_flags() {
        let args = ServerArgs::parse_from([
            "sandstar-engine-server",
            "--tls-cert",
            "/etc/sandstar/cert.pem",
            "--tls-key",
            "/etc/sandstar/key.pem",
        ]);
        assert_eq!(args.tls_cert, Some(PathBuf::from("/etc/sandstar/cert.pem")));
        assert_eq!(args.tls_key, Some(PathBuf::from("/etc/sandstar/key.pem")));
    }

    #[test]
    fn test_tls_not_set_by_default() {
        let args = ServerArgs::parse_from(["sandstar-engine-server"]);
        assert!(args.tls_cert.is_none());
        assert!(args.tls_key.is_none());
    }

    #[test]
    fn test_tls_cert_only() {
        // It's valid to parse --tls-cert without --tls-key (validation happens at runtime)
        let args = ServerArgs::parse_from([
            "sandstar-engine-server",
            "--tls-cert",
            "/path/to/cert.pem",
        ]);
        assert!(args.tls_cert.is_some());
        assert!(args.tls_key.is_none());
    }

    #[test]
    fn test_sox_defaults() {
        let args = ServerArgs::parse_from(["sandstar-engine-server"]);
        assert!(!args.sox);
        assert_eq!(args.sox_port, 1876);
        assert_eq!(args.sox_user, "admin");
        assert_eq!(args.sox_pass, "admin");
    }

    #[test]
    fn test_sox_flag_enabled() {
        let args = ServerArgs::parse_from(["sandstar-engine-server", "--sox"]);
        assert!(args.sox);
        assert_eq!(args.sox_port, 1876);
    }

    #[test]
    fn test_sox_custom_port_and_creds() {
        let args = ServerArgs::parse_from([
            "sandstar-engine-server",
            "--sox",
            "--sox-port",
            "1877",
            "--sox-user",
            "myuser",
            "--sox-pass",
            "mypass",
        ]);
        assert!(args.sox);
        assert_eq!(args.sox_port, 1877);
        assert_eq!(args.sox_user, "myuser");
        assert_eq!(args.sox_pass, "mypass");
    }
}
