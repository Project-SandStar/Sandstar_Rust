//! Configuration-driven control engine.
//!
//! Replaces the Sedona VM for simple HVAC control logic. Reads control
//! loop definitions from `control.toml` and executes PID + sequencing
//! against the engine's channel store. Also supports standalone components
//! (arithmetic, logic, timing, HVAC, scheduling) via `[[component]]` sections.

use std::path::Path;
use std::time::Instant;

use sandstar_engine::components::{
    Add2, And2, DailyScheduleBool, DailyScheduleFloat, DelayOff, DelayOn, Div2, FloatOffset,
    Hysteresis, Mul2, Neg, Not, OneShot, Or2, Ramp, Round, SRLatch, ScheduleEntry, Sub2,
    Thermostat,
};
use sandstar_engine::pid::PidController;
use sandstar_engine::sequencer::LeadSequencer;
use sandstar_engine::{Engine, EngineStatus};
use sandstar_hal::{HalDiagnostics, HalRead, HalWrite};
use serde::Deserialize;
use tracing::{debug, info, warn};

// ── TOML deserialization structs ─────────────────────────────

#[derive(Deserialize)]
struct ControlToml {
    #[serde(rename = "loop", default)]
    loops: Vec<LoopConfig>,
    #[serde(default)]
    component: Vec<ComponentConfig>,
}

#[derive(Deserialize)]
struct LoopConfig {
    name: String,
    feedback_channel: u32,
    setpoint_channel: u32,
    output_channels: Vec<u32>,
    #[serde(default = "default_write_level")]
    write_level: u8,
    pid: PidConfig,
    sequencer: Option<SeqConfig>,
    enable_query: Option<String>,
}

#[derive(Deserialize)]
struct PidConfig {
    #[serde(default = "default_kp")]
    kp: f64,
    #[serde(default)]
    ki: f64,
    #[serde(default)]
    kd: f64,
    #[serde(default)]
    min: f64,
    #[serde(default = "default_max")]
    max: f64,
    #[serde(default = "default_bias")]
    bias: f64,
    #[serde(default)]
    max_delta: f64,
    #[serde(default = "default_true")]
    direct: bool,
    #[serde(default = "default_interval")]
    exec_interval_ms: u64,
}

#[derive(Deserialize)]
struct SeqConfig {
    #[serde(default = "default_hysteresis")]
    hysteresis: f64,
}

#[derive(Deserialize)]
struct ComponentConfig {
    name: String,
    #[serde(rename = "type")]
    component_type: String,
    #[serde(default)]
    input_channels: Vec<u32>,
    output_channel: u32,
    #[serde(default = "default_write_level")]
    write_level: u8,
    // Type-specific fields.
    value: Option<f64>,
    min: Option<f64>,
    max: Option<f64>,
    period_ms: Option<u64>,
    delay_ms: Option<u64>,
    duration_ms: Option<u64>,
    setpoint: Option<f64>,
    deadband: Option<f64>,
    heating: Option<bool>,
    rising_threshold: Option<f64>,
    falling_threshold: Option<f64>,
    high_value: Option<f64>,
    low_value: Option<f64>,
    default_value: Option<f64>,
    entries: Option<Vec<ScheduleEntryConfig>>,
    bool_entries: Option<Vec<BoolScheduleEntryConfig>>,
    offset: Option<f64>,
    decimals: Option<u32>,
}

#[derive(Deserialize)]
struct ScheduleEntryConfig {
    hour: u8,
    minute: u8,
    value: f64,
}

#[derive(Deserialize)]
struct BoolScheduleEntryConfig {
    hour: u8,
    minute: u8,
    value: bool,
}

fn default_write_level() -> u8 {
    8
}
fn default_kp() -> f64 {
    1.0
}
fn default_max() -> f64 {
    100.0
}
fn default_bias() -> f64 {
    50.0
}
fn default_true() -> bool {
    true
}
fn default_interval() -> u64 {
    1000
}
fn default_hysteresis() -> f64 {
    0.5
}

// ── Public types ─────────────────────────────────────────────

/// A configured control loop (PID + optional sequencer).
#[derive(Debug)]
pub struct ControlLoop {
    /// Human-readable name.
    pub name: String,
    /// Channel ID for the process variable (sensor input).
    pub feedback_channel: u32,
    /// Channel ID for the setpoint (may be a virtual channel).
    pub setpoint_channel: u32,
    /// Output channel IDs (if sequencer is used, one per stage; otherwise single output).
    pub output_channels: Vec<u32>,
    /// Priority level for writing outputs (default 8, matching Sedona).
    pub write_level: u8,
    /// PID controller instance.
    pub pid: PidController,
    /// Optional lead sequencer (if multiple output stages).
    pub sequencer: Option<LeadSequencer>,
    /// Haystack filter query that must match for this loop to be enabled.
    /// If set, the loop is only active when matching channels exist.
    pub enable_query: Option<String>,
    /// Whether this loop is currently enabled.
    enabled: bool,
}

/// All supported component types.
#[derive(Debug)]
pub enum ComponentKind {
    Add2(Add2),
    Sub2(Sub2),
    Mul2(Mul2),
    Div2(Div2),
    Neg(Neg),
    Round(Round),
    FloatOffset(FloatOffset),
    ConstFloat(f64),
    Ramp(Ramp),
    Thermostat(Thermostat),
    Hysteresis(Hysteresis),
    DailyScheduleFloat(DailyScheduleFloat),
    DailyScheduleBool(DailyScheduleBool),
    DelayOn(DelayOn),
    DelayOff(DelayOff),
    OneShot(OneShot),
    And2(And2),
    Or2(Or2),
    Not(Not),
    SRLatch(SRLatch),
}

/// A standalone component instance wired to engine channels.
#[derive(Debug)]
pub struct ComponentInstance {
    pub name: String,
    pub input_channels: Vec<u32>,
    pub output_channel: u32,
    pub write_level: u8,
    pub kind: ComponentKind,
    pub enabled: bool,
}

/// The control runner -- holds all configured control loops and components.
#[derive(Debug)]
pub struct ControlRunner {
    pub loops: Vec<ControlLoop>,
    pub components: Vec<ComponentInstance>,
}

impl ControlRunner {
    /// Create an empty runner (no control loops or components).
    pub fn new() -> Self {
        Self {
            loops: Vec::new(),
            components: Vec::new(),
        }
    }

    /// Load control configuration from a TOML file.
    /// Returns `Ok(runner)` or `Err(message)`.
    pub fn load(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;

        let toml_cfg: ControlToml = toml::from_str(&content)
            .map_err(|e| format!("failed to parse {}: {}", path.display(), e))?;

        let mut loops = Vec::with_capacity(toml_cfg.loops.len());

        for lc in toml_cfg.loops {
            // Validate write level.
            if lc.write_level == 0 || lc.write_level > 17 {
                return Err(format!(
                    "loop '{}': write_level {} out of range 1..=17",
                    lc.name, lc.write_level
                ));
            }

            // Build PID controller from config.
            let mut pid = PidController::new();
            pid.kp = lc.pid.kp;
            pid.ki = lc.pid.ki;
            pid.kd = lc.pid.kd;
            pid.out_min = lc.pid.min;
            pid.out_max = lc.pid.max;
            pid.bias = lc.pid.bias;
            pid.max_delta = lc.pid.max_delta;
            pid.direct = lc.pid.direct;
            pid.exec_interval_ms = lc.pid.exec_interval_ms;

            // Build optional sequencer.
            let sequencer = lc.sequencer.map(|sc| {
                let num_stages = lc.output_channels.len();
                let mut seq = LeadSequencer::new(num_stages);
                seq.hysteresis = sc.hysteresis;
                seq
            });

            if lc.output_channels.is_empty() {
                warn!(
                    loop_name = %lc.name,
                    "control loop has no output channels, skipping"
                );
                continue;
            }

            loops.push(ControlLoop {
                name: lc.name,
                feedback_channel: lc.feedback_channel,
                setpoint_channel: lc.setpoint_channel,
                output_channels: lc.output_channels,
                write_level: lc.write_level,
                pid,
                sequencer,
                enable_query: lc.enable_query,
                enabled: true,
            });
        }

        // Build components.
        let mut components = Vec::with_capacity(toml_cfg.component.len());

        for cc in toml_cfg.component {
            if cc.write_level == 0 || cc.write_level > 17 {
                return Err(format!(
                    "component '{}': write_level {} out of range 1..=17",
                    cc.name, cc.write_level
                ));
            }

            let kind = build_component_kind(&cc)?;

            components.push(ComponentInstance {
                name: cc.name,
                input_channels: cc.input_channels,
                output_channel: cc.output_channel,
                write_level: cc.write_level,
                kind,
                enabled: true,
            });
        }

        info!(
            loops = loops.len(),
            components = components.len(),
            path = %path.display(),
            "control configuration loaded"
        );

        Ok(Self { loops, components })
    }

    /// Execute one control cycle against the engine.
    /// Called after `poll_update()` in the main loop.
    pub fn execute<H: HalRead + HalWrite + HalDiagnostics>(
        &mut self,
        engine: &mut Engine<H>,
        now: Instant,
    ) {
        // Execute control loops.
        for ctrl_loop in &mut self.loops {
            if !ctrl_loop.enabled {
                continue;
            }

            // 1. Read feedback (process variable) from engine.
            let pv = match engine.channel_read(ctrl_loop.feedback_channel) {
                Ok(val) if val.status == EngineStatus::Ok => val.cur,
                Ok(_) => {
                    debug!(
                        loop_name = %ctrl_loop.name,
                        channel = ctrl_loop.feedback_channel,
                        "feedback channel not ok, skipping cycle"
                    );
                    continue;
                }
                Err(e) => {
                    debug!(
                        loop_name = %ctrl_loop.name,
                        channel = ctrl_loop.feedback_channel,
                        error = %e,
                        "failed to read feedback channel"
                    );
                    continue;
                }
            };

            // 2. Read setpoint from engine.
            let sp = match engine.channel_read(ctrl_loop.setpoint_channel) {
                Ok(val) => val.cur,
                Err(e) => {
                    debug!(
                        loop_name = %ctrl_loop.name,
                        channel = ctrl_loop.setpoint_channel,
                        error = %e,
                        "failed to read setpoint channel"
                    );
                    continue;
                }
            };

            // 3. Execute PID.
            let pid_out = ctrl_loop.pid.execute(sp, pv, now);

            // 4. If sequencer, stage the outputs.
            if let Some(ref mut seq) = ctrl_loop.sequencer {
                let stages = seq.execute(pid_out);
                for (i, &channel) in ctrl_loop.output_channels.iter().enumerate() {
                    let value = if i < stages.len() && stages[i] {
                        1.0
                    } else {
                        0.0
                    };
                    if let Err(e) = engine.channel_write_level(
                        channel,
                        ctrl_loop.write_level,
                        Some(value),
                        "control",
                        0.0,
                    ) {
                        debug!(
                            loop_name = %ctrl_loop.name,
                            channel = channel,
                            error = %e,
                            "failed to write output channel"
                        );
                    }
                }
            } else if let Some(&channel) = ctrl_loop.output_channels.first() {
                // Single output: write PID output directly.
                if let Err(e) = engine.channel_write_level(
                    channel,
                    ctrl_loop.write_level,
                    Some(pid_out),
                    "control",
                    0.0,
                ) {
                    debug!(
                        loop_name = %ctrl_loop.name,
                        channel = channel,
                        error = %e,
                        "failed to write output channel"
                    );
                }
            }

            debug!(
                loop_name = %ctrl_loop.name,
                sp = sp,
                pv = pv,
                pid_out = pid_out,
                "control cycle"
            );
        }

        // Execute standalone components.
        for comp in &mut self.components {
            if !comp.enabled {
                continue;
            }

            // Read input channels.
            let inputs = read_input_channels(engine, &comp.input_channels, &comp.name);

            let output = execute_component(&mut comp.kind, &inputs, now);

            if let Some(value) = output {
                if let Err(e) = engine.channel_write_level(
                    comp.output_channel,
                    comp.write_level,
                    Some(value),
                    "control",
                    0.0,
                ) {
                    debug!(
                        component = %comp.name,
                        channel = comp.output_channel,
                        error = %e,
                        "failed to write component output"
                    );
                }
            }
        }
    }

    /// Number of configured loops.
    pub fn loop_count(&self) -> usize {
        self.loops.len()
    }

    /// Number of configured components.
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    /// Check if any loops or components are configured.
    pub fn is_empty(&self) -> bool {
        self.loops.is_empty() && self.components.is_empty()
    }

    /// Enable or disable a control loop by name.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        for ctrl_loop in &mut self.loops {
            if ctrl_loop.name == name {
                ctrl_loop.enabled = enabled;
                return true;
            }
        }
        for comp in &mut self.components {
            if comp.name == name {
                comp.enabled = enabled;
                return true;
            }
        }
        false
    }
}

impl Default for ControlRunner {
    fn default() -> Self {
        Self::new()
    }
}

// ── Helper functions ────────────────────────────────────────

/// Read input channel values from the engine, returning them as a Vec<f64>.
/// Missing or errored channels yield 0.0.
fn read_input_channels<H: HalRead + HalWrite + HalDiagnostics>(
    engine: &mut Engine<H>,
    channels: &[u32],
    comp_name: &str,
) -> Vec<f64> {
    channels
        .iter()
        .map(|&ch| match engine.channel_read(ch) {
            Ok(val) => val.cur,
            Err(e) => {
                debug!(
                    component = %comp_name,
                    channel = ch,
                    error = %e,
                    "failed to read component input channel"
                );
                0.0
            }
        })
        .collect()
}

/// Execute a component and return the output value (as f64).
/// Bool outputs are converted to 1.0 (true) / 0.0 (false).
fn execute_component(kind: &mut ComponentKind, inputs: &[f64], now: Instant) -> Option<f64> {
    match kind {
        ComponentKind::Add2(c) => {
            c.in1 = inputs.first().copied().unwrap_or(0.0);
            c.in2 = inputs.get(1).copied().unwrap_or(0.0);
            Some(c.execute())
        }
        ComponentKind::Sub2(c) => {
            c.in1 = inputs.first().copied().unwrap_or(0.0);
            c.in2 = inputs.get(1).copied().unwrap_or(0.0);
            Some(c.execute())
        }
        ComponentKind::Mul2(c) => {
            c.in1 = inputs.first().copied().unwrap_or(0.0);
            c.in2 = inputs.get(1).copied().unwrap_or(0.0);
            Some(c.execute())
        }
        ComponentKind::Div2(c) => {
            c.in1 = inputs.first().copied().unwrap_or(0.0);
            c.in2 = inputs.get(1).copied().unwrap_or(0.0);
            Some(c.execute())
        }
        ComponentKind::Neg(c) => {
            c.input = inputs.first().copied().unwrap_or(0.0);
            Some(c.execute())
        }
        ComponentKind::Round(c) => {
            c.input = inputs.first().copied().unwrap_or(0.0);
            Some(c.execute())
        }
        ComponentKind::FloatOffset(c) => {
            c.input = inputs.first().copied().unwrap_or(0.0);
            Some(c.execute())
        }
        ComponentKind::ConstFloat(val) => Some(*val),
        ComponentKind::Ramp(c) => Some(c.execute(now)),
        ComponentKind::Thermostat(c) => {
            let temp = inputs.first().copied().unwrap_or(0.0);
            let out = c.execute(temp);
            Some(if out { 1.0 } else { 0.0 })
        }
        ComponentKind::Hysteresis(c) => {
            let input = inputs.first().copied().unwrap_or(0.0);
            Some(c.execute(input))
        }
        ComponentKind::DailyScheduleFloat(c) => {
            // Use system time for scheduling (hour, minute from inputs or system clock).
            // If two input channels are provided, use them as (hour, minute).
            // Otherwise use system local time.
            let (hour, minute) = if inputs.len() >= 2 {
                (inputs[0] as u8, inputs[1] as u8)
            } else {
                let now_time = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let secs_in_day = now_time % 86400;
                (
                    (secs_in_day / 3600) as u8,
                    ((secs_in_day % 3600) / 60) as u8,
                )
            };
            Some(c.evaluate(hour, minute))
        }
        ComponentKind::DailyScheduleBool(c) => {
            let (hour, minute) = if inputs.len() >= 2 {
                (inputs[0] as u8, inputs[1] as u8)
            } else {
                let now_time = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let secs_in_day = now_time % 86400;
                (
                    (secs_in_day / 3600) as u8,
                    ((secs_in_day % 3600) / 60) as u8,
                )
            };
            let out = c.evaluate(hour, minute);
            Some(if out { 1.0 } else { 0.0 })
        }
        ComponentKind::DelayOn(c) => {
            c.input = inputs.first().copied().unwrap_or(0.0) != 0.0;
            let out = c.execute(now);
            Some(if out { 1.0 } else { 0.0 })
        }
        ComponentKind::DelayOff(c) => {
            c.input = inputs.first().copied().unwrap_or(0.0) != 0.0;
            let out = c.execute(now);
            Some(if out { 1.0 } else { 0.0 })
        }
        ComponentKind::OneShot(c) => {
            c.input = inputs.first().copied().unwrap_or(0.0) != 0.0;
            let out = c.execute(now);
            Some(if out { 1.0 } else { 0.0 })
        }
        ComponentKind::And2(c) => {
            c.in1 = inputs.first().copied().unwrap_or(0.0) != 0.0;
            c.in2 = inputs.get(1).copied().unwrap_or(0.0) != 0.0;
            let out = c.execute();
            Some(if out { 1.0 } else { 0.0 })
        }
        ComponentKind::Or2(c) => {
            c.in1 = inputs.first().copied().unwrap_or(0.0) != 0.0;
            c.in2 = inputs.get(1).copied().unwrap_or(0.0) != 0.0;
            let out = c.execute();
            Some(if out { 1.0 } else { 0.0 })
        }
        ComponentKind::Not(c) => {
            c.input = inputs.first().copied().unwrap_or(0.0) != 0.0;
            let out = c.execute();
            Some(if out { 1.0 } else { 0.0 })
        }
        ComponentKind::SRLatch(c) => {
            c.set = inputs.first().copied().unwrap_or(0.0) != 0.0;
            c.reset = inputs.get(1).copied().unwrap_or(0.0) != 0.0;
            let out = c.execute();
            Some(if out { 1.0 } else { 0.0 })
        }
    }
}

/// Build a ComponentKind from the TOML config.
fn build_component_kind(cc: &ComponentConfig) -> Result<ComponentKind, String> {
    match cc.component_type.as_str() {
        "add2" => Ok(ComponentKind::Add2(Add2::new())),
        "sub2" => Ok(ComponentKind::Sub2(Sub2::new())),
        "mul2" => Ok(ComponentKind::Mul2(Mul2::new())),
        "div2" => Ok(ComponentKind::Div2(Div2::new())),
        "neg" => Ok(ComponentKind::Neg(Neg::new())),
        "round" => {
            let mut r = Round::new();
            if let Some(d) = cc.decimals {
                r.decimals = d;
            }
            Ok(ComponentKind::Round(r))
        }
        "float_offset" => {
            let mut fo = FloatOffset::new();
            if let Some(o) = cc.offset {
                fo.offset = o;
            }
            Ok(ComponentKind::FloatOffset(fo))
        }
        "const_float" => {
            let val = cc.value.ok_or_else(|| {
                format!(
                    "component '{}': const_float requires 'value' field",
                    cc.name
                )
            })?;
            Ok(ComponentKind::ConstFloat(val))
        }
        "ramp" => {
            let mut r = Ramp::new();
            if let Some(min) = cc.min {
                r.min = min;
            }
            if let Some(max) = cc.max {
                r.max = max;
            }
            if let Some(p) = cc.period_ms {
                r.period_ms = p;
            }
            Ok(ComponentKind::Ramp(r))
        }
        "thermostat" => {
            let mut t = Thermostat::new();
            if let Some(sp) = cc.setpoint {
                t.setpoint = sp;
            }
            if let Some(db) = cc.deadband {
                t.deadband = db;
            }
            if let Some(h) = cc.heating {
                t.heating = h;
            }
            Ok(ComponentKind::Thermostat(t))
        }
        "hysteresis" => {
            let mut h = Hysteresis::new();
            if let Some(rt) = cc.rising_threshold {
                h.rising_threshold = rt;
            }
            if let Some(ft) = cc.falling_threshold {
                h.falling_threshold = ft;
            }
            if let Some(hv) = cc.high_value {
                h.high_value = hv;
            }
            if let Some(lv) = cc.low_value {
                h.low_value = lv;
            }
            Ok(ComponentKind::Hysteresis(h))
        }
        "daily_schedule_float" => {
            let mut sched = DailyScheduleFloat::new();
            if let Some(dv) = cc.default_value {
                sched.default_value = dv;
            }
            if let Some(ref entries) = cc.entries {
                sched.entries = entries
                    .iter()
                    .map(|e| ScheduleEntry {
                        hour: e.hour,
                        minute: e.minute,
                        value: e.value,
                    })
                    .collect();
            }
            Ok(ComponentKind::DailyScheduleFloat(sched))
        }
        "daily_schedule_bool" => {
            let mut sched = DailyScheduleBool::new();
            if let Some(dv) = cc.default_value {
                sched.default_value = dv != 0.0;
            }
            if let Some(ref entries) = cc.bool_entries {
                sched.entries = entries
                    .iter()
                    .map(|e| sandstar_engine::components::BoolScheduleEntry {
                        hour: e.hour,
                        minute: e.minute,
                        value: e.value,
                    })
                    .collect();
            }
            Ok(ComponentKind::DailyScheduleBool(sched))
        }
        "delay_on" => {
            let mut d = DelayOn::new();
            if let Some(ms) = cc.delay_ms {
                d.delay_ms = ms;
            }
            Ok(ComponentKind::DelayOn(d))
        }
        "delay_off" => {
            let mut d = DelayOff::new();
            if let Some(ms) = cc.delay_ms {
                d.delay_ms = ms;
            }
            Ok(ComponentKind::DelayOff(d))
        }
        "one_shot" => {
            let mut os = OneShot::new();
            if let Some(ms) = cc.duration_ms {
                os.duration_ms = ms;
            }
            Ok(ComponentKind::OneShot(os))
        }
        "and2" => Ok(ComponentKind::And2(And2::new())),
        "or2" => Ok(ComponentKind::Or2(Or2::new())),
        "not" => Ok(ComponentKind::Not(Not::new())),
        "sr_latch" => Ok(ComponentKind::SRLatch(SRLatch::new())),
        other => Err(format!("component '{}': unknown type '{}'", cc.name, other)),
    }
}

// ── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sandstar_engine::channel::{Channel, ChannelDirection, ChannelType};
    use sandstar_engine::value::ValueConv;
    use sandstar_hal::mock::MockHal;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;

    /// Helper to create an engine with specific channels.
    /// Each channel gets a unique address (= channel ID) for MockHal keying.
    fn make_engine(channels: &[(u32, ChannelDirection)]) -> Engine<MockHal> {
        let hal = MockHal::new();
        let mut engine = Engine::new(hal);
        for &(id, dir) in channels {
            let ch = Channel::new(
                id,
                ChannelType::Analog,
                dir,
                0,  // device
                id, // address = channel ID (unique per channel)
                false,
                ValueConv::default(),
                &format!("ch{}", id),
            );
            engine.channels.add(ch).unwrap();
            // Enable the channel.
            engine.channels.get_mut(id).unwrap().enabled = true;
        }
        engine
    }

    /// Pre-seed MockHal with a sticky analog value for a given channel.
    fn seed_hal_value(engine: &Engine<MockHal>, channel_id: u32, value: f64) {
        // Address matches channel ID (set in make_engine).
        engine.hal.set_analog(0, channel_id, Ok(value));
    }

    #[test]
    fn test_load_empty_config() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("control.toml");
        fs::write(&path, "# Empty control config\n").unwrap();

        let runner = ControlRunner::load(&path).unwrap();
        assert_eq!(runner.loop_count(), 0);
        assert_eq!(runner.component_count(), 0);
        assert!(runner.is_empty());
    }

    #[test]
    fn test_load_single_loop() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("control.toml");
        fs::write(
            &path,
            r#"
[[loop]]
name = "zone_cooling"
feedback_channel = 4
setpoint_channel = 75
output_channels = [35, 50, 53, 47]
write_level = 8

[loop.pid]
kp = 20.0
ki = 5.0
kd = 0.0
min = 0.0
max = 100.0
direct = true
exec_interval_ms = 1000

[loop.sequencer]
hysteresis = 0.5
"#,
        )
        .unwrap();

        let runner = ControlRunner::load(&path).unwrap();
        assert_eq!(runner.loop_count(), 1);

        let ctrl = &runner.loops[0];
        assert_eq!(ctrl.name, "zone_cooling");
        assert_eq!(ctrl.feedback_channel, 4);
        assert_eq!(ctrl.setpoint_channel, 75);
        assert_eq!(ctrl.output_channels, vec![35, 50, 53, 47]);
        assert_eq!(ctrl.write_level, 8);
        assert!((ctrl.pid.kp - 20.0).abs() < f64::EPSILON);
        assert!((ctrl.pid.ki - 5.0).abs() < f64::EPSILON);
        assert!(ctrl.sequencer.is_some());
        assert_eq!(ctrl.sequencer.as_ref().unwrap().num_stages, 4);
    }

    #[test]
    fn test_load_missing_file() {
        let result = ControlRunner::load(Path::new("/nonexistent/control.toml"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("failed to read"));
    }

    #[test]
    fn test_load_invalid_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("control.toml");
        fs::write(&path, "this is not valid toml {{{\n").unwrap();

        let result = ControlRunner::load(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("failed to parse"));
    }

    #[test]
    fn test_load_invalid_write_level() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("control.toml");
        fs::write(
            &path,
            r#"
[[loop]]
name = "bad_level"
feedback_channel = 1
setpoint_channel = 2
output_channels = [3]
write_level = 0

[loop.pid]
kp = 1.0
"#,
        )
        .unwrap();

        let result = ControlRunner::load(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("write_level"));
    }

    #[test]
    fn test_load_no_output_channels_skipped() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("control.toml");
        fs::write(
            &path,
            r#"
[[loop]]
name = "no_outputs"
feedback_channel = 1
setpoint_channel = 2
output_channels = []

[loop.pid]
kp = 1.0
"#,
        )
        .unwrap();

        let runner = ControlRunner::load(&path).unwrap();
        assert_eq!(
            runner.loop_count(),
            0,
            "loop with no outputs should be skipped"
        );
    }

    #[test]
    fn test_load_default_pid_values() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("control.toml");
        fs::write(
            &path,
            r#"
[[loop]]
name = "defaults"
feedback_channel = 1
setpoint_channel = 2
output_channels = [3]

[loop.pid]
"#,
        )
        .unwrap();

        let runner = ControlRunner::load(&path).unwrap();
        assert_eq!(runner.loop_count(), 1);

        let ctrl = &runner.loops[0];
        assert!((ctrl.pid.kp - 1.0).abs() < f64::EPSILON);
        assert!((ctrl.pid.ki - 0.0).abs() < f64::EPSILON);
        assert!((ctrl.pid.kd - 0.0).abs() < f64::EPSILON);
        assert!((ctrl.pid.out_min - 0.0).abs() < f64::EPSILON);
        assert!((ctrl.pid.out_max - 100.0).abs() < f64::EPSILON);
        assert!((ctrl.pid.bias - 50.0).abs() < f64::EPSILON);
        assert!(ctrl.pid.direct);
        assert_eq!(ctrl.pid.exec_interval_ms, 1000);
        assert_eq!(ctrl.write_level, 8);
        assert!(ctrl.sequencer.is_none());
    }

    #[test]
    fn test_execute_with_mock_engine() {
        // Create engine with channels: 4 (input/feedback), 75 (input/setpoint),
        // 35, 50, 53, 47 (outputs).
        let mut engine = make_engine(&[
            (4, ChannelDirection::In),
            (75, ChannelDirection::In),
            (35, ChannelDirection::Out),
            (50, ChannelDirection::Out),
            (53, ChannelDirection::Out),
            (47, ChannelDirection::Out),
        ]);

        // Seed MockHal so channel_read returns real values through the HAL pipeline.
        seed_hal_value(&engine, 4, 77.0); // feedback = 77F (zone temp)
        seed_hal_value(&engine, 75, 70.0); // setpoint = 70F

        // Create control runner with one loop.
        let mut runner = ControlRunner::new();
        let mut pid = PidController::new();
        pid.kp = 20.0;
        pid.ki = 5.0;
        pid.direct = true;

        let mut seq = LeadSequencer::new(4);
        seq.hysteresis = 0.5;

        runner.loops.push(ControlLoop {
            name: "test_cooling".to_string(),
            feedback_channel: 4,
            setpoint_channel: 75,
            output_channels: vec![35, 50, 53, 47],
            write_level: 8,
            pid,
            sequencer: Some(seq),
            enable_query: None,
            enabled: true,
        });

        // First execute: PID initializes.
        let t0 = Instant::now();
        runner.execute(&mut engine, t0);

        // Second execute: PID computes. With kp=20, error=70-77=-7, direct -> error=-7.
        // p = 20*(-7) = -140, clamped to 0. Sequencer at 0 -> all stages off.
        let t1 = t0 + Duration::from_millis(1000);
        runner.execute(&mut engine, t1);

        // Check output channels. Since error is negative (sp < pv) and direct=true,
        // PID output will be clamped to 0 (min). All stages should be off (value=0.0).
        // Outputs are written via channel_write_level, so check priority arrays.
        for &ch_id in &[35u32, 50, 53, 47] {
            let ch = engine.channels.get(ch_id).unwrap();
            if let Some(ref pa) = ch.priority_array {
                let levels = pa.levels();
                // Level 8 (index 7) should have been written.
                if let Some(val) = levels[7].value {
                    // PID output was 0 (clamped), so all stages off -> 0.0.
                    assert!(
                        val.abs() < f64::EPSILON,
                        "stage output should be 0.0 for ch {}, got {}",
                        ch_id,
                        val
                    );
                }
            }
        }
    }

    #[test]
    fn test_execute_skips_disabled_loop() {
        let mut engine = make_engine(&[
            (4, ChannelDirection::In),
            (75, ChannelDirection::In),
            (35, ChannelDirection::Out),
        ]);

        seed_hal_value(&engine, 4, 77.0);
        seed_hal_value(&engine, 75, 70.0);

        let mut runner = ControlRunner::new();
        let pid = PidController::new();

        runner.loops.push(ControlLoop {
            name: "disabled_loop".to_string(),
            feedback_channel: 4,
            setpoint_channel: 75,
            output_channels: vec![35],
            write_level: 8,
            pid,
            sequencer: None,
            enable_query: None,
            enabled: false,
        });

        let t0 = Instant::now();
        runner.execute(&mut engine, t0);

        // Output channel should have no priority array (never written).
        let ch35 = engine.channels.get(35).unwrap();
        assert!(
            ch35.priority_array.is_none(),
            "disabled loop should not write outputs"
        );
    }

    #[test]
    fn test_execute_skips_bad_feedback() {
        let mut engine = make_engine(&[
            (4, ChannelDirection::In),
            (75, ChannelDirection::In),
            (35, ChannelDirection::Out),
        ]);

        // Do NOT seed HAL for channel 4 -- it will fail with a HalError,
        // causing channel_read to return status Down.
        seed_hal_value(&engine, 75, 70.0);

        let mut runner = ControlRunner::new();
        let pid = PidController::new();

        runner.loops.push(ControlLoop {
            name: "bad_feedback".to_string(),
            feedback_channel: 4,
            setpoint_channel: 75,
            output_channels: vec![35],
            write_level: 8,
            pid,
            sequencer: None,
            enable_query: None,
            enabled: true,
        });

        let t0 = Instant::now();
        runner.execute(&mut engine, t0);

        // Output should not be written because feedback status is not Ok.
        let ch35 = engine.channels.get(35).unwrap();
        assert!(
            ch35.priority_array.is_none(),
            "should not write output when feedback status is not ok"
        );
    }

    #[test]
    fn test_set_enabled() {
        let mut runner = ControlRunner::new();
        let pid = PidController::new();

        runner.loops.push(ControlLoop {
            name: "test".to_string(),
            feedback_channel: 1,
            setpoint_channel: 2,
            output_channels: vec![3],
            write_level: 8,
            pid,
            sequencer: None,
            enable_query: None,
            enabled: true,
        });

        assert!(runner.loops[0].enabled);

        let found = runner.set_enabled("test", false);
        assert!(found);
        assert!(!runner.loops[0].enabled);

        let not_found = runner.set_enabled("nonexistent", false);
        assert!(!not_found);
    }

    #[test]
    fn test_default_trait() {
        let runner = ControlRunner::default();
        assert!(runner.is_empty());
        assert_eq!(runner.loop_count(), 0);
        assert_eq!(runner.component_count(), 0);
    }

    #[test]
    fn test_single_output_no_sequencer() {
        // Test a loop with a single output and no sequencer (PID output written directly).
        let mut engine = make_engine(&[
            (4, ChannelDirection::In),
            (75, ChannelDirection::In),
            (35, ChannelDirection::Out),
        ]);

        // Seed MockHal: pv=70 (zone temp), sp=75 (setpoint).
        seed_hal_value(&engine, 4, 70.0);
        seed_hal_value(&engine, 75, 75.0);

        let mut runner = ControlRunner::new();
        let mut pid = PidController::new();
        pid.kp = 2.0;
        pid.ki = 0.0;
        pid.bias = 50.0;
        pid.direct = true;

        runner.loops.push(ControlLoop {
            name: "single_out".to_string(),
            feedback_channel: 4,
            setpoint_channel: 75,
            output_channels: vec![35],
            write_level: 8,
            pid,
            sequencer: None,
            enable_query: None,
            enabled: true,
        });

        // Init call.
        let t0 = Instant::now();
        runner.execute(&mut engine, t0);

        // Compute call: error=75-70=5, p=2*5=10, output=10+50=60.
        let t1 = t0 + Duration::from_millis(1000);
        runner.execute(&mut engine, t1);

        // Check that channel 35 was written with the PID output.
        let ch35 = engine.channels.get(35).unwrap();
        assert!(
            ch35.priority_array.is_some(),
            "output should have been written"
        );
        let pa = ch35.priority_array.as_ref().unwrap();
        let levels = pa.levels();
        let written = levels[7].value; // level 8 -> index 7
        assert!(written.is_some(), "level 8 should have a value");
        let val = written.unwrap();
        assert!(
            (val - 60.0).abs() < 0.5,
            "PID output should be ~60.0, got {}",
            val
        );
    }

    // ── Component integration tests ─────────────────────────

    #[test]
    fn test_load_component_const_float() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("control.toml");
        fs::write(
            &path,
            r#"
[[component]]
name = "const_setpoint"
type = "const_float"
value = 70.0
output_channel = 75
write_level = 8
"#,
        )
        .unwrap();

        let runner = ControlRunner::load(&path).unwrap();
        assert_eq!(runner.loop_count(), 0);
        assert_eq!(runner.component_count(), 1);

        let comp = &runner.components[0];
        assert_eq!(comp.name, "const_setpoint");
        assert_eq!(comp.output_channel, 75);
        assert_eq!(comp.write_level, 8);
        assert!(
            matches!(comp.kind, ComponentKind::ConstFloat(v) if (v - 70.0).abs() < f64::EPSILON)
        );
    }

    #[test]
    fn test_load_component_div2() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("control.toml");
        fs::write(
            &path,
            r#"
[[component]]
name = "damper_calc"
type = "div2"
input_channels = [16, 10]
output_channel = 22
write_level = 8
"#,
        )
        .unwrap();

        let runner = ControlRunner::load(&path).unwrap();
        assert_eq!(runner.component_count(), 1);
        let comp = &runner.components[0];
        assert_eq!(comp.input_channels, vec![16, 10]);
        assert!(matches!(comp.kind, ComponentKind::Div2(_)));
    }

    #[test]
    fn test_load_component_ramp() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("control.toml");
        fs::write(
            &path,
            r#"
[[component]]
name = "ramp_test"
type = "ramp"
min = 50.0
max = 100.0
period_ms = 5000
output_channel = 30
write_level = 8
"#,
        )
        .unwrap();

        let runner = ControlRunner::load(&path).unwrap();
        assert_eq!(runner.component_count(), 1);
        assert!(matches!(runner.components[0].kind, ComponentKind::Ramp(_)));
    }

    #[test]
    fn test_load_component_schedule() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("control.toml");
        fs::write(
            &path,
            r#"
[[component]]
name = "cooling_schedule"
type = "daily_schedule_float"
default_value = 72.0
output_channel = 75
write_level = 8

[[component.entries]]
hour = 6
minute = 0
value = 70.0

[[component.entries]]
hour = 18
minute = 0
value = 76.0

[[component.entries]]
hour = 22
minute = 0
value = 72.0
"#,
        )
        .unwrap();

        let runner = ControlRunner::load(&path).unwrap();
        assert_eq!(runner.component_count(), 1);
        if let ComponentKind::DailyScheduleFloat(ref sched) = runner.components[0].kind {
            assert_eq!(sched.entries.len(), 3);
            assert!((sched.default_value - 72.0).abs() < f64::EPSILON);
        } else {
            panic!("expected DailyScheduleFloat");
        }
    }

    #[test]
    fn test_load_component_unknown_type() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("control.toml");
        fs::write(
            &path,
            r#"
[[component]]
name = "bad"
type = "unknown_type"
output_channel = 1
"#,
        )
        .unwrap();

        let result = ControlRunner::load(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown type"));
    }

    #[test]
    fn test_load_component_invalid_write_level() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("control.toml");
        fs::write(
            &path,
            r#"
[[component]]
name = "bad_level"
type = "const_float"
value = 1.0
output_channel = 1
write_level = 0
"#,
        )
        .unwrap();

        let result = ControlRunner::load(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("write_level"));
    }

    #[test]
    fn test_load_const_float_missing_value() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("control.toml");
        fs::write(
            &path,
            r#"
[[component]]
name = "bad_const"
type = "const_float"
output_channel = 1
"#,
        )
        .unwrap();

        let result = ControlRunner::load(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("requires 'value'"));
    }

    #[test]
    fn test_component_div2_integration() {
        // Div2 reads two channels and writes result to output channel.
        let mut engine = make_engine(&[
            (16, ChannelDirection::In),
            (10, ChannelDirection::In),
            (22, ChannelDirection::Out),
        ]);

        seed_hal_value(&engine, 16, 100.0);
        seed_hal_value(&engine, 10, 4.0);

        let mut runner = ControlRunner::new();
        runner.components.push(ComponentInstance {
            name: "damper_calc".to_string(),
            input_channels: vec![16, 10],
            output_channel: 22,
            write_level: 8,
            kind: ComponentKind::Div2(Div2::new()),
            enabled: true,
        });

        let t0 = Instant::now();
        runner.execute(&mut engine, t0);

        // 100.0 / 4.0 = 25.0
        let ch22 = engine.channels.get(22).unwrap();
        assert!(
            ch22.priority_array.is_some(),
            "output should have been written"
        );
        let pa = ch22.priority_array.as_ref().unwrap();
        let val = pa.levels()[7].value.unwrap();
        assert!(
            (val - 25.0).abs() < 0.001,
            "Div2 output should be 25.0, got {}",
            val
        );
    }

    #[test]
    fn test_component_ramp_integration() {
        let mut engine = make_engine(&[(30, ChannelDirection::Out)]);

        let mut ramp = Ramp::new();
        ramp.min = 0.0;
        ramp.max = 100.0;
        ramp.period_ms = 1000;

        let mut runner = ControlRunner::new();
        runner.components.push(ComponentInstance {
            name: "ramp_test".to_string(),
            input_channels: vec![],
            output_channel: 30,
            write_level: 8,
            kind: ComponentKind::Ramp(ramp),
            enabled: true,
        });

        // First call: initializes ramp at min.
        let t0 = Instant::now();
        runner.execute(&mut engine, t0);

        let ch30 = engine.channels.get(30).unwrap();
        let val0 = ch30.priority_array.as_ref().unwrap().levels()[7]
            .value
            .unwrap();
        assert!(val0.abs() < 0.001, "ramp should start at 0.0, got {}", val0);

        // At 500ms: should be ~50.
        let t1 = t0 + Duration::from_millis(500);
        runner.execute(&mut engine, t1);

        let ch30 = engine.channels.get(30).unwrap();
        let val1 = ch30.priority_array.as_ref().unwrap().levels()[7]
            .value
            .unwrap();
        assert!(
            (val1 - 50.0).abs() < 1.0,
            "ramp should be ~50.0 at 500ms, got {}",
            val1
        );
    }

    #[test]
    fn test_component_schedule_integration() {
        let mut engine = make_engine(&[
            (100, ChannelDirection::In), // hour channel
            (101, ChannelDirection::In), // minute channel
            (75, ChannelDirection::Out), // output
        ]);

        // Simulate time 12:00 via input channels.
        seed_hal_value(&engine, 100, 12.0);
        seed_hal_value(&engine, 101, 0.0);

        let mut sched = DailyScheduleFloat::new();
        sched.default_value = 72.0;
        sched.entries = vec![
            ScheduleEntry {
                hour: 6,
                minute: 0,
                value: 70.0,
            },
            ScheduleEntry {
                hour: 18,
                minute: 0,
                value: 76.0,
            },
        ];

        let mut runner = ControlRunner::new();
        runner.components.push(ComponentInstance {
            name: "schedule".to_string(),
            input_channels: vec![100, 101],
            output_channel: 75,
            write_level: 8,
            kind: ComponentKind::DailyScheduleFloat(sched),
            enabled: true,
        });

        let t0 = Instant::now();
        runner.execute(&mut engine, t0);

        // At 12:00, the 06:00 entry (70.0) is the latest that has passed.
        let ch75 = engine.channels.get(75).unwrap();
        let val = ch75.priority_array.as_ref().unwrap().levels()[7]
            .value
            .unwrap();
        assert!(
            (val - 70.0).abs() < 0.001,
            "schedule at 12:00 should output 70.0, got {}",
            val
        );
    }

    #[test]
    fn test_mixed_loops_and_components() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("control.toml");
        fs::write(
            &path,
            r#"
[[loop]]
name = "zone_cooling"
feedback_channel = 4
setpoint_channel = 75
output_channels = [35]
write_level = 8

[loop.pid]
kp = 1.0

[[component]]
name = "const_sp"
type = "const_float"
value = 72.0
output_channel = 76
write_level = 8

[[component]]
name = "adder"
type = "add2"
input_channels = [1, 2]
output_channel = 3
write_level = 8
"#,
        )
        .unwrap();

        let runner = ControlRunner::load(&path).unwrap();
        assert_eq!(runner.loop_count(), 1);
        assert_eq!(runner.component_count(), 2);
        assert!(!runner.is_empty());
    }

    #[test]
    fn test_set_enabled_component() {
        let mut runner = ControlRunner::new();
        runner.components.push(ComponentInstance {
            name: "test_comp".to_string(),
            input_channels: vec![],
            output_channel: 1,
            write_level: 8,
            kind: ComponentKind::ConstFloat(42.0),
            enabled: true,
        });

        assert!(runner.components[0].enabled);
        let found = runner.set_enabled("test_comp", false);
        assert!(found);
        assert!(!runner.components[0].enabled);
    }

    #[test]
    fn test_component_disabled_skips_execution() {
        let mut engine = make_engine(&[(1, ChannelDirection::Out)]);

        let mut runner = ControlRunner::new();
        runner.components.push(ComponentInstance {
            name: "disabled".to_string(),
            input_channels: vec![],
            output_channel: 1,
            write_level: 8,
            kind: ComponentKind::ConstFloat(99.0),
            enabled: false,
        });

        let t0 = Instant::now();
        runner.execute(&mut engine, t0);

        let ch1 = engine.channels.get(1).unwrap();
        assert!(
            ch1.priority_array.is_none(),
            "disabled component should not write output"
        );
    }

    #[test]
    fn test_component_const_float_execution() {
        let mut engine = make_engine(&[(75, ChannelDirection::Out)]);

        let mut runner = ControlRunner::new();
        runner.components.push(ComponentInstance {
            name: "const_sp".to_string(),
            input_channels: vec![],
            output_channel: 75,
            write_level: 8,
            kind: ComponentKind::ConstFloat(70.0),
            enabled: true,
        });

        let t0 = Instant::now();
        runner.execute(&mut engine, t0);

        let ch75 = engine.channels.get(75).unwrap();
        assert!(ch75.priority_array.is_some());
        let val = ch75.priority_array.as_ref().unwrap().levels()[7]
            .value
            .unwrap();
        assert!(
            (val - 70.0).abs() < f64::EPSILON,
            "const_float should output 70.0, got {}",
            val
        );
    }

    #[test]
    fn test_component_add2_execution() {
        let mut engine = make_engine(&[
            (1, ChannelDirection::In),
            (2, ChannelDirection::In),
            (3, ChannelDirection::Out),
        ]);

        seed_hal_value(&engine, 1, 30.0);
        seed_hal_value(&engine, 2, 12.5);

        let mut runner = ControlRunner::new();
        runner.components.push(ComponentInstance {
            name: "adder".to_string(),
            input_channels: vec![1, 2],
            output_channel: 3,
            write_level: 8,
            kind: ComponentKind::Add2(Add2::new()),
            enabled: true,
        });

        let t0 = Instant::now();
        runner.execute(&mut engine, t0);

        let ch3 = engine.channels.get(3).unwrap();
        let val = ch3.priority_array.as_ref().unwrap().levels()[7]
            .value
            .unwrap();
        assert!(
            (val - 42.5).abs() < 0.001,
            "add2 should output 42.5, got {}",
            val
        );
    }

    #[test]
    fn test_component_thermostat_execution() {
        let mut engine = make_engine(&[(4, ChannelDirection::In), (90, ChannelDirection::Out)]);

        seed_hal_value(&engine, 4, 68.0); // below setpoint - deadband/2

        let mut therm = Thermostat::new();
        therm.setpoint = 72.0;
        therm.deadband = 2.0;
        therm.heating = true;

        let mut runner = ControlRunner::new();
        runner.components.push(ComponentInstance {
            name: "heat".to_string(),
            input_channels: vec![4],
            output_channel: 90,
            write_level: 8,
            kind: ComponentKind::Thermostat(therm),
            enabled: true,
        });

        let t0 = Instant::now();
        runner.execute(&mut engine, t0);

        // 68.0 < 71.0 (setpoint - deadband/2), so heating should be on (1.0).
        let ch90 = engine.channels.get(90).unwrap();
        let val = ch90.priority_array.as_ref().unwrap().levels()[7]
            .value
            .unwrap();
        assert!(
            (val - 1.0).abs() < f64::EPSILON,
            "thermostat should output 1.0 (heating on), got {}",
            val
        );
    }

    #[test]
    fn test_load_all_component_types() {
        // Verify all component types can be loaded from TOML.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("control.toml");
        fs::write(
            &path,
            r#"
[[component]]
name = "c_add2"
type = "add2"
input_channels = [1, 2]
output_channel = 100

[[component]]
name = "c_sub2"
type = "sub2"
input_channels = [1, 2]
output_channel = 101

[[component]]
name = "c_mul2"
type = "mul2"
input_channels = [1, 2]
output_channel = 102

[[component]]
name = "c_div2"
type = "div2"
input_channels = [1, 2]
output_channel = 103

[[component]]
name = "c_neg"
type = "neg"
input_channels = [1]
output_channel = 104

[[component]]
name = "c_round"
type = "round"
input_channels = [1]
output_channel = 105
decimals = 2

[[component]]
name = "c_offset"
type = "float_offset"
input_channels = [1]
output_channel = 106
offset = 5.0

[[component]]
name = "c_const"
type = "const_float"
value = 42.0
output_channel = 107

[[component]]
name = "c_ramp"
type = "ramp"
min = 10.0
max = 90.0
period_ms = 2000
output_channel = 108

[[component]]
name = "c_therm"
type = "thermostat"
input_channels = [1]
output_channel = 109
setpoint = 72.0
deadband = 2.0
heating = true

[[component]]
name = "c_hyst"
type = "hysteresis"
input_channels = [1]
output_channel = 110
rising_threshold = 80.0
falling_threshold = 70.0
high_value = 100.0
low_value = 0.0

[[component]]
name = "c_delay_on"
type = "delay_on"
input_channels = [1]
output_channel = 111
delay_ms = 500

[[component]]
name = "c_delay_off"
type = "delay_off"
input_channels = [1]
output_channel = 112
delay_ms = 500

[[component]]
name = "c_oneshot"
type = "one_shot"
input_channels = [1]
output_channel = 113
duration_ms = 200

[[component]]
name = "c_and2"
type = "and2"
input_channels = [1, 2]
output_channel = 114

[[component]]
name = "c_or2"
type = "or2"
input_channels = [1, 2]
output_channel = 115

[[component]]
name = "c_not"
type = "not"
input_channels = [1]
output_channel = 116

[[component]]
name = "c_latch"
type = "sr_latch"
input_channels = [1, 2]
output_channel = 117
"#,
        )
        .unwrap();

        let runner = ControlRunner::load(&path).unwrap();
        assert_eq!(runner.component_count(), 18);
    }
}
