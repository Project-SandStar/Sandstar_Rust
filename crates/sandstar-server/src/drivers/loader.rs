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
use super::{DriverId, DriverPointRef};
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

/// Generic driver loader — reads the env var, parses configs, registers
/// drivers, wires up poll buckets, calls `open_all`, and spawns the periodic
/// tick task that flows `sync_cur` results into engine channels.
///
/// Errors at any stage are logged but do not abort the caller (identical
/// semantics to the previous `load_bacnet_drivers` / `load_mqtt_drivers`).
pub async fn load_drivers<L: DriverLoader>(
    handle: &DriverHandle,
    engine_handle: &EngineHandle,
) {
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
            let (mut ok_count, mut err_count, mut write_err_count) = (0usize, 0usize, 0usize);
            for (driver_id, point_id, res) in &results {
                match res {
                    Ok(v) => {
                        ok_count += 1;
                        tracing::info!(
                            driver = %driver_id,
                            point_id,
                            value = v,
                            "{} sync_cur -> write_channel",
                            L::LABEL
                        );
                        let who = format!("{}:{}", L::DRIVER_TYPE, driver_id);
                        if let Err(e) = engine_tick
                            .write_channel(
                                *point_id,
                                Some(*v),
                                DRIVER_WRITE_LEVEL,
                                who,
                                DRIVER_WRITE_DURATION_SECS,
                            )
                            .await
                        {
                            write_err_count += 1;
                            tracing::warn!(
                                driver = %driver_id,
                                point_id,
                                error = %e,
                                "{} engine write_channel failed — \
                                 is point_id a configured virtual channel?",
                                L::LABEL
                            );
                        }
                    }
                    Err(e) => {
                        err_count += 1;
                        tracing::warn!(
                            driver = %driver_id,
                            point_id,
                            error = %e,
                            "{} sync_cur failed",
                            L::LABEL
                        );
                    }
                }
            }
            tracing::info!(
                ok = ok_count,
                err = err_count,
                write_err = write_err_count,
                "{} poll tick complete",
                L::LABEL
            );
        }
    });
    tracing::info!("{} poll tick task spawned (5s interval)", L::LABEL);
}
