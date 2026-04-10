//! Sedona `.sax` XML to `control.toml` converter.
//!
//! Parses a Sedona application XML file and emits an equivalent TOML
//! configuration that the Rust [`ControlRunner`](crate::control::ControlRunner)
//! can load directly. The converter understands PID loops (`control::LP`),
//! lead sequencers (`hvac::LSeq`), analog/bool I/O components, and
//! standalone math/control blocks.

use std::collections::HashMap;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

// ── Parsed SAX types ─────────────────────────────────────────

/// A single Sedona component parsed from a `<comp>` element.
#[derive(Debug, Clone)]
struct SaxComponent {
    id: u32,
    /// Full slash-separated path, e.g. `/cool/LP`.
    path: String,
    /// Sedona type, e.g. `control::LP`, `hvac::LSeq`.
    comp_type: String,
    /// Property bag: `<prop name="kp" val="20.0"/>` → `{"kp": "20.0"}`.
    props: HashMap<String, String>,
}

/// A link from one component slot to another.
#[derive(Debug, Clone)]
struct SaxLink {
    from_path: String,
    from_slot: String,
    to_path: String,
    to_slot: String,
}

/// Top-level converter state.
pub struct SaxConverter {
    components: HashMap<String, SaxComponent>,
    links: Vec<SaxLink>,
    warnings: Vec<String>,
}

/// Result of a conversion — the TOML text plus any warnings.
pub struct ConversionResult {
    pub toml: String,
    pub warnings: Vec<String>,
}

// ── Priority-from-slot helper ────────────────────────────────

/// Extract a BACnet priority level from a slot name.
///
/// `"in10"` → 10, `"in16"` → 16, `"in"` → 8 (default).
fn priority_from_slot(slot: &str) -> u8 {
    if let Some(digits) = slot.strip_prefix("in") {
        if digits.is_empty() {
            return 8;
        }
        digits.parse::<u8>().unwrap_or(8)
    } else {
        8
    }
}

// ── Parsing ──────────────────────────────────────────────────

impl Default for SaxConverter {
    fn default() -> Self {
        Self::new()
    }
}

impl SaxConverter {
    /// Create an empty converter.
    pub fn new() -> Self {
        Self {
            components: HashMap::new(),
            links: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Parse a `.sax` file from disk.
    pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let xml = std::fs::read_to_string(path.as_ref())
            .map_err(|e| format!("failed to read {}: {e}", path.as_ref().display()))?;
        Self::parse_str(&xml)
    }

    /// Parse a `.sax` string.
    pub fn parse_str(xml: &str) -> Result<Self, String> {
        let mut conv = SaxConverter::new();
        let mut reader = Reader::from_str(xml);

        // Stack of (name, path) for tracking nesting.
        let mut path_stack: Vec<String> = Vec::new();
        // Track the current <comp> being built at each nesting depth.
        // We store None on the stack if the element is not a <comp>.
        let mut comp_stack: Vec<Option<SaxComponent>> = Vec::new();

        loop {
            match reader.read_event() {
                Ok(Event::Start(ref e)) => {
                    let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();

                    if tag == "links" {
                        continue;
                    }

                    if tag == "comp" {
                        let attrs = Self::parse_attrs(e);
                        let comp_name = attrs.get("name").cloned().unwrap_or_default();
                        let id: u32 = attrs.get("id").and_then(|v| v.parse().ok()).unwrap_or(0);
                        let comp_type = attrs.get("type").cloned().unwrap_or_default();

                        let full_path = if path_stack.is_empty() {
                            format!("/{comp_name}")
                        } else {
                            let parent = path_stack.last().unwrap();
                            format!("{parent}/{comp_name}")
                        };

                        path_stack.push(full_path.clone());
                        comp_stack.push(Some(SaxComponent {
                            id,
                            path: full_path,
                            comp_type,
                            props: HashMap::new(),
                        }));
                    } else {
                        // Not a comp — push a None sentinel so End pops correctly.
                        // Actually only push for tags that are self-closing or have
                        // children we care about. We handle <prop> and <link> as
                        // Empty events below. Push nothing for other Start tags.
                    }
                }

                Ok(Event::Empty(ref e)) => {
                    let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();

                    if tag == "prop" && !comp_stack.is_empty() {
                        let attrs = Self::parse_attrs(e);
                        if let (Some(name), Some(val)) = (attrs.get("name"), attrs.get("val")) {
                            if let Some(Some(comp)) = comp_stack.last_mut() {
                                comp.props.insert(name.clone(), val.clone());
                            }
                        }
                    } else if tag == "link" {
                        let attrs = Self::parse_attrs(e);
                        if let (Some(from), Some(to)) = (attrs.get("from"), attrs.get("to")) {
                            if let (Some(link_from), Some(link_to)) =
                                (Self::split_slot(from), Self::split_slot(to))
                            {
                                conv.links.push(SaxLink {
                                    from_path: link_from.0,
                                    from_slot: link_from.1,
                                    to_path: link_to.0,
                                    to_slot: link_to.1,
                                });
                            }
                        }
                    } else if tag == "comp" {
                        // Self-closing <comp ... /> — treat as leaf.
                        let attrs = Self::parse_attrs(e);
                        let comp_name = attrs.get("name").cloned().unwrap_or_default();
                        let id: u32 = attrs.get("id").and_then(|v| v.parse().ok()).unwrap_or(0);
                        let comp_type = attrs.get("type").cloned().unwrap_or_default();

                        let full_path = if path_stack.is_empty() {
                            format!("/{comp_name}")
                        } else {
                            let parent = path_stack.last().unwrap();
                            format!("{parent}/{comp_name}")
                        };

                        conv.components.insert(
                            full_path.clone(),
                            SaxComponent {
                                id,
                                path: full_path,
                                comp_type,
                                props: HashMap::new(),
                            },
                        );
                    }
                }

                Ok(Event::End(ref e)) => {
                    let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();

                    if tag == "links" {
                        continue;
                    }

                    if tag == "comp" {
                        // Pop the component we were building.
                        if let Some(Some(comp)) = comp_stack.pop() {
                            conv.components.insert(comp.path.clone(), comp);
                        }
                        path_stack.pop();
                    }
                }

                Ok(Event::Eof) => break,
                Err(e) => return Err(format!("XML parse error: {e}")),
                _ => {}
            }
        }

        Ok(conv)
    }

    /// Helper: parse XML attributes into a HashMap.
    fn parse_attrs(e: &quick_xml::events::BytesStart) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for attr in e.attributes().flatten() {
            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
            let val = String::from_utf8_lossy(&attr.value).to_string();
            map.insert(key, val);
        }
        map
    }

    /// Split `"/cool/LP.out"` into `("/cool/LP", "out")`.
    fn split_slot(s: &str) -> Option<(String, String)> {
        let dot = s.rfind('.')?;
        Some((s[..dot].to_string(), s[dot + 1..].to_string()))
    }

    // ── Link helpers ─────────────────────────────────────────

    /// Find the link whose `to_path == path` and `to_slot == slot`.
    fn find_link_to(&self, path: &str, slot: &str) -> Option<&SaxLink> {
        self.links
            .iter()
            .find(|l| l.to_path == path && l.to_slot == slot)
    }

    /// Find all links whose `from_path == path` and `from_slot == slot`.
    fn find_links_from(&self, path: &str, slot: &str) -> Vec<&SaxLink> {
        self.links
            .iter()
            .filter(|l| l.from_path == path && l.from_slot == slot)
            .collect()
    }

    /// Follow a link chain backward from (path, slot), skipping through
    /// `control::WriteFloat` passthroughs.
    ///
    /// Returns the ultimate (source_path, source_slot).
    fn follow_link_backward(&self, path: &str, slot: &str) -> Option<(String, String)> {
        self.follow_link_backward_depth(path, slot, 0)
    }

    fn follow_link_backward_depth(
        &self,
        path: &str,
        slot: &str,
        depth: usize,
    ) -> Option<(String, String)> {
        if depth > 20 {
            return None; // prevent infinite loops
        }
        let link = self.find_link_to(path, slot)?;
        let source_path = &link.from_path;
        let source_slot = &link.from_slot;

        // If the source is a WriteFloat, follow through it.
        if let Some(comp) = self.components.get(source_path.as_str()) {
            if comp.comp_type == "control::WriteFloat" && source_slot == "out" {
                // Follow the WriteFloat's input.
                if let Some(result) = self.follow_link_backward_depth(source_path, "in", depth + 1)
                {
                    return Some(result);
                }
            }
        }

        Some((source_path.clone(), source_slot.clone()))
    }

    /// Follow a link chain forward from (path, slot), skipping through
    /// `control::WriteFloat` passthroughs.
    fn follow_link_forward(&self, path: &str, slot: &str) -> Vec<(String, String)> {
        self.follow_link_forward_depth(path, slot, 0)
    }

    fn follow_link_forward_depth(
        &self,
        path: &str,
        slot: &str,
        depth: usize,
    ) -> Vec<(String, String)> {
        if depth > 20 {
            return Vec::new();
        }
        let targets = self.find_links_from(path, slot);
        let mut results = Vec::new();

        for link in targets {
            let target_path = &link.to_path;
            let target_slot = &link.to_slot;

            if let Some(comp) = self.components.get(target_path.as_str()) {
                if comp.comp_type == "control::WriteFloat" && target_slot == "in" {
                    // Follow through the WriteFloat's output.
                    let mut fwd = self.follow_link_forward_depth(target_path, "out", depth + 1);
                    results.append(&mut fwd);
                    continue;
                }
            }

            results.push((target_path.clone(), target_slot.clone()));
        }

        results
    }

    // ── Conversion ───────────────────────────────────────────

    /// Convert the parsed SAX into a TOML string.
    pub fn convert(&mut self) -> ConversionResult {
        let mut out = String::new();
        out.push_str("# Auto-generated from Sedona .sax by sandstar sax-converter\n");
        out.push_str("# Review and adjust before deploying.\n\n");

        // Track which components have been consumed by a loop or as
        // upstream/downstream of a loop.
        let mut consumed: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Skip all /service/* components.
        for path in self.components.keys() {
            if path.starts_with("/service") {
                consumed.insert(path.clone());
            }
        }

        // 1) Detect PID loops (control::LP).
        let lp_paths: Vec<String> = self
            .components
            .values()
            .filter(|c| c.comp_type == "control::LP")
            .map(|c| c.path.clone())
            .collect();

        for lp_path in &lp_paths {
            if let Some(section) = self.build_loop_section(lp_path, &mut consumed) {
                out.push_str(&section);
                out.push('\n');
            }
        }

        // 2) Detect standalone components.
        let standalone_types = [
            "control::ConstFloat",
            "math::Add2",
            "math::Sub2",
            "math::Mul2",
            "math::Div2",
            "control::Ramp",
            "math::Neg",
            "control::ConstBool",
        ];

        // Collect paths first, then iterate.
        let standalone_paths: Vec<String> = self
            .components
            .values()
            .filter(|c| standalone_types.contains(&c.comp_type.as_str()))
            .filter(|c| !consumed.contains(&c.path))
            .map(|c| c.path.clone())
            .collect();

        for path in &standalone_paths {
            if let Some(section) = self.build_component_section(path, &mut consumed) {
                out.push_str(&section);
                out.push('\n');
            }
        }

        // 3) Emit warnings for unsupported components.
        for comp in self.components.values() {
            if consumed.contains(&comp.path) {
                continue;
            }
            if comp.comp_type.starts_with("shaystack::") {
                self.warnings.push(format!(
                    "WARNING: shaystack component {} ({}) not supported",
                    comp.path, comp.comp_type
                ));
            } else if comp.comp_type == "EacIo::ExposeTags" {
                self.warnings.push(format!(
                    "INFO: ExposeTags {} skipped (tag management)",
                    comp.path
                ));
            } else if comp.comp_type == "EacIo::RecordCount" {
                self.warnings.push(format!(
                    "INFO: RecordCount {} skipped (enable_query in loop)",
                    comp.path
                ));
            } else if comp.comp_type == "EacIo::CycleFolder"
                || comp.comp_type == "sys::Folder"
                || comp.comp_type.starts_with("sys::")
                || comp.comp_type.starts_with("sox::")
                || comp.comp_type.starts_with("web::")
                || comp.comp_type.starts_with("platUnix::")
                || comp.comp_type.starts_with("inet::")
            {
                // Folder / service types — silently skip.
            } else if comp.comp_type == "control::WriteFloat" {
                // WriteFloat passthroughs are consumed by link following — skip.
            } else if comp.comp_type == "EacIo::AnalogInput"
                || comp.comp_type == "EacIo::AnalogOutput"
                || comp.comp_type == "EacIo::AnalogValue"
                || comp.comp_type == "EacIo::BoolOutput"
                || comp.comp_type == "EacIo::BinaryValue"
            {
                // I/O components not consumed by a loop — info only.
                if !consumed.contains(&comp.path) {
                    self.warnings.push(format!(
                        "INFO: I/O component {} ({}) not part of a loop/component chain",
                        comp.path, comp.comp_type
                    ));
                }
            } else if !consumed.contains(&comp.path) {
                self.warnings.push(format!(
                    "INFO: unhandled component {} ({})",
                    comp.path, comp.comp_type
                ));
            }
        }

        // Prepend warnings as comments.
        if !self.warnings.is_empty() {
            let mut header = String::new();
            for w in &self.warnings {
                header.push_str(&format!("# {w}\n"));
            }
            header.push('\n');
            out = header + &out;
        }

        ConversionResult {
            toml: out,
            warnings: self.warnings.clone(),
        }
    }

    /// Build a `[[loop]]` TOML section for a `control::LP` component.
    fn build_loop_section(
        &self,
        lp_path: &str,
        consumed: &mut std::collections::HashSet<String>,
    ) -> Option<String> {
        let lp = self.components.get(lp_path)?;
        consumed.insert(lp_path.to_string());

        let kp: f64 = lp
            .props
            .get("kp")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0);
        let ki: f64 = lp
            .props
            .get("ki")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0);
        let kd: f64 = lp
            .props
            .get("kd")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0);

        // Follow .cv input backward to find feedback channel.
        let feedback_channel =
            self.follow_link_backward(lp_path, "cv")
                .and_then(|(src_path, _)| {
                    let comp = self.components.get(&src_path)?;
                    if comp.comp_type == "EacIo::AnalogInput" {
                        consumed.insert(src_path.clone());
                        comp.props
                            .get("channel")
                            .and_then(|v| v.parse::<u32>().ok())
                    } else {
                        None
                    }
                });

        // Follow .sp input backward to find setpoint channel.
        let (setpoint_channel, _sp_write_level) = self
            .follow_link_backward(lp_path, "sp")
            .and_then(|(src_path, _src_slot)| {
                let comp = self.components.get(&src_path)?;
                if comp.comp_type == "EacIo::AnalogValue" {
                    consumed.insert(src_path.clone());
                    let ch = comp
                        .props
                        .get("virtualCh")
                        .and_then(|v| v.parse::<u32>().ok());
                    // Find the slot that writes to this AnalogValue to get write_level.
                    let wl = Self::detect_write_level_from_props(comp);
                    Some((ch, wl))
                } else {
                    None
                }
            })
            .unwrap_or((None, 8));

        // Follow .out forward to find LSeq or direct outputs.
        let fwd_out = self.follow_link_forward(lp_path, "out");

        let mut output_channels: Vec<u32> = Vec::new();
        let mut write_level: u8 = 8;
        let mut hysteresis: Option<f64> = None;

        for (target_path, target_slot) in &fwd_out {
            if let Some(comp) = self.components.get(target_path.as_str()) {
                if comp.comp_type == "hvac::LSeq" && target_slot == "in" {
                    consumed.insert(target_path.clone());
                    // Parse delta for hysteresis.
                    let delta: f64 = comp
                        .props
                        .get("delta")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(20.0);
                    // LSeq hysteresis is half the stage width. We approximate.
                    hysteresis = Some(delta / 100.0 * 2.5);

                    // Follow LSeq outputs to BoolOutput channels.
                    let num_outs: usize = comp
                        .props
                        .get("numOuts")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(4);

                    for i in 1..=num_outs {
                        let slot = format!("out{i}");
                        let targets = self.follow_link_forward(target_path, &slot);
                        for (bo_path, bo_slot) in &targets {
                            if let Some(bo_comp) = self.components.get(bo_path.as_str()) {
                                if bo_comp.comp_type == "EacIo::BoolOutput" {
                                    consumed.insert(bo_path.clone());
                                    if let Some(ch) = bo_comp
                                        .props
                                        .get("channel")
                                        .and_then(|v| v.parse::<u32>().ok())
                                    {
                                        output_channels.push(ch);
                                    }
                                    write_level = priority_from_slot(bo_slot);
                                }
                            }
                        }
                    }
                } else if comp.comp_type == "EacIo::BoolOutput" {
                    // Direct LP → BoolOutput (no LSeq).
                    consumed.insert(target_path.clone());
                    if let Some(ch) = comp
                        .props
                        .get("channel")
                        .and_then(|v| v.parse::<u32>().ok())
                    {
                        output_channels.push(ch);
                    }
                    write_level = priority_from_slot(target_slot);
                } else if comp.comp_type == "EacIo::AnalogOutput" {
                    // Direct LP → AnalogOutput.
                    consumed.insert(target_path.clone());
                    if let Some(ch) = comp
                        .props
                        .get("channel")
                        .and_then(|v| v.parse::<u32>().ok())
                    {
                        output_channels.push(ch);
                    }
                    write_level = priority_from_slot(target_slot);
                }
            }
        }

        // Build name from path: /cool/LP → cool_lp
        let loop_name = lp_path
            .trim_start_matches('/')
            .replace('/', "_")
            .to_lowercase();

        let fb = feedback_channel.unwrap_or(0);
        let sp = setpoint_channel.unwrap_or(0);

        let mut section = String::new();
        section.push_str(&format!("# Converted from {} (id={})\n", lp.path, lp.id));
        section.push_str("[[loop]]\n");
        section.push_str(&format!("name = \"{loop_name}\"\n"));
        section.push_str(&format!("feedback_channel = {fb}\n"));
        section.push_str(&format!("setpoint_channel = {sp}\n"));
        section.push_str(&format!(
            "output_channels = [{}]\n",
            output_channels
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        section.push_str(&format!("write_level = {write_level}\n"));
        section.push('\n');
        section.push_str("[loop.pid]\n");
        section.push_str(&format!("kp = {kp}\n"));
        section.push_str(&format!("ki = {ki}\n"));
        section.push_str(&format!("kd = {kd}\n"));
        section.push_str("min = 0.0\n");
        section.push_str("max = 100.0\n");
        section.push_str("direct = true\n");
        section.push_str("exec_interval_ms = 1000\n");

        if hysteresis.is_some() || !output_channels.is_empty() {
            section.push('\n');
            section.push_str("[loop.sequencer]\n");
            section.push_str(&format!("hysteresis = {:.1}\n", hysteresis.unwrap_or(0.5)));
        }

        Some(section)
    }

    /// Build a `[[component]]` TOML section for a standalone component.
    fn build_component_section(
        &self,
        path: &str,
        consumed: &mut std::collections::HashSet<String>,
    ) -> Option<String> {
        let comp = self.components.get(path)?;
        consumed.insert(path.to_string());

        let comp_name = path
            .trim_start_matches('/')
            .replace('/', "_")
            .to_lowercase();

        let mut section = String::new();
        section.push_str(&format!(
            "# Converted from {} (id={})\n",
            comp.path, comp.id
        ));
        section.push_str("[[component]]\n");
        section.push_str(&format!("name = \"{comp_name}\"\n"));

        match comp.comp_type.as_str() {
            "control::ConstFloat" => {
                let value: f64 = comp
                    .props
                    .get("out")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0);
                section.push_str("type = \"const_float\"\n");
                section.push_str(&format!("value = {value}\n"));

                // Follow output link to find target channel.
                let targets = self.follow_link_forward(path, "out");
                let (out_ch, wl) = self.resolve_output_from_targets(&targets, consumed);
                section.push_str(&format!("output_channel = {}\n", out_ch.unwrap_or(0)));
                section.push_str(&format!("write_level = {wl}\n"));
            }
            "math::Div2" => {
                section.push_str("type = \"div2\"\n");
                let mut inputs = Vec::new();

                // in1
                if let Some((src_path, _)) = self.follow_link_backward(path, "in1") {
                    if let Some(src_comp) = self.components.get(&src_path) {
                        if let Some(ch) = self.get_component_channel(src_comp) {
                            inputs.push(ch);
                        } else {
                            inputs.push(0);
                        }
                    } else {
                        inputs.push(0);
                    }
                } else {
                    // No link — use inline prop value as constant (channel 0).
                    inputs.push(0);
                }

                // in2
                if let Some((src_path, _)) = self.follow_link_backward(path, "in2") {
                    if let Some(src_comp) = self.components.get(&src_path) {
                        if let Some(ch) = self.get_component_channel(src_comp) {
                            inputs.push(ch);
                        } else {
                            inputs.push(0);
                        }
                    } else {
                        inputs.push(0);
                    }
                } else {
                    inputs.push(0);
                }

                section.push_str(&format!(
                    "input_channels = [{}]\n",
                    inputs
                        .iter()
                        .map(|c| c.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));

                // Follow output.
                let targets = self.follow_link_forward(path, "out");
                let (out_ch, wl) = self.resolve_output_from_targets(&targets, consumed);
                section.push_str(&format!("output_channel = {}\n", out_ch.unwrap_or(0)));
                section.push_str(&format!("write_level = {wl}\n"));
            }
            "math::Add2" => {
                section.push_str("type = \"add2\"\n");
                let inputs = self.resolve_two_inputs(path);
                section.push_str(&format!(
                    "input_channels = [{}]\n",
                    inputs
                        .iter()
                        .map(|c| c.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                let targets = self.follow_link_forward(path, "out");
                let (out_ch, wl) = self.resolve_output_from_targets(&targets, consumed);
                section.push_str(&format!("output_channel = {}\n", out_ch.unwrap_or(0)));
                section.push_str(&format!("write_level = {wl}\n"));
            }
            "math::Sub2" => {
                section.push_str("type = \"sub2\"\n");
                let inputs = self.resolve_two_inputs(path);
                section.push_str(&format!(
                    "input_channels = [{}]\n",
                    inputs
                        .iter()
                        .map(|c| c.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                let targets = self.follow_link_forward(path, "out");
                let (out_ch, wl) = self.resolve_output_from_targets(&targets, consumed);
                section.push_str(&format!("output_channel = {}\n", out_ch.unwrap_or(0)));
                section.push_str(&format!("write_level = {wl}\n"));
            }
            "math::Mul2" => {
                section.push_str("type = \"mul2\"\n");
                let inputs = self.resolve_two_inputs(path);
                section.push_str(&format!(
                    "input_channels = [{}]\n",
                    inputs
                        .iter()
                        .map(|c| c.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                let targets = self.follow_link_forward(path, "out");
                let (out_ch, wl) = self.resolve_output_from_targets(&targets, consumed);
                section.push_str(&format!("output_channel = {}\n", out_ch.unwrap_or(0)));
                section.push_str(&format!("write_level = {wl}\n"));
            }
            "control::Ramp" => {
                section.push_str("type = \"ramp\"\n");
                let min: f64 = comp
                    .props
                    .get("min")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0);
                let max: f64 = comp
                    .props
                    .get("max")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(100.0);
                section.push_str(&format!("min = {min}\n"));
                section.push_str(&format!("max = {max}\n"));
                section.push_str("period_ms = 5000\n");
                let targets = self.follow_link_forward(path, "out");
                let (out_ch, wl) = self.resolve_output_from_targets(&targets, consumed);
                section.push_str(&format!("output_channel = {}\n", out_ch.unwrap_or(0)));
                section.push_str(&format!("write_level = {wl}\n"));
            }
            "math::Neg" => {
                section.push_str("type = \"neg\"\n");
                let mut inputs = Vec::new();
                if let Some((src_path, _)) = self.follow_link_backward(path, "in") {
                    if let Some(src_comp) = self.components.get(&src_path) {
                        if let Some(ch) = self.get_component_channel(src_comp) {
                            inputs.push(ch);
                        }
                    }
                }
                if !inputs.is_empty() {
                    section.push_str(&format!(
                        "input_channels = [{}]\n",
                        inputs
                            .iter()
                            .map(|c| c.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                let targets = self.follow_link_forward(path, "out");
                let (out_ch, wl) = self.resolve_output_from_targets(&targets, consumed);
                section.push_str(&format!("output_channel = {}\n", out_ch.unwrap_or(0)));
                section.push_str(&format!("write_level = {wl}\n"));
            }
            _ => {
                section.push_str(&format!("type = \"{}\"\n", comp.comp_type));
                section.push_str("output_channel = 0\n");
                section.push_str("write_level = 8\n");
            }
        }

        Some(section)
    }

    /// Resolve the two inputs (in1, in2) for a binary math component.
    fn resolve_two_inputs(&self, path: &str) -> Vec<u32> {
        let mut inputs = Vec::new();
        for slot in &["in1", "in2"] {
            if let Some((src_path, _)) = self.follow_link_backward(path, slot) {
                if let Some(src_comp) = self.components.get(&src_path) {
                    if let Some(ch) = self.get_component_channel(src_comp) {
                        inputs.push(ch);
                    } else {
                        inputs.push(0);
                    }
                } else {
                    inputs.push(0);
                }
            } else {
                inputs.push(0);
            }
        }
        inputs
    }

    /// Get the "channel" for a component:
    /// - EacIo::AnalogInput/AnalogOutput/BoolOutput → `channel` prop
    /// - EacIo::AnalogValue/BinaryValue → `virtualCh` prop
    /// - EacIo::ExposeTags → `channel` prop
    fn get_component_channel(&self, comp: &SaxComponent) -> Option<u32> {
        match comp.comp_type.as_str() {
            "EacIo::AnalogInput" | "EacIo::AnalogOutput" | "EacIo::BoolOutput" => {
                comp.props.get("channel").and_then(|v| v.parse().ok())
            }
            "EacIo::AnalogValue" | "EacIo::BinaryValue" => {
                comp.props.get("virtualCh").and_then(|v| v.parse().ok())
            }
            "EacIo::ExposeTags" => comp.props.get("channel").and_then(|v| v.parse().ok()),
            _ => None,
        }
    }

    /// Given forward-link targets, find the output channel and write level.
    fn resolve_output_from_targets(
        &self,
        targets: &[(String, String)],
        consumed: &mut std::collections::HashSet<String>,
    ) -> (Option<u32>, u8) {
        for (target_path, target_slot) in targets {
            if let Some(comp) = self.components.get(target_path.as_str()) {
                if let Some(ch) = self.get_component_channel(comp) {
                    consumed.insert(target_path.clone());
                    return (Some(ch), priority_from_slot(target_slot));
                }
            }
        }
        (None, 8)
    }

    /// Detect write_level from the `inNN` properties present on an I/O component.
    fn detect_write_level_from_props(comp: &SaxComponent) -> u8 {
        for key in comp.props.keys() {
            if let Some(digits) = key.strip_prefix("in") {
                if !digits.is_empty() {
                    if let Ok(level) = digits.parse::<u8>() {
                        return level;
                    }
                }
            }
        }
        8
    }
}

// ── Public conversion function ───────────────────────────────

/// Convert a `.sax` file to a `control.toml` string.
pub fn convert_sax_to_toml<P: AsRef<Path>>(sax_path: P) -> Result<ConversionResult, String> {
    let mut converter = SaxConverter::parse_file(sax_path)?;
    Ok(converter.convert())
}

/// Convert a `.sax` XML string to a `control.toml` string.
pub fn convert_sax_str_to_toml(xml: &str) -> Result<ConversionResult, String> {
    let mut converter = SaxConverter::parse_str(xml)?;
    Ok(converter.convert())
}

// ── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Path to the production .sax file.
    fn production_sax_path() -> String {
        // Navigate from crate root to the production file.
        let manifest = env!("CARGO_MANIFEST_DIR");
        let p = std::path::Path::new(manifest)
            .join("..")
            .join("..")
            .join("..")
            .join("shaystack")
            .join("sandstar")
            .join("sandstar")
            .join("EacIo")
            .join("EacIoApp")
            .join("app.sax");
        p.to_string_lossy().to_string()
    }

    #[test]
    fn test_convert_production_sax() {
        let sax_path = production_sax_path();
        if !std::path::Path::new(&sax_path).exists() {
            eprintln!("Skipping: production .sax not found at {sax_path}");
            return;
        }

        let result = convert_sax_to_toml(&sax_path).expect("conversion should succeed");
        let toml = &result.toml;

        println!("=== Generated TOML ===\n{toml}");
        println!("=== Warnings ===");
        for w in &result.warnings {
            println!("  {w}");
        }

        // Must contain a [[loop]] section.
        assert!(toml.contains("[[loop]]"), "expected [[loop]] section");

        // PID gains from the production LP: kp=20, ki=5.
        assert!(toml.contains("kp = 20"), "expected kp = 20");
        assert!(toml.contains("ki = 5"), "expected ki = 5");

        // Feedback channel = 4 (from /sensor/AnalogI ch=4).
        assert!(
            toml.contains("feedback_channel = 4"),
            "expected feedback_channel = 4"
        );

        // Setpoint channel = 75 (from /setpt/AnalogV virtualCh=75).
        assert!(
            toml.contains("setpoint_channel = 75"),
            "expected setpoint_channel = 75"
        );

        // Output channels should include 35, 50, 53, 47.
        assert!(toml.contains("35"), "expected output ch 35");
        assert!(toml.contains("50"), "expected output ch 50");
        assert!(toml.contains("53"), "expected output ch 53");
        assert!(toml.contains("47"), "expected output ch 47");

        // Must have a sequencer.
        assert!(
            toml.contains("[loop.sequencer]"),
            "expected sequencer section"
        );

        // Must have at least one [[component]] section (the Div2 or ConstFloat).
        assert!(
            toml.contains("[[component]]"),
            "expected [[component]] section"
        );

        // ConstFloat should be present.
        assert!(
            toml.contains("const_float"),
            "expected const_float component"
        );

        // Div2 should be present.
        assert!(toml.contains("div2"), "expected div2 component");
    }

    #[test]
    fn test_parse_links() {
        let xml = r#"
<sedonaApp>
<app>
  <comp name="a" id="1" type="control::LP">
    <prop name="kp" val="10.0"/>
  </comp>
  <comp name="b" id="2" type="EacIo::AnalogInput">
    <prop name="channel" val="5"/>
  </comp>
</app>
<links>
  <link from="/a.out" to="/b.in10"/>
  <link from="/b.out" to="/a.cv"/>
</links>
</sedonaApp>"#;

        let conv = SaxConverter::parse_str(xml).unwrap();
        assert_eq!(conv.links.len(), 2);

        assert_eq!(conv.links[0].from_path, "/a");
        assert_eq!(conv.links[0].from_slot, "out");
        assert_eq!(conv.links[0].to_path, "/b");
        assert_eq!(conv.links[0].to_slot, "in10");

        assert_eq!(conv.links[1].from_path, "/b");
        assert_eq!(conv.links[1].from_slot, "out");
        assert_eq!(conv.links[1].to_path, "/a");
        assert_eq!(conv.links[1].to_slot, "cv");
    }

    #[test]
    fn test_eliminate_write_float() {
        // WriteFloat should not appear in output TOML as a named component.
        let xml = r#"
<sedonaApp>
<app>
  <comp name="sensor" id="1" type="EacIo::CycleFolder">
    <comp name="AI" id="2" type="EacIo::AnalogInput">
      <prop name="channel" val="4"/>
    </comp>
    <comp name="WF" id="3" type="control::WriteFloat">
      <prop name="in" val="77.0"/>
      <prop name="out" val="77.0"/>
    </comp>
  </comp>
  <comp name="LP" id="4" type="control::LP">
    <prop name="kp" val="5.0"/>
    <prop name="ki" val="1.0"/>
  </comp>
  <comp name="BO" id="5" type="EacIo::BoolOutput">
    <prop name="channel" val="35"/>
    <prop name="in10" val="true"/>
  </comp>
  <comp name="AV" id="6" type="EacIo::AnalogValue">
    <prop name="virtualCh" val="75"/>
    <prop name="in16" val="70.0"/>
  </comp>
</app>
<links>
  <link from="/sensor/AI.out" to="/sensor/WF.in"/>
  <link from="/sensor/WF.out" to="/LP.cv"/>
  <link from="/AV.out" to="/LP.sp"/>
  <link from="/LP.out" to="/BO.in10"/>
</links>
</sedonaApp>"#;

        let mut conv = SaxConverter::parse_str(xml).unwrap();
        let result = conv.convert();
        let toml = &result.toml;

        println!("=== TOML ===\n{toml}");

        // WriteFloat should NOT appear as a component.
        assert!(
            !toml.contains("write_float"),
            "WriteFloat should be eliminated"
        );

        // But the loop should detect feedback_channel = 4 through the WriteFloat.
        assert!(
            toml.contains("feedback_channel = 4"),
            "should follow through WriteFloat to find AnalogInput ch=4"
        );
    }

    #[test]
    fn test_skip_services() {
        let xml = r#"
<sedonaApp>
<app>
  <comp name="service" id="1" type="sys::Folder">
    <comp name="plat" id="2" type="platUnix::UnixPlatformService">
      <prop name="platformId" val="test"/>
    </comp>
    <comp name="users" id="3" type="sys::UserService"/>
  </comp>
  <comp name="LP" id="4" type="control::LP">
    <prop name="kp" val="1.0"/>
  </comp>
</app>
<links/>
</sedonaApp>"#;

        let mut conv = SaxConverter::parse_str(xml).unwrap();
        let result = conv.convert();

        // Service components should not appear in TOML output.
        assert!(
            !result.toml.contains("plat"),
            "platform service should be skipped"
        );
        assert!(
            !result.toml.contains("users"),
            "user service should be skipped"
        );
    }

    #[test]
    fn test_follow_link_chain() {
        // Chain: AnalogInput → WriteFloat → LP.cv
        let xml = r#"
<sedonaApp>
<app>
  <comp name="AI" id="1" type="EacIo::AnalogInput">
    <prop name="channel" val="7"/>
  </comp>
  <comp name="WF1" id="2" type="control::WriteFloat">
    <prop name="in" val="0.0"/>
    <prop name="out" val="0.0"/>
  </comp>
  <comp name="WF2" id="3" type="control::WriteFloat">
    <prop name="in" val="0.0"/>
    <prop name="out" val="0.0"/>
  </comp>
  <comp name="LP" id="4" type="control::LP">
    <prop name="kp" val="1.0"/>
  </comp>
</app>
<links>
  <link from="/AI.out" to="/WF1.in"/>
  <link from="/WF1.out" to="/WF2.in"/>
  <link from="/WF2.out" to="/LP.cv"/>
</links>
</sedonaApp>"#;

        let conv = SaxConverter::parse_str(xml).unwrap();

        // Following backward from /LP.cv should resolve through 2 WriteFloats
        // to find /AI.
        let result = conv.follow_link_backward("/LP", "cv");
        assert!(result.is_some(), "should resolve through chain");
        let (src_path, src_slot) = result.unwrap();
        assert_eq!(src_path, "/AI", "should resolve to AnalogInput");
        assert_eq!(src_slot, "out", "should resolve to .out slot");
    }

    #[test]
    fn test_priority_from_slot_name() {
        assert_eq!(priority_from_slot("in10"), 10);
        assert_eq!(priority_from_slot("in16"), 16);
        assert_eq!(priority_from_slot("in8"), 8);
        assert_eq!(priority_from_slot("in1"), 1);
        assert_eq!(priority_from_slot("in"), 8); // default
        assert_eq!(priority_from_slot("out"), 8); // not an input slot
    }

    #[test]
    fn test_round_trip() {
        // Convert the production .sax to TOML, then verify the TOML can be
        // parsed by toml::from_str into the ControlToml structure.
        let sax_path = production_sax_path();
        if !std::path::Path::new(&sax_path).exists() {
            eprintln!("Skipping: production .sax not found at {sax_path}");
            return;
        }

        let result = convert_sax_to_toml(&sax_path).expect("conversion should succeed");
        let toml_str = &result.toml;

        // Strip comment lines for parsing (comments are valid TOML, but
        // let's be safe).
        println!("=== Round-trip TOML ===\n{toml_str}");

        // Parse using the same serde structs the control runner uses.
        #[derive(serde::Deserialize)]
        struct TestControlToml {
            #[serde(rename = "loop", default)]
            loops: Vec<TestLoopConfig>,
            #[serde(default)]
            component: Vec<TestComponentConfig>,
        }

        #[derive(serde::Deserialize)]
        struct TestLoopConfig {
            name: String,
            feedback_channel: u32,
            setpoint_channel: u32,
            output_channels: Vec<u32>,
            #[serde(default)]
            write_level: u8,
            pid: TestPidConfig,
            sequencer: Option<TestSeqConfig>,
        }

        #[derive(serde::Deserialize)]
        struct TestPidConfig {
            kp: f64,
            ki: f64,
            kd: f64,
            min: f64,
            max: f64,
            direct: bool,
            exec_interval_ms: u64,
        }

        #[derive(serde::Deserialize)]
        struct TestSeqConfig {
            hysteresis: f64,
        }

        #[derive(serde::Deserialize)]
        struct TestComponentConfig {
            name: String,
            #[serde(rename = "type")]
            component_type: String,
            #[serde(default)]
            output_channel: u32,
            #[serde(default)]
            write_level: u8,
            #[serde(default)]
            value: Option<f64>,
            #[serde(default)]
            input_channels: Vec<u32>,
        }

        let parsed: TestControlToml =
            toml::from_str(toml_str).expect("generated TOML should parse");

        assert!(!parsed.loops.is_empty(), "should have at least one loop");
        assert_eq!(parsed.loops[0].feedback_channel, 4);
        assert_eq!(parsed.loops[0].setpoint_channel, 75);
        assert!(
            parsed.loops[0].sequencer.is_some(),
            "loop should have sequencer"
        );
        assert!(!parsed.component.is_empty(), "should have components");
    }

    #[test]
    fn test_warnings_for_unsupported() {
        let xml = r#"
<sedonaApp>
<app>
  <comp name="shay" id="1" type="shaystack::TagWriter">
    <prop name="tag" val="test"/>
  </comp>
  <comp name="expose" id="2" type="EacIo::ExposeTags">
    <prop name="channel" val="10"/>
    <prop name="Tag" val="temp"/>
  </comp>
  <comp name="rec" id="3" type="EacIo::RecordCount">
    <prop name="pointQuery" val="test"/>
  </comp>
</app>
<links/>
</sedonaApp>"#;

        let mut conv = SaxConverter::parse_str(xml).unwrap();
        let result = conv.convert();

        // Should have warnings about shaystack.
        let has_shaystack_warning = result
            .warnings
            .iter()
            .any(|w| w.contains("shaystack") && w.contains("not supported"));
        assert!(
            has_shaystack_warning,
            "should warn about shaystack component"
        );

        // Should have info about ExposeTags.
        let has_expose_info = result
            .warnings
            .iter()
            .any(|w| w.contains("ExposeTags") && w.contains("skipped"));
        assert!(has_expose_info, "should info about ExposeTags");

        // Should have info about RecordCount.
        let has_record_info = result.warnings.iter().any(|w| w.contains("RecordCount"));
        assert!(has_record_info, "should info about RecordCount");
    }
}
