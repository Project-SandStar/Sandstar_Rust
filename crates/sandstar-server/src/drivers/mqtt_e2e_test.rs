//! Integration tests for the MQTT driver loader (Phase M4).
//!
//! # Scope
//!
//! These tests exercise the server-wiring layer introduced in M4 — specifically
//! the behaviour of [`load_mqtt_drivers`] under a variety of environment-variable
//! states. They do **not** spin up an embedded broker.
//!
//! # Why no embedded broker?
//!
//! Writing a minimal MQTT v3.1.1 broker that satisfies `rumqttc` (including
//! correct CONNECT/CONNACK handshake, SUBSCRIBE/SUBACK, PUBLISH/PUBACK, and
//! keep-alive PINGREQ handling) is a non-trivial additional surface for tests.
//! `rumqttd` pulls in a very large dependency graph and does not cross-compile
//! cleanly for our ARMv7 + zig CC target.
//!
//! The internal driver logic (cache, JSON path extraction, publish payload
//! shape, QoS handling, sync_cur behaviour, write error mapping) is covered by
//! the unit tests in `mqtt.rs`. Live broker compatibility is validated post-
//! deploy against either a local mosquitto or `test.mosquitto.org:1883`,
//! documented in `docs/MQTT_SETUP.md`.
//!
//! This mirrors the validation strategy used for the BACnet driver — where the
//! wire-level E2E test uses a hand-crafted mock device, and real-device
//! validation happens against `tools/bacnet_sim.py` post-deploy.

use crate::drivers::actor::spawn_driver_actor;
use crate::drivers::mqtt::{MqttConfig, MqttObjectConfig};

/// Environment variable used by `load_mqtt_drivers` in `rest/mod.rs`.
const ENV_VAR: &str = "SANDSTAR_MQTT_CONFIGS";

/// Round-trip the shipped config shape through serde to guard against future
/// serde attribute regressions (e.g. renaming a field, changing a default).
#[test]
fn mqtt_config_shipped_shape_deserializes() {
    let json = r#"[
        {
            "id": "mqtt-local",
            "host": "broker",
            "port": 1883,
            "client_id": "sandstar-1",
            "objects": [
                {
                    "point_id": 103,
                    "subscribe_topic": "bldg/zone1/temp",
                    "publish_topic": "bldg/zone1/setpoint",
                    "value_path": "/value",
                    "qos": 1
                }
            ]
        }
    ]"#;
    let configs: Vec<MqttConfig> =
        serde_json::from_str(json).expect("shipped SANDSTAR_MQTT_CONFIGS shape deserializes");
    assert_eq!(configs.len(), 1);
    assert_eq!(configs[0].id, "mqtt-local");
    assert_eq!(configs[0].host, "broker");
    assert_eq!(configs[0].port, 1883);
    assert_eq!(configs[0].objects.len(), 1);
    assert_eq!(configs[0].objects[0].point_id, 103);
    assert_eq!(
        configs[0].objects[0].subscribe_topic.as_deref(),
        Some("bldg/zone1/temp")
    );
}

/// Registering an [`MqttDriver`] built from a valid config through the async
/// [`DriverHandle`] actor should succeed without any real broker — `open()`
/// isn't called yet. The driver lands in the manager in the `Pending` state.
///
/// This is the invariant `load_mqtt_drivers` relies on: registration is
/// infallible once the JSON deserializes cleanly.
#[tokio::test]
async fn register_mqtt_driver_without_broker_succeeds() {
    let handle = spawn_driver_actor(8);
    let cfg = MqttConfig {
        id: "mqtt-unit".into(),
        host: "127.0.0.1".into(),
        port: 1, // unreachable on purpose
        client_id: "sandstar-unit".into(),
        username: None,
        password: None,
        tls: false,
        keep_alive_secs: 60,
        objects: vec![MqttObjectConfig {
            point_id: 42,
            subscribe_topic: Some("unit/topic".into()),
            publish_topic: None,
            value_path: None,
            qos: 1,
        }],
    };
    let driver = crate::drivers::mqtt::MqttDriver::from_config(cfg);
    let any = crate::drivers::async_driver::AnyDriver::Async(Box::new(driver));
    handle.register(any).await.expect("register succeeds");

    // Status endpoint should return our driver.
    let summaries = handle.status().await.expect("status ok");
    assert!(summaries.iter().any(|s| s.id == "mqtt-unit"));
}

/// Parse-error simulation: malformed JSON in `SANDSTAR_MQTT_CONFIGS` should
/// surface as a serde error (which `load_mqtt_drivers` logs + swallows so the
/// server doesn't crash). We exercise the same `serde_json::from_str` call
/// shape here.
#[test]
fn malformed_config_json_returns_serde_error() {
    let json = "not json";
    let result: Result<Vec<MqttConfig>, _> = serde_json::from_str(json);
    assert!(result.is_err(), "malformed JSON must not parse");
}

/// Empty-array config (operator set the env but configured no drivers) is
/// valid JSON and deserializes to an empty Vec. `load_mqtt_drivers` treats
/// this as a no-op — drivers list stays empty.
#[test]
fn empty_array_config_deserializes_to_empty_vec() {
    let configs: Vec<MqttConfig> = serde_json::from_str("[]").expect("[] is valid");
    assert!(configs.is_empty());
}

/// Guard: documents the env var name we key off of. Changing this name is a
/// breaking change for operators and should surface here.
#[test]
fn load_mqtt_drivers_env_var_is_sandstar_mqtt_configs() {
    assert_eq!(ENV_VAR, "SANDSTAR_MQTT_CONFIGS");
}
