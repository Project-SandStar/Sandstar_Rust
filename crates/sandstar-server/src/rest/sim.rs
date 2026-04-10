//! Simulator REST endpoints for injecting sensor data and reading control outputs.
//!
//! Only available when compiled with `simulator-hal` feature.
//! Allows external tools (BASemulator, scenario runners) to drive the engine
//! by injecting fake sensor values and reading back control outputs.

use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use sandstar_hal::simulator::{ReadKey, SharedSimState, WriteKey};
use serde::{Deserialize, Serialize};
use std::time::Duration;

// ── Request/Response types ──────────────────────────────────

/// POST /api/sim/inject request body.
#[derive(Debug, Deserialize)]
pub struct InjectRequest {
    pub points: Vec<InjectPoint>,
}

/// A single point to inject into the simulator HAL.
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InjectPoint {
    Analog {
        device: u32,
        address: u32,
        value: f64,
    },
    Digital {
        address: u32,
        value: bool,
    },
    I2c {
        device: u32,
        address: u32,
        label: String,
        value: f64,
    },
    Pwm {
        chip: u32,
        channel: u32,
        value: f64,
    },
    Uart {
        device: u32,
        label: String,
        value: f64,
    },
}

impl InjectPoint {
    fn type_name(&self) -> &'static str {
        match self {
            InjectPoint::Analog { .. } => "analog",
            InjectPoint::Digital { .. } => "digital",
            InjectPoint::I2c { .. } => "i2c",
            InjectPoint::Pwm { .. } => "pwm",
            InjectPoint::Uart { .. } => "uart",
        }
    }

    fn value_f64(&self) -> f64 {
        match self {
            InjectPoint::Analog { value, .. } => *value,
            InjectPoint::Digital { value, .. } => {
                if *value {
                    1.0
                } else {
                    0.0
                }
            }
            InjectPoint::I2c { value, .. } => *value,
            InjectPoint::Pwm { value, .. } => *value,
            InjectPoint::Uart { value, .. } => *value,
        }
    }
}

/// POST /api/sim/inject response.
#[derive(Debug, Serialize, Deserialize)]
pub struct InjectResponse {
    pub injected: usize,
}

/// GET /api/sim/outputs response.
#[derive(Debug, Serialize, Deserialize)]
pub struct OutputsResponse {
    pub outputs: Vec<OutputEntry>,
}

/// A single output written by the engine (captured by SimulatorHal).
#[derive(Debug, Serialize, Deserialize)]
pub struct OutputEntry {
    pub key: WriteKeyJson,
    pub value: f64,
}

/// JSON-serializable wrapper for WriteKey (in case the HAL type
/// doesn't derive Serialize, we map it here).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WriteKeyJson {
    Digital { address: u32 },
    Pwm { chip: u32, channel: u32 },
}

impl From<WriteKey> for WriteKeyJson {
    fn from(k: WriteKey) -> Self {
        match k {
            WriteKey::Digital { address } => WriteKeyJson::Digital { address },
            WriteKey::Pwm { chip, channel } => WriteKeyJson::Pwm { chip, channel },
        }
    }
}

/// GET /api/sim/state response.
#[derive(Debug, Serialize, Deserialize)]
pub struct StateResponse {
    pub read_count: usize,
    pub digital_read_count: usize,
    pub write_count: usize,
}

/// POST /api/sim/scenario request body.
#[derive(Debug, Deserialize)]
pub struct ScenarioRequest {
    pub steps: Vec<ScenarioStep>,
}

/// A single timed step in a scenario.
#[derive(Debug, Deserialize, Clone)]
pub struct ScenarioStep {
    /// Milliseconds from scenario start when this step fires.
    pub offset_ms: u64,
    /// Points to inject at this step.
    pub points: Vec<InjectPoint>,
}

/// POST /api/sim/scenario response.
#[derive(Debug, Serialize, Deserialize)]
pub struct ScenarioResponse {
    pub status: String,
    pub steps: usize,
}

// ── Handlers ────────────────────────────────────────────────

/// POST /api/sim/inject — inject sensor values into the simulator HAL.
///
/// Accepts a batch of typed points (analog, digital, i2c, pwm, uart)
/// and stores them in the shared sim state for the next poll cycle.
async fn inject_handler(
    State(state): State<SharedSimState>,
    Json(req): Json<InjectRequest>,
) -> Json<InjectResponse> {
    let mut s = state.write().expect("sim state lock poisoned");
    let count = req.points.len();
    for point in &req.points {
        tracing::debug!(
            point_type = point.type_name(),
            value = point.value_f64(),
            "sim inject"
        );
    }
    for point in req.points {
        apply_inject_point(&mut s, point);
    }
    tracing::info!(count, "sim injected points");
    Json(InjectResponse { injected: count })
}

/// GET /api/sim/outputs — drain and return all engine writes since last call.
///
/// Each call drains the write buffer, so outputs are only returned once.
/// This prevents stale data accumulation during long test runs.
async fn outputs_handler(State(state): State<SharedSimState>) -> Json<OutputsResponse> {
    let mut s = state.write().expect("sim state lock poisoned");
    let outputs: Vec<OutputEntry> = s
        .writes
        .drain()
        .map(|(key, value)| OutputEntry {
            key: WriteKeyJson::from(key),
            value,
        })
        .collect();
    if !outputs.is_empty() {
        for out in &outputs {
            tracing::debug!(key = ?out.key, value = out.value, "sim output drained");
        }
        tracing::info!(count = outputs.len(), "sim outputs drained");
    }
    Json(OutputsResponse { outputs })
}

/// GET /api/sim/state — snapshot of current sim state sizes (read-only).
async fn state_handler(State(state): State<SharedSimState>) -> Json<StateResponse> {
    let s = state.read().expect("sim state lock poisoned");
    Json(StateResponse {
        read_count: s.reads.len(),
        digital_read_count: s.digital_reads.len(),
        write_count: s.writes.len(),
    })
}

/// POST /api/sim/scenario — run a timed sequence of inject steps.
///
/// Spawns a background task that injects points at the specified offsets.
/// Returns immediately with `"status": "started"`.
async fn scenario_handler(
    State(state): State<SharedSimState>,
    Json(req): Json<ScenarioRequest>,
) -> Json<ScenarioResponse> {
    let step_count = req.steps.len();
    let state_clone = state.clone();
    tracing::info!(steps = step_count, "sim scenario started");
    tokio::spawn(async move {
        let start = tokio::time::Instant::now();
        for (i, step) in req.steps.into_iter().enumerate() {
            let offset_ms = step.offset_ms;
            let target = start + Duration::from_millis(offset_ms);
            tokio::time::sleep_until(target).await;
            let point_count = step.points.len();
            let mut s = state_clone.write().expect("sim state lock poisoned");
            for point in step.points {
                apply_inject_point(&mut s, point);
            }
            tracing::debug!(
                step = i,
                offset_ms,
                points = point_count,
                "sim scenario step"
            );
        }
        tracing::info!("sim scenario complete");
    });
    Json(ScenarioResponse {
        status: "started".to_string(),
        steps: step_count,
    })
}

// ── Helpers ─────────────────────────────────────────────────

/// Apply a single inject point to the sim state.
fn apply_inject_point(s: &mut sandstar_hal::simulator::SimState, point: InjectPoint) {
    match point {
        InjectPoint::Analog {
            device,
            address,
            value,
        } => {
            s.reads.insert(ReadKey::Analog { device, address }, value);
        }
        InjectPoint::Digital { address, value } => {
            s.digital_reads.insert(address, value);
        }
        InjectPoint::I2c {
            device,
            address,
            label,
            value,
        } => {
            s.reads.insert(
                ReadKey::I2c {
                    device,
                    address,
                    label,
                },
                value,
            );
        }
        InjectPoint::Pwm {
            chip,
            channel,
            value,
        } => {
            s.reads.insert(ReadKey::Pwm { chip, channel }, value);
        }
        InjectPoint::Uart {
            device,
            label,
            value,
        } => {
            s.reads.insert(ReadKey::Uart { device, label }, value);
        }
    }
}

// ── Router ──────────────────────────────────────────────────

/// Build the simulator sub-router.
///
/// Mounts four endpoints under `/api/sim/`:
/// - `POST /api/sim/inject` — inject sensor values
/// - `GET  /api/sim/outputs` — drain engine writes
/// - `GET  /api/sim/state` — read state summary
/// - `POST /api/sim/scenario` — run timed injection sequence
pub fn router(state: SharedSimState) -> Router {
    Router::new()
        .route("/api/sim/inject", post(inject_handler))
        .route("/api/sim/outputs", get(outputs_handler))
        .route("/api/sim/state", get(state_handler))
        .route("/api/sim/scenario", post(scenario_handler))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use sandstar_hal::simulator::SimState;
    use std::sync::{Arc, RwLock};
    use tower::ServiceExt;

    fn test_state() -> SharedSimState {
        Arc::new(RwLock::new(SimState::new()))
    }

    #[tokio::test]
    async fn test_inject_analog() {
        let state = test_state();
        let app = router(state.clone());

        let body = serde_json::json!({
            "points": [
                {"type": "analog", "device": 0, "address": 0, "value": 2048.0},
                {"type": "analog", "device": 0, "address": 1, "value": 1024.0}
            ]
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sim/inject")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let result: InjectResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result.injected, 2);

        // Verify state was updated
        let s = state.read().unwrap();
        assert_eq!(s.reads.len(), 2);
        assert_eq!(
            *s.reads
                .get(&ReadKey::Analog {
                    device: 0,
                    address: 0
                })
                .unwrap(),
            2048.0
        );
    }

    #[tokio::test]
    async fn test_inject_digital() {
        let state = test_state();
        let app = router(state.clone());

        let body = serde_json::json!({
            "points": [
                {"type": "digital", "address": 45, "value": true}
            ]
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sim/inject")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let s = state.read().unwrap();
        assert_eq!(*s.digital_reads.get(&45).unwrap(), true);
    }

    #[tokio::test]
    async fn test_inject_i2c() {
        let state = test_state();
        let app = router(state.clone());

        let body = serde_json::json!({
            "points": [
                {"type": "i2c", "device": 2, "address": 64, "label": "sdp810", "value": 120.5}
            ]
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sim/inject")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let s = state.read().unwrap();
        let key = ReadKey::I2c {
            device: 2,
            address: 64,
            label: "sdp810".to_string(),
        };
        assert_eq!(*s.reads.get(&key).unwrap(), 120.5);
    }

    #[tokio::test]
    async fn test_inject_mixed_types() {
        let state = test_state();
        let app = router(state.clone());

        let body = serde_json::json!({
            "points": [
                {"type": "analog", "device": 0, "address": 0, "value": 1800.0},
                {"type": "digital", "address": 45, "value": false},
                {"type": "pwm", "chip": 4, "channel": 0, "value": 0.5},
                {"type": "uart", "device": 1, "label": "co2", "value": 400.0}
            ]
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sim/inject")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let result: InjectResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result.injected, 4);

        let s = state.read().unwrap();
        assert_eq!(s.reads.len(), 3); // analog + pwm + uart (i2c not sent)
        assert_eq!(s.digital_reads.len(), 1);
    }

    #[tokio::test]
    async fn test_state_endpoint() {
        let state = test_state();
        // Pre-populate some state
        {
            let mut s = state.write().unwrap();
            s.reads.insert(
                ReadKey::Analog {
                    device: 0,
                    address: 0,
                },
                1.0,
            );
            s.reads.insert(
                ReadKey::Analog {
                    device: 0,
                    address: 1,
                },
                2.0,
            );
            s.digital_reads.insert(45, true);
        }

        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/sim/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let result: StateResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result.read_count, 2);
        assert_eq!(result.digital_read_count, 1);
        assert_eq!(result.write_count, 0);
    }

    #[tokio::test]
    async fn test_outputs_drains() {
        let state = test_state();
        // Simulate engine writes
        {
            let mut s = state.write().unwrap();
            s.writes.insert(WriteKey::Digital { address: 45 }, 1.0);
            s.writes.insert(
                WriteKey::Pwm {
                    chip: 4,
                    channel: 0,
                },
                0.75,
            );
        }

        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/sim/outputs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let result: OutputsResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result.outputs.len(), 2);

        // Verify drain: writes should now be empty
        let s = state.read().unwrap();
        assert_eq!(s.writes.len(), 0);
    }

    #[tokio::test]
    async fn test_inject_overwrites_existing() {
        let state = test_state();
        // Set initial value
        {
            let mut s = state.write().unwrap();
            s.reads.insert(
                ReadKey::Analog {
                    device: 0,
                    address: 0,
                },
                1000.0,
            );
        }

        let app = router(state.clone());
        let body = serde_json::json!({
            "points": [
                {"type": "analog", "device": 0, "address": 0, "value": 2000.0}
            ]
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sim/inject")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let s = state.read().unwrap();
        assert_eq!(
            *s.reads
                .get(&ReadKey::Analog {
                    device: 0,
                    address: 0
                })
                .unwrap(),
            2000.0
        );
    }

    #[tokio::test]
    async fn test_inject_empty_points() {
        let state = test_state();
        let app = router(state.clone());

        let body = serde_json::json!({ "points": [] });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sim/inject")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let result: InjectResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result.injected, 0);
    }

    #[tokio::test]
    async fn test_scenario_returns_started() {
        let state = test_state();
        let app = router(state.clone());

        let body = serde_json::json!({
            "steps": [
                {
                    "offset_ms": 0,
                    "points": [{"type": "analog", "device": 0, "address": 0, "value": 1800.0}]
                },
                {
                    "offset_ms": 100,
                    "points": [{"type": "analog", "device": 0, "address": 0, "value": 2000.0}]
                }
            ]
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sim/scenario")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let result: ScenarioResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result.status, "started");
        assert_eq!(result.steps, 2);
    }

    #[tokio::test]
    async fn test_scenario_executes_steps() {
        let state = test_state();
        let app = router(state.clone());

        let body = serde_json::json!({
            "steps": [
                {
                    "offset_ms": 0,
                    "points": [{"type": "analog", "device": 0, "address": 0, "value": 1800.0}]
                },
                {
                    "offset_ms": 50,
                    "points": [{"type": "analog", "device": 0, "address": 0, "value": 2200.0}]
                }
            ]
        });

        let _resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sim/scenario")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Wait for scenario to complete
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Last step should have set value to 2200
        let s = state.read().unwrap();
        assert_eq!(
            *s.reads
                .get(&ReadKey::Analog {
                    device: 0,
                    address: 0
                })
                .unwrap(),
            2200.0
        );
    }

    #[tokio::test]
    async fn test_bad_json_returns_error() {
        let state = test_state();
        let app = router(state.clone());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sim/inject")
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Axum returns 400 for deserialization failures
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
