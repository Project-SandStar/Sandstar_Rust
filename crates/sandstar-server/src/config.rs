//! Server configuration and startup modes.

use std::path::PathBuf;

use tracing::{info, warn};

use crate::args::ServerArgs;
use crate::auth::AuthStore;

/// Server configuration.
pub struct ServerConfig {
    /// Path to the IPC socket (Unix) or TCP address (Windows).
    pub socket_path: String,

    /// Poll interval in milliseconds.
    pub poll_interval_ms: u64,

    /// Config directory for points.csv, tables.csv, database.zinc.
    /// None = use demo mode with hardcoded channels.
    pub config_dir: Option<PathBuf>,

    /// Read-only validation mode: disables watchdog, rejects output writes.
    pub read_only: bool,

    /// Auth store: holds bearer token and/or SCRAM credentials.
    pub auth_store: AuthStore,

    /// Legacy accessor for backward compat (mirrors auth_store.bearer_token).
    pub auth_token: Option<String>,

    /// Maximum REST API requests per second (0 = unlimited).
    pub rate_limit: u64,
}

impl ServerConfig {
    /// Build config from parsed CLI args.
    ///
    /// Clap handles the priority: CLI arg > env var > default.
    pub fn from_args(args: &ServerArgs) -> Self {
        let config_dir = args.config_dir.clone().filter(|p| p.exists());

        // Canonicalize the config directory to prevent path traversal attacks
        // (e.g., SANDSTAR_CONFIG_DIR="../../../etc")
        let config_dir = canonicalize_config_dir(config_dir);

        // Build AuthStore from CLI args
        let mut auth_store = AuthStore::new();

        if let Some(ref token) = args.auth_token {
            auth_store.set_bearer_token(token.clone());
        }

        // SCRAM auth: --auth-user + --auth-pass
        match (&args.auth_user, &args.auth_pass) {
            (Some(user), Some(pass)) => {
                auth_store.add_user(user, pass);
                info!(user = %user, "SCRAM-SHA-256 auth configured");
            }
            (Some(_), None) => {
                warn!("--auth-user provided without --auth-pass — SCRAM auth disabled");
            }
            (None, Some(_)) => {
                warn!("--auth-pass provided without --auth-user — SCRAM auth disabled");
            }
            (None, None) => {}
        }

        Self {
            socket_path: args.socket.clone(),
            poll_interval_ms: args.poll_interval_ms,
            config_dir,
            read_only: args.read_only,
            auth_store,
            auth_token: args.auth_token.clone(),
            rate_limit: args.rate_limit,
        }
    }
}

/// Canonicalize a config directory path and validate it is a real directory.
///
/// Returns `None` if the path does not exist, is not a directory, or
/// canonicalization fails. Logs a warning if the canonicalized path differs
/// from the original (potential path traversal attempt).
fn canonicalize_config_dir(dir: Option<PathBuf>) -> Option<PathBuf> {
    let original = dir?;

    let canonical = match original.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            warn!(
                path = %original.display(),
                err = %e,
                "config dir canonicalization failed — ignoring"
            );
            return None;
        }
    };

    if !canonical.is_dir() {
        warn!(
            path = %canonical.display(),
            "config dir path is not a directory — ignoring"
        );
        return None;
    }

    // Detect path traversal: compare original and canonical paths.
    // On Windows, canonicalize adds a \\?\ prefix, so we strip it for comparison.
    let original_str = original.to_string_lossy();
    let canonical_str = canonical.to_string_lossy();
    let canonical_stripped = canonical_str
        .strip_prefix(r"\\?\")
        .unwrap_or(&canonical_str);

    if original_str != canonical_stripped {
        warn!(
            original = %original_str,
            canonical = %canonical_stripped,
            "config dir path changed after canonicalization — possible traversal attempt"
        );
    }

    Some(canonical)
}

/// Load demo channels for testing (when no config_dir is provided).
///
/// Only available with the `mock-hal` feature — demo mode pre-seeds
/// MockHal with synthetic sensor values.
#[cfg(feature = "mock-hal")]
pub fn load_demo_channels(engine: &mut sandstar_engine::Engine<sandstar_hal::mock::MockHal>) {
    use sandstar_engine::channel::{Channel, ChannelDirection, ChannelType};
    use sandstar_engine::value::ValueConv;
    let channels = [
        (
            1113,
            ChannelType::Analog,
            ChannelDirection::In,
            0,
            0,
            "AI1 Thermistor 10K",
        ),
        (
            1200,
            ChannelType::Analog,
            ChannelDirection::In,
            0,
            1,
            "AI2 0-10V",
        ),
        (
            612,
            ChannelType::I2c,
            ChannelDirection::In,
            2,
            0x40,
            "I2C SDP610 CFM",
        ),
        (
            2001,
            ChannelType::Digital,
            ChannelDirection::Out,
            0,
            47,
            "DO1 Relay",
        ),
        (
            2002,
            ChannelType::Pwm,
            ChannelDirection::Out,
            0,
            0,
            "PWM1 Fan Speed",
        ),
    ];

    for (id, ct, dir, dev, addr, label) in channels {
        let ch = Channel::new(id, ct, dir, dev, addr, false, ValueConv::default(), label);
        if let Err(e) = engine.channels.add(ch) {
            tracing::warn!(channel = id, err = %e, "failed to add demo channel");
        }
        if !dir.is_output() {
            let _ = engine.polls.add(id);
        }
    }

    // Pre-load mock HAL values for demo channels
    engine.hal.set_analog(0, 0, Ok(2048.0));
    engine.hal.set_analog(0, 1, Ok(3276.0));
    engine.hal.set_i2c(2, 0x40, "I2C SDP610 CFM", Ok(500.0));
}

/// Load demo channels for simulator HAL (when no config_dir is provided).
///
/// Sets up the same demo channels as mock-hal mode, but injects initial
/// sensor values via the shared simulator state.
#[cfg(feature = "simulator-hal")]
pub fn load_demo_channels(
    engine: &mut sandstar_engine::Engine<sandstar_hal::simulator::SimulatorHal>,
) {
    use sandstar_engine::channel::{Channel, ChannelDirection, ChannelType};
    use sandstar_engine::value::ValueConv;
    use sandstar_hal::simulator::ReadKey;

    // Physical I/O channels (match BASemulator mapping)
    let channels = [
        // Analog inputs — 4 temperature/voltage sensors
        (
            1113,
            ChannelType::Analog,
            ChannelDirection::In,
            0u32,
            0u32,
            "AI1 Thermistor 10K",
        ),
        (
            1200,
            ChannelType::Analog,
            ChannelDirection::In,
            0,
            1,
            "AI2 Discharge Air",
        ),
        (
            1300,
            ChannelType::Analog,
            ChannelDirection::In,
            0,
            2,
            "AI3 Outdoor Air",
        ),
        (
            1400,
            ChannelType::Analog,
            ChannelDirection::In,
            0,
            3,
            "AI4 Return Air",
        ),
        // I2C airflow sensor
        (
            612,
            ChannelType::I2c,
            ChannelDirection::In,
            2,
            0x40,
            "I2C SDP810 CFM",
        ),
        // Digital input — occupancy
        (
            2100,
            ChannelType::Digital,
            ChannelDirection::In,
            0,
            40,
            "DI1 Occupancy",
        ),
        // Digital outputs — 4 cooling stages
        (
            2001,
            ChannelType::Digital,
            ChannelDirection::Out,
            0,
            45,
            "DO1 Cool Stage 1",
        ),
        (
            2002,
            ChannelType::Digital,
            ChannelDirection::Out,
            0,
            46,
            "DO2 Cool Stage 2",
        ),
        (
            2003,
            ChannelType::Digital,
            ChannelDirection::Out,
            0,
            47,
            "DO3 Cool Stage 3",
        ),
        (
            2004,
            ChannelType::Digital,
            ChannelDirection::Out,
            0,
            48,
            "DO4 Cool Stage 4",
        ),
        // PWM output — fan speed
        (
            2005,
            ChannelType::Pwm,
            ChannelDirection::Out,
            4,
            0,
            "PWM1 Fan Speed",
        ),
    ];

    for (id, ct, dir, dev, addr, label) in channels {
        let ch = Channel::new(id, ct, dir, dev, addr, false, ValueConv::default(), label);
        if let Err(e) = engine.channels.add(ch) {
            tracing::warn!(channel = id, err = %e, "failed to add demo channel");
        }
        if !dir.is_output() {
            let _ = engine.polls.add(id);
        }
    }

    // Virtual channels — for control engine setpoints and intermediate values
    let virtuals = [
        (7500, ChannelType::VirtualAnalog, "Zone Cooling Setpoint"),
        (7501, ChannelType::VirtualAnalog, "Scheduled Setpoint"),
        (7502, ChannelType::VirtualAnalog, "Calibrated Temp"),
        (7503, ChannelType::VirtualAnalog, "Fan Speed Command"),
        (7504, ChannelType::VirtualDigital, "Aux Heat Relay"),
        (7505, ChannelType::VirtualDigital, "Safety Interlock"),
        (7506, ChannelType::VirtualDigital, "Compressor Delay"),
    ];

    for (id, ct, label) in virtuals {
        let ch = Channel::new(
            id,
            ct,
            ChannelDirection::Out,
            0,
            0,
            false,
            ValueConv::default(),
            label,
        );
        if let Err(e) = engine.channels.add(ch) {
            tracing::warn!(channel = id, err = %e, "failed to add virtual channel");
        }
    }

    // Pre-load simulator state with realistic initial sensor values.
    // With default ValueConv (no table/scale), raw passes through as cur directly,
    // so we inject engineering units (°F, CFM) rather than raw ADC counts.
    let state = engine.hal.shared_state();
    let mut s = state.write().expect("sim state lock poisoned");
    s.reads.insert(
        ReadKey::Analog {
            device: 0,
            address: 0,
        },
        70.0,
    ); // 70°F zone temp
    s.reads.insert(
        ReadKey::Analog {
            device: 0,
            address: 1,
        },
        55.0,
    ); // 55°F discharge air
    s.reads.insert(
        ReadKey::Analog {
            device: 0,
            address: 2,
        },
        85.0,
    ); // 85°F outdoor air
    s.reads.insert(
        ReadKey::Analog {
            device: 0,
            address: 3,
        },
        68.0,
    ); // 68°F return air
    s.reads.insert(
        ReadKey::I2c {
            device: 2,
            address: 0x40,
            label: "I2C SDP810 CFM".to_string(),
        },
        500.0,
    );
    s.digital_reads.insert(40, true); // Occupied
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn canonicalize_existing_dir() {
        let dir = TempDir::new().unwrap();
        let result = canonicalize_config_dir(Some(dir.path().to_path_buf()));
        assert!(result.is_some(), "existing dir should canonicalize");
        // Canonicalized path must be absolute
        assert!(result.unwrap().is_absolute());
    }

    #[test]
    fn canonicalize_none_returns_none() {
        assert!(canonicalize_config_dir(None).is_none());
    }

    #[test]
    fn canonicalize_nonexistent_returns_none() {
        let result = canonicalize_config_dir(Some(PathBuf::from("/nonexistent/path/xyz123")));
        assert!(result.is_none(), "nonexistent path should return None");
    }

    #[test]
    fn canonicalize_file_not_dir_returns_none() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("afile.txt");
        std::fs::write(&file, "hello").unwrap();
        let result = canonicalize_config_dir(Some(file));
        assert!(
            result.is_none(),
            "file (not a directory) should return None"
        );
    }

    #[test]
    fn canonicalize_resolves_dot_dot() {
        // Create a/b/ directories, then pass a/../a/b which should resolve to a/b
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a");
        let b = a.join("b");
        std::fs::create_dir_all(&b).unwrap();

        let traversal = a.join("..").join("a").join("b");
        let result = canonicalize_config_dir(Some(traversal));
        assert!(result.is_some());
        let canon = result.unwrap();
        // The canonical path should NOT contain ".."
        assert!(
            !canon.to_string_lossy().contains(".."),
            "canonicalized path should not contain '..': {}",
            canon.display()
        );
    }
}
