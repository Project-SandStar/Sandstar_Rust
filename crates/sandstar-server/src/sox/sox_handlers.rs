//! SOX command handlers and virtual component tree.
//!
//! The Sandstar engine uses flat channels, but Sedona Application Editor
//! expects a hierarchical component tree. This module presents a virtual tree:
//!
//! ```text
//! App (compId=0)
//! +-- service (1)
//! |   +-- sox (2)
//! |   +-- users (3)
//! |   +-- plat (4)
//! +-- io (5)
//! |   +-- ch_1113 (100) -> maps to engine channel 1113
//! |   +-- ch_1713 (101) -> maps to engine channel 1713
//! |   +-- ...
//! +-- control (6)
//!     +-- (reserved for future PID/sequencer mapping)
//! ```
//!
//! SOX commands are handled synchronously against the tree. Write commands
//! are forwarded to the engine via the `EngineHandle` command channel.

use std::collections::{HashMap, HashSet};

use sandstar_ipc::types::ChannelInfo;

use super::sox_protocol::{SoxCmd, SoxReader, SoxRequest, SoxResponse, SoxValueType};

// ---- Slot values ----

/// A slot value in the virtual component tree.
#[derive(Debug, Clone, PartialEq)]
pub enum SlotValue {
    Bool(bool),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Str(String),
    Buf(Vec<u8>),
    Null,
}

impl SlotValue {
    /// Return the SOX type ID for this value.
    pub fn type_id(&self) -> u8 {
        match self {
            SlotValue::Bool(_) => SoxValueType::Bool as u8,
            SlotValue::Int(_) => SoxValueType::Int as u8,
            SlotValue::Long(_) => SoxValueType::Long as u8,
            SlotValue::Float(_) => SoxValueType::Float as u8,
            SlotValue::Double(_) => SoxValueType::Double as u8,
            SlotValue::Str(_) => SoxValueType::Double as u8 + 1, // Str = 7 (after Buf=8 in protocol)
            SlotValue::Buf(_) => SoxValueType::Buf as u8,
            SlotValue::Null => SoxValueType::Void as u8,
        }
    }
}

// ---- Slot flags ----

/// Slot property flags (matches Sedona slot flag definitions).
pub const SLOT_FLAG_PROPERTY: u8 = 0x00;
pub const SLOT_FLAG_CONFIG: u8 = 0x01;
#[allow(dead_code)]
pub const SLOT_FLAG_ACTION: u8 = 0x02;
#[allow(dead_code)]
pub const SLOT_FLAG_RUNTIME: u8 = 0x04;
#[allow(dead_code)]
pub const SLOT_FLAG_OPERATOR: u8 = 0x08;

// ---- Virtual component ----

/// A virtual Sedona component mapped from engine data.
#[derive(Debug, Clone)]
pub struct VirtualComponent {
    pub comp_id: u16,
    pub parent_id: u16,
    pub name: String,
    pub type_name: String,
    pub kit_id: u8,
    pub type_id: u8,
    pub children: Vec<u16>,
    pub slots: Vec<VirtualSlot>,
}

/// A single slot on a virtual component.
#[derive(Debug, Clone)]
pub struct VirtualSlot {
    pub name: String,
    pub type_id: u8,
    pub flags: u8,
    pub value: SlotValue,
}

// ---- Component tree ----

/// No parent sentinel (root component).
pub const NO_PARENT: u16 = 0xFFFF;

/// Base comp_id for channel-mapped components.
pub const CHANNEL_COMP_BASE: u16 = 100;

/// The virtual component tree builder.
pub struct ComponentTree {
    components: HashMap<u16, VirtualComponent>,
    next_id: u16,
    /// Upper bound (exclusive) of channel-mapped component IDs.
    /// Components in [CHANNEL_COMP_BASE, channel_comp_end) are channels with valid schemas.
    /// Components >= channel_comp_end are dynamically added (no manifest schema).
    pub channel_comp_end: u16,
}

impl ComponentTree {
    /// Create an empty tree.
    pub fn new() -> Self {
        Self {
            components: HashMap::new(),
            next_id: 0,
            channel_comp_end: CHANNEL_COMP_BASE,
        }
    }

    /// Add a component to the tree. Also registers it as a child of its parent.
    pub fn add(&mut self, comp: VirtualComponent) {
        let comp_id = comp.comp_id;
        let parent_id = comp.parent_id;
        if comp_id >= self.next_id {
            self.next_id = comp_id + 1;
        }
        self.components.insert(comp_id, comp);
        // Register as child of parent (if parent exists and is not self).
        if parent_id != NO_PARENT && parent_id != comp_id {
            if let Some(parent) = self.components.get_mut(&parent_id) {
                if !parent.children.contains(&comp_id) {
                    parent.children.push(comp_id);
                }
            }
        }
    }

    /// Retrieve a component by ID.
    pub fn get(&self, comp_id: u16) -> Option<&VirtualComponent> {
        self.components.get(&comp_id)
    }

    /// Retrieve a mutable component by ID.
    #[allow(dead_code)]
    pub fn get_mut(&mut self, comp_id: u16) -> Option<&mut VirtualComponent> {
        self.components.get_mut(&comp_id)
    }

    /// Total number of components in the tree.
    pub fn len(&self) -> usize {
        self.components.len()
    }

    /// Whether the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }

    /// All component IDs in the tree (unordered).
    pub fn comp_ids(&self) -> Vec<u16> {
        self.components.keys().copied().collect()
    }

    /// Allocate the next available component ID.
    pub fn next_comp_id(&self) -> u16 {
        self.components.keys().copied().max().unwrap_or(0) + 1
    }

    /// Remove a component and unlink it from its parent's children list.
    /// Returns the removed component, or None if not found.
    pub fn remove(&mut self, comp_id: u16) -> Option<VirtualComponent> {
        let comp = self.components.remove(&comp_id)?;
        // Remove from parent's children list
        if let Some(parent) = self.components.get_mut(&comp.parent_id) {
            parent.children.retain(|&id| id != comp_id);
        }
        Some(comp)
    }

    /// Rename a component. Returns true if found and renamed.
    pub fn rename(&mut self, comp_id: u16, new_name: String) -> bool {
        if let Some(comp) = self.components.get_mut(&comp_id) {
            comp.name = new_name;
            true
        } else {
            false
        }
    }

    /// Build a virtual component tree from engine channel data.
    ///
    /// Creates the standard Sedona tree structure with service nodes
    /// and maps each channel to a `control::NumericPoint` component.
    pub fn from_channels(channels: &[ChannelInfo]) -> Self {
        let mut tree = Self::new();

        // Root: App (compId=0)
        tree.add(VirtualComponent {
            comp_id: 0,
            parent_id: NO_PARENT,
            name: "app".into(),
            type_name: "sys::App".into(),
            kit_id: 0,  // sys
            type_id: 10, // App
            children: Vec::new(),
            slots: vec![VirtualSlot {
                name: "appName".into(),
                type_id: SoxValueType::Float as u8, // using Float slot for string (wire compat)
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Str("sandstar".into()),
            }],
        });

        // Service folder (compId=1)
        tree.add(VirtualComponent {
            comp_id: 1,
            parent_id: 0,
            name: "service".into(),
            type_name: "sys::Folder".into(),
            kit_id: 0,  // sys
            type_id: 11, // Folder
            children: Vec::new(),
            slots: Vec::new(),
        });

        // SOX service (compId=2)
        tree.add(VirtualComponent {
            comp_id: 2,
            parent_id: 1,
            name: "sox".into(),
            type_name: "sox::SoxService".into(),
            kit_id: 12, // sox
            type_id: 1,  // SoxService
            children: Vec::new(),
            slots: vec![VirtualSlot {
                name: "port".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(1876),
            }],
        });

        // Users service (compId=3)
        tree.add(VirtualComponent {
            comp_id: 3,
            parent_id: 1,
            name: "users".into(),
            type_name: "sys::UserService".into(),
            kit_id: 0,  // sys
            type_id: 16, // UserService
            children: Vec::new(),
            slots: Vec::new(),
        });

        // Platform service (compId=4)
        tree.add(VirtualComponent {
            comp_id: 4,
            parent_id: 1,
            name: "plat".into(),
            type_name: "sys::PlatformService".into(),
            kit_id: 0,  // sys
            type_id: 13, // PlatformService
            children: Vec::new(),
            slots: vec![VirtualSlot {
                name: "platformId".into(),
                type_id: SoxValueType::Float as u8, // string slot
                flags: SLOT_FLAG_PROPERTY,
                value: SlotValue::Str("sandstar-rust".into()),
            }],
        });

        // IO folder (compId=5)
        tree.add(VirtualComponent {
            comp_id: 5,
            parent_id: 0,
            name: "io".into(),
            type_name: "sys::Folder".into(),
            kit_id: 0,  // sys
            type_id: 11, // Folder
            children: Vec::new(),
            slots: Vec::new(),
        });

        // Control folder (compId=6)
        tree.add(VirtualComponent {
            comp_id: 6,
            parent_id: 0,
            name: "control".into(),
            type_name: "sys::Folder".into(),
            kit_id: 0,  // sys
            type_id: 11, // Folder
            children: Vec::new(),
            slots: Vec::new(),
        });

        // Map each channel to a component under io (compId = 100 + index)
        for (i, ch) in channels.iter().enumerate() {
            let comp_id = CHANNEL_COMP_BASE + i as u16;
            tree.add(VirtualComponent {
                comp_id,
                parent_id: 5, // io folder
                name: format!("ch_{}", ch.id),
                type_name: channel_type_name(&ch.direction),
                kit_id: 1, // EacIo kit (index 1 in DEFAULT_KITS)
                type_id: 0, // AnalogInput
                children: Vec::new(),
                slots: channel_slots(ch),
            });
        }

        // Record the upper bound of channel comp_ids
        tree.channel_comp_end = CHANNEL_COMP_BASE + channels.len() as u16;

        tree
    }

    /// Check if a comp_id is a channel-mapped component with a valid manifest schema.
    pub fn is_channel_comp(&self, comp_id: u16) -> bool {
        comp_id >= CHANNEL_COMP_BASE && comp_id < self.channel_comp_end
    }

    /// Update slot values for channel-mapped components from fresh channel data.
    ///
    /// Returns the list of comp_ids that had value changes (for COV events).
    pub fn update_from_channels(&mut self, channels: &[ChannelInfo]) -> Vec<u16> {
        let mut changed = Vec::new();
        for (i, ch) in channels.iter().enumerate() {
            let comp_id = CHANNEL_COMP_BASE + i as u16;
            if let Some(comp) = self.components.get_mut(&comp_id) {
                let new_slots = channel_slots(ch);
                if slots_differ(&comp.slots, &new_slots) {
                    comp.slots = new_slots;
                    changed.push(comp_id);
                }
            }
        }
        changed
    }
}

impl Default for ComponentTree {
    fn default() -> Self {
        Self::new()
    }
}

/// Recursively collect a component and all its descendants.
fn collect_subtree(tree: &ComponentTree, comp_id: u16, out: &mut Vec<u16>) {
    if out.contains(&comp_id) {
        return;
    }
    out.push(comp_id);
    if let Some(comp) = tree.get(comp_id) {
        for &child_id in &comp.children {
            collect_subtree(tree, child_id, out);
        }
    }
}

/// Determine the Sedona type name from channel direction.
fn channel_type_name(direction: &str) -> String {
    match direction {
        "AI" | "ao" | "AO" => "control::NumericPoint".into(),
        "DI" | "do" | "DO" => "control::BooleanPoint".into(),
        _ => "control::NumericPoint".into(),
    }
}

/// Build the slot list for a channel-mapped component.
///
/// Must match EacIo::AnalogInput manifest slot order exactly.
/// The editor reads slot values by iterating the schema's slot array — no type bytes on wire.
///
/// From EacIo-6f9da65b.xml type id="0" (AnalogInput):
///   Inherited: meta (int, config)
///   0: channelName (Buf, runtime)
///   1: channel     (int, runtime)
///   2: pointQuery  (Buf, config)
///   3: pointQuerySize (int, runtime)
///   4: pointQueryStatus (bool, runtime)
///   5: out         (float, runtime)
///   6: curStatus   (Buf, runtime)
///   7: enabled     (bool, runtime)
///   8: query       (void, action — not serialized)
fn channel_slots(ch: &ChannelInfo) -> Vec<VirtualSlot> {
    vec![
        // Inherited from sys::Component
        VirtualSlot {
            name: "meta".into(),
            type_id: SoxValueType::Int as u8,
            flags: SLOT_FLAG_CONFIG,
            value: SlotValue::Int(1), // default meta value
        },
        // EacIo::AnalogInput slots in manifest order
        VirtualSlot {
            name: "channelName".into(),
            type_id: SoxValueType::Buf as u8,
            flags: SLOT_FLAG_RUNTIME,
            value: SlotValue::Str(ch.label.clone()),
        },
        VirtualSlot {
            name: "channel".into(),
            type_id: SoxValueType::Int as u8,
            flags: SLOT_FLAG_RUNTIME,
            value: SlotValue::Int(ch.id as i32),
        },
        VirtualSlot {
            name: "pointQuery".into(),
            type_id: SoxValueType::Buf as u8,
            flags: SLOT_FLAG_CONFIG,
            value: SlotValue::Str(String::new()),
        },
        VirtualSlot {
            name: "pointQuerySize".into(),
            type_id: SoxValueType::Int as u8,
            flags: SLOT_FLAG_RUNTIME,
            value: SlotValue::Int(0),
        },
        VirtualSlot {
            name: "pointQueryStatus".into(),
            type_id: SoxValueType::Bool as u8,
            flags: SLOT_FLAG_RUNTIME,
            value: SlotValue::Bool(false),
        },
        VirtualSlot {
            name: "out".into(),
            type_id: SoxValueType::Float as u8,
            flags: SLOT_FLAG_RUNTIME,
            value: SlotValue::Float(ch.cur as f32),
        },
        VirtualSlot {
            name: "curStatus".into(),
            type_id: SoxValueType::Buf as u8,
            flags: SLOT_FLAG_RUNTIME,
            value: SlotValue::Str(ch.status.clone()),
        },
        VirtualSlot {
            name: "enabled".into(),
            type_id: SoxValueType::Bool as u8,
            flags: SLOT_FLAG_RUNTIME,
            value: SlotValue::Bool(ch.enabled),
        },
        // query (void, action) — not included, never serialized
    ]
}

/// Compare two slot vectors for value equality.
fn slots_differ(a: &[VirtualSlot], b: &[VirtualSlot]) -> bool {
    if a.len() != b.len() {
        return true;
    }
    for (sa, sb) in a.iter().zip(b.iter()) {
        if sa.value != sb.value {
            return true;
        }
    }
    false
}

/// Convert a channel status string to an integer code.
///
/// Sedona uses integer status codes: 0=ok, 1=fault, 2=down, 3=disabled, 4=stale.
pub fn status_to_int(status: &str) -> i32 {
    match status {
        "ok" | "Ok" | "OK" => 0,
        "fault" | "Fault" | "FAULT" => 1,
        "down" | "Down" | "DOWN" => 2,
        "disabled" | "Disabled" | "DISABLED" => 3,
        "stale" | "Stale" | "STALE" => 4,
        _ => 1, // unknown -> fault
    }
}

/// Convert an integer status code back to a string.
#[allow(dead_code)]
pub fn int_to_status(code: i32) -> &'static str {
    match code {
        0 => "ok",
        1 => "fault",
        2 => "down",
        3 => "disabled",
        4 => "stale",
        _ => "fault",
    }
}

// ---- Encode slot value into a SoxResponse ----

/// Encode a slot value into the response payload.
pub fn encode_slot_value(resp: &mut SoxResponse, value: &SlotValue) {
    match value {
        SlotValue::Bool(v) => {
            resp.write_u8(if *v { 1 } else { 0 });
        }
        SlotValue::Int(v) => {
            resp.write_i32(*v);
        }
        SlotValue::Long(v) => {
            resp.payload.extend_from_slice(&v.to_be_bytes());
        }
        SlotValue::Float(v) => {
            resp.write_f32(*v);
        }
        SlotValue::Double(v) => {
            resp.write_f64(*v);
        }
        SlotValue::Str(v) => {
            // Sedona Str binary format: u2 size (including null) + chars + 0x00
            let bytes = v.as_bytes();
            resp.write_u16((bytes.len() + 1) as u16); // size includes null terminator
            resp.write_bytes(bytes);
            resp.write_u8(0x00); // null terminator
        }
        SlotValue::Buf(v) => {
            resp.write_u16(v.len() as u16);
            resp.write_bytes(v);
        }
        SlotValue::Null => {
            // No payload for void/null.
        }
    }
}

/// Encode a slot value directly into a raw byte vector (for COV events).
///
/// Sedona property values on the wire: no type prefix, just the value bytes.
/// Buf/Str properties use u2 length + bytes (NOT null-terminated).
pub fn encode_slot_value_raw(buf: &mut Vec<u8>, value: &SlotValue) {
    match value {
        SlotValue::Bool(v) => buf.push(if *v { 1 } else { 0 }),
        SlotValue::Int(v) => buf.extend_from_slice(&v.to_be_bytes()),
        SlotValue::Long(v) => buf.extend_from_slice(&v.to_be_bytes()),
        SlotValue::Float(v) => buf.extend_from_slice(&v.to_be_bytes()),
        SlotValue::Double(v) => buf.extend_from_slice(&v.to_be_bytes()),
        SlotValue::Str(s) => {
            // Sedona Str binary format: u2 size (including null) + chars + 0x00
            let bytes = s.as_bytes();
            buf.extend_from_slice(&((bytes.len() + 1) as u16).to_be_bytes());
            buf.extend_from_slice(bytes);
            buf.push(0x00); // null terminator
        }
        SlotValue::Buf(v) => {
            buf.extend_from_slice(&(v.len() as u16).to_be_bytes());
            buf.extend_from_slice(v);
        }
        SlotValue::Null => {} // void — no bytes
    }
}

/// Decode a slot value from a SoxReader given a type ID.
pub fn decode_slot_value(reader: &mut SoxReader<'_>, type_id: u8) -> Option<SlotValue> {
    match type_id {
        t if t == SoxValueType::Bool as u8 => reader.read_u8().map(|v| SlotValue::Bool(v != 0)),
        t if t == SoxValueType::Int as u8 => reader.read_i32().map(SlotValue::Int),
        t if t == SoxValueType::Long as u8 => {
            // Read 8 bytes for i64
            if reader.remaining() >= 8 {
                let bytes = reader.read_bytes(8)?;
                let v = i64::from_be_bytes(bytes.try_into().ok()?);
                Some(SlotValue::Long(v))
            } else {
                None
            }
        }
        t if t == SoxValueType::Float as u8 => reader.read_f32().map(SlotValue::Float),
        t if t == SoxValueType::Double as u8 => reader.read_f64().map(|v| SlotValue::Double(v)),
        t if t == SoxValueType::Buf as u8 => {
            let len = reader.read_u16()? as usize;
            let bytes = reader.read_bytes(len)?;
            Some(SlotValue::Buf(bytes.to_vec()))
        }
        // Treat Byte/Short as Int for simplicity
        t if t == SoxValueType::Byte as u8 => reader.read_u8().map(|v| SlotValue::Int(v as i32)),
        t if t == SoxValueType::Short as u8 => {
            reader.read_u16().map(|v| SlotValue::Int(v as i32))
        }
        _ => Some(SlotValue::Null),
    }
}

// ---- Kit definitions ----

/// Kit info for readSchema/readVersion responses.
#[derive(Debug, Clone)]
pub struct KitInfo {
    pub name: &'static str,
    pub checksum: u32,
    pub version: &'static str,
}

/// Default kit list matching the EacIo Sedona application (from shaystack/app/app.sax).
/// Checksums extracted from kit filenames on device (name-CHECKSUM-version.kit).
pub const DEFAULT_KITS: &[KitInfo] = &[
    KitInfo { name: "sys",        checksum: 0xd3984c51, version: "1.2.28" },
    KitInfo { name: "EacIo",     checksum: 0x6f9da65b, version: "1.2.30" },
    KitInfo { name: "control",   checksum: 0x808b7db3, version: "1.2.28" },
    KitInfo { name: "driver",    checksum: 0xb4cc82ce, version: "1.2.28" },
    KitInfo { name: "func",      checksum: 0x821b7396, version: "1.2.28" },
    KitInfo { name: "hvac",      checksum: 0x7264c67c, version: "1.2.28" },
    KitInfo { name: "inet",      checksum: 0x25648ba7, version: "1.2.28" },
    KitInfo { name: "logic",     checksum: 0x9fe95ce1, version: "1.2.28" },
    KitInfo { name: "math",      checksum: 0xc22b255c, version: "1.2.28" },
    KitInfo { name: "platUnix",  checksum: 0x751711ab, version: "1.2.28" },
    KitInfo { name: "pricomp",   checksum: 0xb5cd6698, version: "1.2.28" },
    KitInfo { name: "shaystack", checksum: 0xedf7a27c, version: "1.2"    },
    KitInfo { name: "sox",       checksum: 0x397a84dd, version: "1.2.28" },
    KitInfo { name: "types",     checksum: 0x10936551, version: "1.2.28" },
    KitInfo { name: "web",       checksum: 0x0d0dd007, version: "1.2.29" },
];

// ---- Subscription manager ----

/// Manages SOX COV (change-of-value) subscriptions.
///
/// Maps component IDs to the set of sessions watching them.
pub struct SubscriptionManager {
    /// comp_id -> set of session_ids watching this component.
    subscriptions: HashMap<u16, HashSet<u16>>,
    /// session_id -> set of comp_ids this session is watching.
    by_session: HashMap<u16, HashSet<u16>>,
}

impl SubscriptionManager {
    pub fn new() -> Self {
        Self {
            subscriptions: HashMap::new(),
            by_session: HashMap::new(),
        }
    }

    /// Subscribe a session to a component's COV events.
    pub fn subscribe(&mut self, session_id: u16, comp_id: u16) {
        self.subscriptions
            .entry(comp_id)
            .or_default()
            .insert(session_id);
        self.by_session
            .entry(session_id)
            .or_default()
            .insert(comp_id);
    }

    /// Unsubscribe a session from a component.
    pub fn unsubscribe(&mut self, session_id: u16, comp_id: u16) {
        if let Some(sessions) = self.subscriptions.get_mut(&comp_id) {
            sessions.remove(&session_id);
            if sessions.is_empty() {
                self.subscriptions.remove(&comp_id);
            }
        }
        if let Some(comps) = self.by_session.get_mut(&session_id) {
            comps.remove(&comp_id);
            if comps.is_empty() {
                self.by_session.remove(&session_id);
            }
        }
    }

    /// Unsubscribe a session from all components (session teardown).
    pub fn unsubscribe_all(&mut self, session_id: u16) {
        if let Some(comps) = self.by_session.remove(&session_id) {
            for comp_id in comps {
                if let Some(sessions) = self.subscriptions.get_mut(&comp_id) {
                    sessions.remove(&session_id);
                    if sessions.is_empty() {
                        self.subscriptions.remove(&comp_id);
                    }
                }
            }
        }
    }

    /// Get the set of session IDs watching a given component.
    pub fn get_watchers(&self, comp_id: u16) -> Option<&HashSet<u16>> {
        self.subscriptions.get(&comp_id)
    }

    /// Check if a session is subscribed to a component.
    pub fn is_subscribed(&self, session_id: u16, comp_id: u16) -> bool {
        self.subscriptions
            .get(&comp_id)
            .is_some_and(|s| s.contains(&session_id))
    }

    /// Get the total number of active subscriptions.
    pub fn total_subscriptions(&self) -> usize {
        self.subscriptions.values().map(|s| s.len()).sum()
    }

    /// Get the components a session is subscribed to.
    #[allow(dead_code)]
    pub fn session_components(&self, session_id: u16) -> Option<&HashSet<u16>> {
        self.by_session.get(&session_id)
    }

    /// Build COV event payloads for changed components.
    ///
    /// Returns `(session_id, event_bytes)` pairs for each subscriber
    /// that needs to be notified.
    ///
    /// Sedona event wire format (unsolicited push):
    ///   byte 0:   'e' (lowercase — unsolicited event, NOT a response)
    ///   byte 1:   0xFF (replyNum — no reply expected)
    ///   byte 2-3: comp_id (u16 big-endian)
    ///   byte 4:   what ('r' for runtime)
    ///   bytes 5+: slot values in schema order (NO type bytes, NO count)
    pub fn build_events(
        &self,
        changed_comps: &[u16],
        tree: &ComponentTree,
    ) -> Vec<(u16, Vec<u8>)> {
        let mut events = Vec::new();
        for &comp_id in changed_comps {
            // Only push COV events for channel-mapped components with valid manifest schemas.
            // Added components (comp_id >= channel_comp_end) have auto-extended slots.
            if !tree.is_channel_comp(comp_id) {
                continue;
            }
            let Some(comp) = tree.get(comp_id) else {
                continue;
            };
            let Some(watchers) = self.subscriptions.get(&comp_id) else {
                continue;
            };
            // Build raw event bytes: ['e', 0xFF, comp_id, 'r', slot_values...]
            let mut payload = Vec::with_capacity(64);
            payload.push(b'e');  // lowercase — unsolicited event
            payload.push(0xFF);  // replyNum (unused for events)
            payload.extend_from_slice(&comp_id.to_be_bytes());
            payload.push(b'r');  // what = runtime

            // Write slot values in schema order (NO type_id prefix, NO count)
            for slot in &comp.slots {
                if slot.flags & SLOT_FLAG_ACTION != 0 {
                    continue; // skip action slots
                }
                if slot.flags & SLOT_FLAG_CONFIG != 0 {
                    continue; // skip config slots — this is a runtime event
                }
                encode_slot_value_raw(&mut payload, &slot.value);
            }

            for &session_id in watchers {
                events.push((session_id, payload.clone()));
            }
        }
        events
    }
}

impl Default for SubscriptionManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---- SOX command handlers ----

/// Handle a SOX request and produce a response.
pub fn handle_sox_request(
    request: &SoxRequest,
    tree: &mut ComponentTree,
    subscriptions: &mut SubscriptionManager,
    session_id: u16,
) -> SoxResponse {
    match request.cmd {
        SoxCmd::ReadSchema => handle_read_schema(request),
        SoxCmd::ReadVersion => handle_read_version(request),
        SoxCmd::ReadComp => handle_read_comp(request, tree),
        SoxCmd::ReadProp => handle_read_prop(request, tree),
        SoxCmd::Subscribe => handle_subscribe(request, subscriptions, session_id, tree),
        SoxCmd::Unsubscribe => handle_unsubscribe(request, subscriptions, session_id),
        SoxCmd::Write => handle_write(request, tree),
        SoxCmd::Add => handle_add(request, tree),
        SoxCmd::Delete => handle_delete(request, tree),
        SoxCmd::Rename => handle_rename(request, tree),
        SoxCmd::Invoke => handle_invoke(request, tree),
        SoxCmd::FileOpen => handle_file_open(request),
        SoxCmd::FileRead => handle_file_read(request),
        SoxCmd::FileClose => handle_file_close(request),
        _ => error_msg(request.cmd, request.req_id, "unsupported command"),
    }
}

// ---- SOX File Transfer ----

use std::sync::Mutex;

/// Global file transfer state for SOX kit downloads.
static SOX_FILE_XFER: Mutex<Option<SoxFileXfer>> = Mutex::new(None);

struct SoxFileXfer {
    data: Vec<u8>,
    chunk_size: usize,
}

const SOX_CHUNK_SIZE: usize = 256;
const KITS_BASE_DIR: &str = "/home/eacio/sandstar/etc/kits";
const MANIFESTS_DIR: &str = "/home/eacio/sandstar/etc/manifests";

/// fileOpen ('f') — open a file for reading, return size info.
///
/// Supports URI schemes:
///   - `m:kitname.xml` — kit manifest download
///   - `m:m.zip` — bundled manifests (not yet implemented, returns error)
///   - `/kits/...` — kit binary download
///
/// Response: u4 fileSize, u2 numChunks, u2 chunkSize
fn handle_file_open(req: &SoxRequest) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let method = reader.read_str().unwrap_or_default();
    let uri = reader.read_str().unwrap_or_default();

    tracing::info!(method = %method, uri = %uri, "SOX: fileOpen");

    // Resolve URI to a local file path
    let local_path = if uri.starts_with("m:") {
        // Manifest URI: "m:kitname.xml" or "m:m.zip"
        let manifest_name = &uri[2..];
        if manifest_name == "m.zip" {
            tracing::warn!("SOX: fileOpen m:m.zip not supported");
            return error_msg(SoxCmd::FileOpen, req.req_id, "m.zip not supported");
        }
        format!("{}/{}", MANIFESTS_DIR, manifest_name)
    } else if uri.starts_with("/kits/") {
        format!("{}/{}", KITS_BASE_DIR, &uri[6..])
    } else {
        format!("{}/{}", KITS_BASE_DIR, &uri)
    };

    // Sanitize against path traversal
    if local_path.contains("..") || local_path.contains('\0') {
        tracing::warn!(uri = %uri, "SOX: fileOpen rejected — path traversal");
        return error_msg(SoxCmd::FileOpen, req.req_id, "invalid path");
    }

    // Canonicalize and verify the resolved path stays within allowed dirs
    let canonical = match std::fs::canonicalize(&local_path) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(path = %local_path, err = %e, "SOX: fileOpen failed");
            return error_msg(SoxCmd::FileOpen, req.req_id, "file not found");
        }
    };
    let canonical_str = canonical.to_string_lossy();
    if !canonical_str.starts_with(KITS_BASE_DIR) && !canonical_str.starts_with(MANIFESTS_DIR) {
        tracing::warn!(path = %canonical_str, "SOX: fileOpen rejected — outside allowed dirs");
        return error_msg(SoxCmd::FileOpen, req.req_id, "invalid path");
    }

    match std::fs::read(&canonical) {
        Ok(data) => {
            let file_size = data.len();
            let num_chunks = (file_size + SOX_CHUNK_SIZE - 1) / SOX_CHUNK_SIZE;

            tracing::info!(path = %canonical_str, size = file_size, chunks = num_chunks, "SOX: fileOpen OK");

            let mut xfer = SOX_FILE_XFER.lock().expect("SOX file xfer mutex poisoned");
            *xfer = Some(SoxFileXfer {
                data,
                chunk_size: SOX_CHUNK_SIZE,
            });

            let mut resp = SoxResponse::success(SoxCmd::FileOpen, req.req_id);
            resp.write_u32(file_size as u32);
            resp.write_u16(num_chunks as u16);
            resp.write_u16(SOX_CHUNK_SIZE as u16);
            resp
        }
        Err(e) => {
            tracing::warn!(path = %canonical_str, err = %e, "SOX: fileOpen read failed");
            error_msg(SoxCmd::FileOpen, req.req_id, "file not found")
        }
    }
}

/// fileRead ('g') — read a chunk of the open file.
/// Request: u2 chunkNum
/// Response: raw chunk bytes
fn handle_file_read(req: &SoxRequest) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let chunk_num = reader.read_u16().unwrap_or(0) as usize;

    let xfer = SOX_FILE_XFER.lock().expect("SOX file xfer mutex poisoned");
    if let Some(ref file) = *xfer {
        let start = match chunk_num.checked_mul(file.chunk_size) {
            Some(s) => s,
            None => return error_msg(SoxCmd::FileRead, req.req_id, "chunk out of range"),
        };
        let end = (start + file.chunk_size).min(file.data.len());

        if start < file.data.len() {
            let mut resp = SoxResponse::success(SoxCmd::FileRead, req.req_id);
            resp.write_bytes(&file.data[start..end]);
            resp
        } else {
            error_msg(SoxCmd::FileRead, req.req_id, "chunk out of range")
        }
    } else {
        error_msg(SoxCmd::FileRead, req.req_id, "no file open")
    }
}

/// fileClose ('q') — close the current file transfer.
fn handle_file_close(req: &SoxRequest) -> SoxResponse {
    let mut xfer = SOX_FILE_XFER.lock().expect("SOX file xfer mutex poisoned");
    *xfer = None;
    tracing::info!("SOX: fileClose");
    SoxResponse::success(SoxCmd::FileClose, req.req_id)
}

/// Create an error response with a message.
fn error_msg(cmd: SoxCmd, req_id: u8, msg: &str) -> SoxResponse {
    let mut resp = SoxResponse::error(cmd, req_id);
    resp.write_str(msg);
    resp
}

/// readSchema ('v') — return kit names + checksums.
///
/// Sedona SOX spec: u1 kitCount, then per kit: str name, i4 checksum
fn handle_read_schema(req: &SoxRequest) -> SoxResponse {
    let mut resp = SoxResponse::success(SoxCmd::ReadSchema, req.req_id);
    resp.write_u8(DEFAULT_KITS.len() as u8);
    for kit in DEFAULT_KITS {
        resp.write_str(kit.name);
        resp.write_u32(kit.checksum);
    }
    resp
}

/// readVersion ('y') — return platform ID, scode flags, kit versions, and properties.
///
/// Sedona SOX spec:
///   str platformId, u1 scodeFlags,
///   kits[kitCount] { str version },
///   u1 numProps, props[numProps] { str key, str value }
fn handle_read_version(req: &SoxRequest) -> SoxResponse {
    let mut resp = SoxResponse::success(SoxCmd::ReadVersion, req.req_id);
    resp.write_str("EacIo");   // platformId
    resp.write_u8(0x00);       // scodeFlags
    // Kit version strings (same order as readSchema)
    for kit in DEFAULT_KITS {
        resp.write_str(kit.version);
    }
    // Properties
    resp.write_u8(1);
    resp.write_str("soxVer");
    resp.write_str("1.1");
    resp
}

/// readComp ('c') -- read a component tree node.
///
/// Request payload: u2 compId, u1 what ('t'=tree, 'c'=config, 'r'=runtime, 'l'=links).
fn handle_read_comp(req: &SoxRequest, tree: &ComponentTree) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let comp_id = match reader.read_u16() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing compId"),
    };
    let what = reader.read_u8().unwrap_or(b't');

    let Some(comp) = tree.get(comp_id) else {
        return error_msg(req.cmd, req.req_id, "unknown comp");
    };

    let mut resp = SoxResponse::success(SoxCmd::ReadComp, req.req_id);
    resp.write_u16(comp.comp_id);
    resp.write_u8(what); // echo the 'what' byte back

    match what {
        b't' => {
            // Tree: kitId, typeId, name, parentId, permissions, children
            resp.write_u8(comp.kit_id);
            resp.write_u8(comp.type_id);
            resp.write_str(&comp.name);
            resp.write_u16(comp.parent_id);
            resp.write_u8(0xFF); // permissions (operator level)
            resp.write_u8(comp.children.len() as u8);
            for &child_id in &comp.children {
                resp.write_u16(child_id);
            }
            // Note: tree mode only includes structure, not property values.
            // Property values come from COV events or readComp what='c'/'r'.
        }
        b'c' => {
            // Config: only slots with CONFIG flag, in schema order.
            for slot in &comp.slots {
                if slot.flags & SLOT_FLAG_ACTION != 0 { continue; }
                if slot.flags & SLOT_FLAG_CONFIG == 0 { continue; } // skip non-config
                encode_slot_value(&mut resp, &slot.value);
            }
        }
        b'r' => {
            // Runtime: only slots WITHOUT CONFIG flag, in schema order.
            for slot in &comp.slots {
                if slot.flags & SLOT_FLAG_ACTION != 0 { continue; }
                if slot.flags & SLOT_FLAG_CONFIG != 0 { continue; } // skip config
                encode_slot_value(&mut resp, &slot.value);
            }
        }
        b'l' => {
            // Links: u1 numLinks, then link data.
            resp.write_u8(0); // no links
        }
        _ => {
            // Unknown what — return empty
        }
    }

    resp
}

/// readProp ('r') — read a single property value.
///
/// Request: u2 compId, u1 slotId
/// Response: 'R' + u2 compId + u1 slotId + u1 typeId + encoded value
fn handle_read_prop(req: &SoxRequest, tree: &ComponentTree) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let comp_id = match reader.read_u16() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing compId"),
    };
    let slot_id = match reader.read_u8() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing slotId"),
    };

    let Some(comp) = tree.get(comp_id) else {
        return error_msg(req.cmd, req.req_id, "unknown comp");
    };

    let Some(slot) = comp.slots.get(slot_id as usize) else {
        // For added components, return a default float 0.0
        let mut resp = SoxResponse::success(SoxCmd::ReadProp, req.req_id);
        resp.write_u16(comp_id);
        resp.write_u8(slot_id);
        resp.write_u8(SoxValueType::Float as u8);
        resp.write_f32(0.0);
        return resp;
    };

    let mut resp = SoxResponse::success(SoxCmd::ReadProp, req.req_id);
    resp.write_u16(comp_id);
    resp.write_u8(slot_id);
    resp.write_u8(slot.type_id);
    encode_slot_value(&mut resp, &slot.value);
    resp
}

/// subscribe ('s') — register for COV events.
///
/// Old protocol (doSubscribe): u2 compId, u1 what (e.g. 't', 'c', 'r', 'l')
///   Response: 'S' + replyNum + compId + what + component data
///
/// New protocol (batchSubscribe): u1 mask, u1 count, [u2 compId...]
///   Response: 'S' + replyNum + remaining(u1)
fn handle_subscribe(
    req: &SoxRequest,
    subs: &mut SubscriptionManager,
    session_id: u16,
    tree: &ComponentTree,
) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);

    // Detect old vs new format by payload length:
    // Old: 3 bytes (u2 compId + u1 what)
    // New: 2+ bytes (u1 mask + u1 count + optional u2 compId[])
    if req.payload.len() == 3 {
        // Old protocol (doSubscribe): u2 compId, u1 what
        let comp_id = reader.read_u16().unwrap_or(0);
        let what = reader.read_u8().unwrap_or(b't');

        subs.subscribe(session_id, comp_id);
        // Also subscribe descendants
        let mut all_ids = Vec::new();
        collect_subtree(tree, comp_id, &mut all_ids);
        for &id in &all_ids {
            subs.subscribe(session_id, id);
        }

        tracing::info!(session = session_id, comp_id, what_byte = what, "SOX: doSubscribe (old protocol)");

        // Response includes component data (same as readComp)
        let mut resp = SoxResponse::success(SoxCmd::Subscribe, req.req_id);
        if let Some(comp) = tree.get(comp_id) {
            resp.write_u16(comp_id);
            resp.write_u8(what);
            match what {
                b't' => {
                    resp.write_u8(comp.kit_id);
                    resp.write_u8(comp.type_id);
                    resp.write_str(&comp.name);
                    resp.write_u16(comp.parent_id);
                    resp.write_u8(0xFF);
                    resp.write_u8(comp.children.len() as u8);
                    for &child_id in &comp.children {
                        resp.write_u16(child_id);
                    }
                }
                b'c' => {
                    for slot in &comp.slots {
                        if slot.flags & SLOT_FLAG_ACTION != 0 { continue; }
                        if slot.flags & SLOT_FLAG_CONFIG == 0 { continue; }
                        encode_slot_value(&mut resp, &slot.value);
                    }
                }
                b'r' => {
                    for slot in &comp.slots {
                        if slot.flags & SLOT_FLAG_ACTION != 0 { continue; }
                        if slot.flags & SLOT_FLAG_CONFIG != 0 { continue; }
                        encode_slot_value(&mut resp, &slot.value);
                    }
                }
                b'l' => {
                    resp.write_u8(0);
                }
                _ => {}
            }
        }
        resp
    } else {
        // New protocol (batchSubscribe): u1 mask, u1 count, [u2 compId...]
        let mask = reader.read_u8().unwrap_or(0xFF);
        let count = reader.read_u8().unwrap_or(0);

        let mut comp_ids: Vec<u16> = Vec::new();
        if count == 0 {
            comp_ids = tree.comp_ids();
        } else {
            for _ in 0..count {
                if let Some(comp_id) = reader.read_u16() {
                    comp_ids.push(comp_id);
                }
            }
        }

        let mut all_ids: Vec<u16> = Vec::new();
        for &id in &comp_ids {
            collect_subtree(tree, id, &mut all_ids);
        }
        for &id in &all_ids {
            subs.subscribe(session_id, id);
        }
        tracing::info!(session = session_id, mask, requested = comp_ids.len(), total = all_ids.len(), "SOX: batchSubscribe");

        let mut resp = SoxResponse::success(SoxCmd::Subscribe, req.req_id);
        // remaining: number of pending events the client should wait for.
        // Set to min(total, 255) so the client blocks and processes initial COV events.
        resp.write_u8(all_ids.len().min(255) as u8);
        resp
    }
}

/// unsubscribe ('u') -- remove COV registration for a component.
///
/// Request payload: u2 compId, u1 whatMask.
fn handle_unsubscribe(
    req: &SoxRequest,
    subs: &mut SubscriptionManager,
    session_id: u16,
) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let comp_id = match reader.read_u16() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing compId"),
    };
    let _what_mask = reader.read_u8().unwrap_or(0xFF);

    subs.unsubscribe(session_id, comp_id);
    SoxResponse::success(SoxCmd::Unsubscribe, req.req_id)
}

/// add ('a') — add a new component to the tree.
///
/// Request: u2 parentId, u1 kitId, u1 typeId, str name, [configValues...]
/// Response: 'A' + u2 newCompId
fn handle_add(req: &SoxRequest, tree: &mut ComponentTree) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let parent_id = match reader.read_u16() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing parentId"),
    };
    let kit_id = reader.read_u8().unwrap_or(0);
    let type_id = reader.read_u8().unwrap_or(0);
    let name = reader.read_str().unwrap_or_default();

    // Verify parent exists
    if tree.get(parent_id).is_none() {
        return error_msg(req.cmd, req.req_id, "bad compId");
    }

    let new_id = tree.next_comp_id();
    let comp = VirtualComponent {
        comp_id: new_id,
        parent_id,
        name,
        type_name: format!("kit{}::type{}", kit_id, type_id),
        kit_id,
        type_id,
        children: Vec::new(),
        slots: Vec::new(), // config values could be parsed from remaining bytes
    };
    tree.add(comp);

    tracing::info!(new_id, parent_id, kit_id, type_id, "SOX: component added");

    let mut resp = SoxResponse::success(SoxCmd::Add, req.req_id);
    resp.write_u16(new_id);
    resp
}

/// delete ('d') — remove a component from the tree.
///
/// Request: u2 compId
/// Response: 'D'
fn handle_delete(req: &SoxRequest, tree: &mut ComponentTree) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let comp_id = match reader.read_u16() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing compId"),
    };

    // Don't allow deleting the root app (comp 0) or service nodes
    if comp_id < 7 {
        return error_msg(req.cmd, req.req_id, "cannot delete system component");
    }

    if tree.remove(comp_id).is_some() {
        tracing::info!(comp_id, "SOX: component deleted");
        SoxResponse::success(SoxCmd::Delete, req.req_id)
    } else {
        error_msg(req.cmd, req.req_id, "bad compId")
    }
}

/// rename ('r') — rename a component.
///
/// Request: u2 compId, str newName
/// Response: 'R'
fn handle_rename(req: &SoxRequest, tree: &mut ComponentTree) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let comp_id = match reader.read_u16() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing compId"),
    };
    let new_name = reader.read_str().unwrap_or_default();

    if tree.rename(comp_id, new_name.clone()) {
        tracing::info!(comp_id, name = %new_name, "SOX: component renamed");
        SoxResponse::success(SoxCmd::Rename, req.req_id)
    } else {
        error_msg(req.cmd, req.req_id, "bad compId")
    }
}

/// invoke ('k') — invoke an action on a component.
///
/// Request: u2 compId, u1 slotId, [argValue]
/// Response: 'K'
///
/// For "set" actions (like ConstFloat.set), parse the float argument
/// and update the component's "out" slot value in the tree.
fn handle_invoke(req: &SoxRequest, tree: &mut ComponentTree) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let comp_id = reader.read_u16().unwrap_or(0);
    let slot_id = reader.read_u8().unwrap_or(0);

    // Try to parse a float argument (common for "set" actions)
    let arg_value = reader.read_f32();

    if let Some(val) = arg_value {
        tracing::info!(comp_id, slot_id, value = val, "SOX: invoke set action");
        // Update the component's slots — find a float slot named "out" or use slot 1
        if let Some(comp) = tree.get_mut(comp_id) {
            // Look for "out" slot or the first float slot after actions
            for slot in comp.slots.iter_mut() {
                if slot.name == "out" || (matches!(slot.value, SlotValue::Float(_))) {
                    slot.value = SlotValue::Float(val);
                    tracing::info!(comp_id, slot = %slot.name, value = val, "SOX: set action applied");
                    break;
                }
            }
        }
    } else {
        tracing::info!(comp_id, slot_id, "SOX: invoke action (no arg)");
    }

    SoxResponse::success(SoxCmd::Invoke, req.req_id)
}

/// write ('w') -- write a slot value on a component.
///
/// Request payload: u2 compId, u1 slotId, value (NO typeId prefix).
/// The Java editor's `val.encodeBinary(req)` writes the value directly
/// without a type discriminator. We determine the type from the existing slot.
fn handle_write(req: &SoxRequest, tree: &mut ComponentTree) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let comp_id = match reader.read_u16() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing compId"),
    };
    let slot_id = match reader.read_u8() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing slotId"),
    };

    // Determine the slot type from the existing component schema.
    // If the slot doesn't exist yet, try to decode as float (most common).
    let type_id = tree.get(comp_id)
        .and_then(|c| c.slots.get(slot_id as usize))
        .map(|s| s.type_id)
        .unwrap_or(SoxValueType::Float as u8);

    let value = match decode_slot_value(&mut reader, type_id) {
        Some(v) => v,
        None => return error_msg(req.cmd, req.req_id, "bad value"),
    };

    // Verify the component exists.
    let Some(comp) = tree.get(comp_id) else {
        return error_msg(req.cmd, req.req_id, "unknown comp");
    };
    let _ = comp; // drop immutable borrow

    // Update or create the slot value in the tree.
    if let Some(comp) = tree.get_mut(comp_id) {
        // Auto-extend slots if needed (for dynamically added components)
        while comp.slots.len() <= slot_id as usize {
            comp.slots.push(VirtualSlot {
                name: format!("slot{}", comp.slots.len()),
                type_id: type_id,
                flags: SLOT_FLAG_RUNTIME,
                value: SlotValue::Null,
            });
        }
        let slot = &mut comp.slots[slot_id as usize];
        slot.type_id = type_id;
        tracing::info!(comp_id, slot_id, name = %slot.name, ?value, "SOX: slot value updated");
        slot.value = value;
    }

    // Response: just 'W' + replyNum (no extra data — Java client only reads these 2 bytes)
    SoxResponse::success(SoxCmd::Write, req.req_id)
}

/// Extract write request details from a SOX write command.
///
/// Returns a `WriteRequest` if the request is a valid write.
/// The tree is needed to determine the slot's type (no typeId on the wire).
pub fn parse_write_request(req: &SoxRequest, tree: &ComponentTree) -> Option<WriteRequest> {
    if req.cmd != SoxCmd::Write {
        return None;
    }
    let mut reader = SoxReader::new(&req.payload);
    let comp_id = reader.read_u16()?;
    let slot_id = reader.read_u8()?;
    let type_id = tree.get(comp_id)
        .and_then(|c| c.slots.get(slot_id as usize))
        .map(|s| s.type_id)
        .unwrap_or(SoxValueType::Float as u8);
    let value = decode_slot_value(&mut reader, type_id)?;
    Some(WriteRequest {
        comp_id,
        slot_id,
        value,
    })
}

/// A parsed write request that can be forwarded to the engine.
#[derive(Debug, Clone)]
pub struct WriteRequest {
    pub comp_id: u16,
    pub slot_id: u8,
    pub value: SlotValue,
}

impl WriteRequest {
    /// If this write targets a channel-mapped component, return the
    /// engine channel ID and the float value to write.
    ///
    /// Channel components have comp_id >= CHANNEL_COMP_BASE.
    /// Slot 6 ("out") is the primary writable output.
    pub fn to_channel_write(&self, tree: &ComponentTree) -> Option<(u32, f64)> {
        if self.comp_id < CHANNEL_COMP_BASE {
            return None;
        }
        // Only slot 6 ("out") is writable for channel components.
        if self.slot_id != 6 {
            return None;
        }
        let comp = tree.get(self.comp_id)?;
        // Extract channel ID from the "channel" slot (index 2).
        let channel_id = match &comp.slots.get(2)?.value {
            SlotValue::Int(id) => *id as u32,
            _ => return None,
        };
        let value = match &self.value {
            SlotValue::Float(v) => *v as f64,
            SlotValue::Double(v) => *v,
            SlotValue::Int(v) => *v as f64,
            _ => return None,
        };
        Some((channel_id, value))
    }
}

// ---- Tests ----

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_channels() -> Vec<ChannelInfo> {
        vec![
            ChannelInfo {
                id: 1113,
                label: "AI1 10K Therm".into(),
                channel_type: "analog".into(),
                direction: "AI".into(),
                enabled: true,
                status: "ok".into(),
                cur: 72.5,
                raw: 2048.0,
            },
            ChannelInfo {
                id: 1713,
                label: "AI7 Wall Temp".into(),
                channel_type: "analog".into(),
                direction: "AI".into(),
                enabled: true,
                status: "ok".into(),
                cur: 78.0,
                raw: 1900.0,
            },
            ChannelInfo {
                id: 2100,
                label: "DI1 Status".into(),
                channel_type: "digital".into(),
                direction: "DI".into(),
                enabled: true,
                status: "ok".into(),
                cur: 1.0,
                raw: 1.0,
            },
        ]
    }

    // ---- ComponentTree tests ----

    #[test]
    fn tree_new_is_empty() {
        let tree = ComponentTree::new();
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
    }

    #[test]
    fn tree_add_and_get() {
        let mut tree = ComponentTree::new();
        tree.add(VirtualComponent {
            comp_id: 0,
            parent_id: NO_PARENT,
            name: "app".into(),
            type_name: "sys::App".into(),
            kit_id: 0,
            type_id: 0,
            children: Vec::new(),
            slots: Vec::new(),
        });
        assert_eq!(tree.len(), 1);
        assert!(!tree.is_empty());
        let comp = tree.get(0).unwrap();
        assert_eq!(comp.name, "app");
        assert_eq!(comp.parent_id, NO_PARENT);
    }

    #[test]
    fn tree_parent_child_registration() {
        let mut tree = ComponentTree::new();
        tree.add(VirtualComponent {
            comp_id: 0,
            parent_id: NO_PARENT,
            name: "app".into(),
            type_name: "sys::App".into(),
            kit_id: 0,
            type_id: 0,
            children: Vec::new(),
            slots: Vec::new(),
        });
        tree.add(VirtualComponent {
            comp_id: 1,
            parent_id: 0,
            name: "service".into(),
            type_name: "sys::Folder".into(),
            kit_id: 0,
            type_id: 1,
            children: Vec::new(),
            slots: Vec::new(),
        });
        let root = tree.get(0).unwrap();
        assert_eq!(root.children, vec![1]);
    }

    #[test]
    fn from_channels_creates_root() {
        let tree = ComponentTree::from_channels(&[]);
        let root = tree.get(0).unwrap();
        assert_eq!(root.name, "app");
        assert_eq!(root.type_name, "sys::App");
        assert_eq!(root.parent_id, NO_PARENT);
    }

    #[test]
    fn from_channels_creates_service_folder() {
        let tree = ComponentTree::from_channels(&[]);
        let svc = tree.get(1).unwrap();
        assert_eq!(svc.name, "service");
        assert_eq!(svc.parent_id, 0);
        assert!(svc.children.contains(&2));
        assert!(svc.children.contains(&3));
        assert!(svc.children.contains(&4));
    }

    #[test]
    fn from_channels_creates_io_folder() {
        let tree = ComponentTree::from_channels(&[]);
        let io = tree.get(5).unwrap();
        assert_eq!(io.name, "io");
        assert_eq!(io.parent_id, 0);
    }

    #[test]
    fn from_channels_creates_control_folder() {
        let tree = ComponentTree::from_channels(&[]);
        let ctrl = tree.get(6).unwrap();
        assert_eq!(ctrl.name, "control");
        assert_eq!(ctrl.parent_id, 0);
    }

    #[test]
    fn from_channels_builds_correct_tree_structure() {
        let channels = sample_channels();
        let tree = ComponentTree::from_channels(&channels);

        // 7 fixed nodes + 3 channel nodes = 10
        assert_eq!(tree.len(), 10);

        // Root children: service(1), io(5), control(6)
        let root = tree.get(0).unwrap();
        assert!(root.children.contains(&1));
        assert!(root.children.contains(&5));
        assert!(root.children.contains(&6));
    }

    #[test]
    fn from_channels_maps_channels_to_components() {
        let channels = sample_channels();
        let tree = ComponentTree::from_channels(&channels);

        let ch1 = tree.get(100).unwrap();
        assert_eq!(ch1.name, "ch_1113");
        assert_eq!(ch1.parent_id, 5);
        assert_eq!(ch1.type_name, "control::NumericPoint");

        let ch2 = tree.get(101).unwrap();
        assert_eq!(ch2.name, "ch_1713");

        let ch3 = tree.get(102).unwrap();
        assert_eq!(ch3.name, "ch_2100");
        assert_eq!(ch3.type_name, "control::BooleanPoint");
    }

    #[test]
    fn channel_component_has_correct_slots() {
        let channels = sample_channels();
        let tree = ComponentTree::from_channels(&channels);

        let ch = tree.get(100).unwrap();
        // 9 slots: meta, channelName, channel, pointQuery, pointQuerySize,
        //          pointQueryStatus, out, curStatus, enabled
        assert_eq!(ch.slots.len(), 9);
        assert_eq!(ch.slots[0].name, "meta");
        assert_eq!(ch.slots[1].name, "channelName");
        assert_eq!(ch.slots[2].name, "channel");
        assert_eq!(ch.slots[3].name, "pointQuery");
        assert_eq!(ch.slots[4].name, "pointQuerySize");
        assert_eq!(ch.slots[5].name, "pointQueryStatus");
        assert_eq!(ch.slots[6].name, "out");
        assert_eq!(ch.slots[7].name, "curStatus");
        assert_eq!(ch.slots[8].name, "enabled");

        assert_eq!(ch.slots[0].value, SlotValue::Int(1)); // meta
        assert_eq!(ch.slots[2].value, SlotValue::Int(1113)); // channel
        assert_eq!(ch.slots[6].value, SlotValue::Float(72.5)); // out
        assert_eq!(ch.slots[8].value, SlotValue::Bool(true)); // enabled
    }

    #[test]
    fn io_folder_has_channel_children() {
        let channels = sample_channels();
        let tree = ComponentTree::from_channels(&channels);
        let io = tree.get(5).unwrap();
        assert_eq!(io.children.len(), 3);
        assert!(io.children.contains(&100));
        assert!(io.children.contains(&101));
        assert!(io.children.contains(&102));
    }

    #[test]
    fn tree_with_10_channels_has_correct_count() {
        let channels: Vec<ChannelInfo> = (0..10)
            .map(|i| ChannelInfo {
                id: 1000 + i,
                label: format!("ch_{}", 1000 + i),
                channel_type: "analog".into(),
                direction: "AI".into(),
                enabled: true,
                status: "ok".into(),
                cur: i as f64,
                raw: i as f64,
            })
            .collect();
        let tree = ComponentTree::from_channels(&channels);
        // 7 fixed + 10 channel = 17
        assert_eq!(tree.len(), 17);
    }

    #[test]
    fn update_from_channels_detects_changes() {
        let channels = sample_channels();
        let mut tree = ComponentTree::from_channels(&channels);

        let mut updated = sample_channels();
        updated[0].cur = 75.0;
        let changed = tree.update_from_channels(&updated);
        assert!(changed.contains(&100));
        assert!(!changed.contains(&101));
    }

    #[test]
    fn update_from_channels_no_changes() {
        let channels = sample_channels();
        let mut tree = ComponentTree::from_channels(&channels);
        let changed = tree.update_from_channels(&channels);
        assert!(changed.is_empty());
    }

    // ---- Status conversion tests ----

    #[test]
    fn status_to_int_converts_correctly() {
        assert_eq!(status_to_int("ok"), 0);
        assert_eq!(status_to_int("OK"), 0);
        assert_eq!(status_to_int("fault"), 1);
        assert_eq!(status_to_int("down"), 2);
        assert_eq!(status_to_int("disabled"), 3);
        assert_eq!(status_to_int("stale"), 4);
        assert_eq!(status_to_int("unknown"), 1);
    }

    #[test]
    fn int_to_status_converts_correctly() {
        assert_eq!(int_to_status(0), "ok");
        assert_eq!(int_to_status(1), "fault");
        assert_eq!(int_to_status(2), "down");
        assert_eq!(int_to_status(3), "disabled");
        assert_eq!(int_to_status(4), "stale");
        assert_eq!(int_to_status(99), "fault");
    }

    // ---- Slot value encoding tests ----

    #[test]
    fn encode_bool_value() {
        let mut resp = SoxResponse::success(SoxCmd::Event, 0);
        encode_slot_value(&mut resp, &SlotValue::Bool(true));
        assert_eq!(resp.payload, vec![1]);
    }

    #[test]
    fn encode_int_value() {
        let mut resp = SoxResponse::success(SoxCmd::Event, 0);
        encode_slot_value(&mut resp, &SlotValue::Int(256));
        assert_eq!(resp.payload, 256i32.to_be_bytes().to_vec());
    }

    #[test]
    fn encode_float_value() {
        let mut resp = SoxResponse::success(SoxCmd::Event, 0);
        encode_slot_value(&mut resp, &SlotValue::Float(1.5));
        assert_eq!(resp.payload, 1.5f32.to_be_bytes().to_vec());
    }

    #[test]
    fn encode_str_value() {
        let mut resp = SoxResponse::success(SoxCmd::Event, 0);
        encode_slot_value(&mut resp, &SlotValue::Str("hi".into()));
        // Sedona Str format: u2(len+1) + bytes + 0x00
        assert_eq!(resp.payload, vec![0, 3, b'h', b'i', 0x00]);
    }

    #[test]
    fn encode_null_value() {
        let mut resp = SoxResponse::success(SoxCmd::Event, 0);
        encode_slot_value(&mut resp, &SlotValue::Null);
        assert!(resp.payload.is_empty());
    }

    #[test]
    fn encode_buf_value() {
        let mut resp = SoxResponse::success(SoxCmd::Event, 0);
        encode_slot_value(&mut resp, &SlotValue::Buf(vec![0xAA, 0xBB]));
        assert_eq!(resp.payload, vec![0, 2, 0xAA, 0xBB]);
    }

    #[test]
    fn encode_long_value() {
        let mut resp = SoxResponse::success(SoxCmd::Event, 0);
        encode_slot_value(&mut resp, &SlotValue::Long(0x0102030405060708));
        assert_eq!(resp.payload, 0x0102030405060708i64.to_be_bytes().to_vec());
    }

    #[test]
    fn encode_double_value() {
        let mut resp = SoxResponse::success(SoxCmd::Event, 0);
        encode_slot_value(&mut resp, &SlotValue::Double(3.14));
        assert_eq!(resp.payload, 3.14f64.to_be_bytes().to_vec());
    }

    // ---- Decode slot value tests ----

    #[test]
    fn decode_bool_value() {
        let data = [1u8];
        let mut r = SoxReader::new(&data);
        assert_eq!(
            decode_slot_value(&mut r, SoxValueType::Bool as u8),
            Some(SlotValue::Bool(true))
        );
    }

    #[test]
    fn decode_int_value() {
        let data = 42i32.to_be_bytes();
        let mut r = SoxReader::new(&data);
        assert_eq!(
            decode_slot_value(&mut r, SoxValueType::Int as u8),
            Some(SlotValue::Int(42))
        );
    }

    #[test]
    fn decode_float_value() {
        let data = 72.5f32.to_be_bytes();
        let mut r = SoxReader::new(&data);
        let val = decode_slot_value(&mut r, SoxValueType::Float as u8).unwrap();
        match val {
            SlotValue::Float(v) => assert!((v - 72.5).abs() < 0.001),
            _ => panic!("expected Float"),
        }
    }

    #[test]
    fn decode_buf_value() {
        let mut data = Vec::new();
        data.extend_from_slice(&3u16.to_be_bytes());
        data.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        let mut r = SoxReader::new(&data);
        assert_eq!(
            decode_slot_value(&mut r, SoxValueType::Buf as u8),
            Some(SlotValue::Buf(vec![0xAA, 0xBB, 0xCC]))
        );
    }

    // ---- Handler tests ----

    #[test]
    fn handle_read_schema_response_format() {
        let req = SoxRequest {
            cmd: SoxCmd::ReadSchema,
            req_id: 10,
            payload: Vec::new(),
        };
        let resp = handle_read_schema(&req);
        let bytes = resp.to_bytes();
        assert_eq!(bytes[0], b'V');
        assert_eq!(bytes[1], 10);
        assert_eq!(bytes[2], DEFAULT_KITS.len() as u8);
    }

    #[test]
    fn handle_read_version_response() {
        let req = SoxRequest {
            cmd: SoxCmd::ReadVersion,
            req_id: 5,
            payload: Vec::new(),
        };
        let resp = handle_read_version(&req);
        assert_eq!(resp.cmd, b'Y');
        assert_eq!(resp.req_id, 5);
        // Payload starts with null-terminated platformId "EacIo"
        let mut r = SoxReader::new(&resp.payload);
        assert_eq!(r.read_str(), Some("EacIo".into())); // platformId
        assert_eq!(r.read_u8(), Some(0x00)); // scodeFlags
    }

    #[test]
    fn handle_read_comp_root() {
        let tree = ComponentTree::from_channels(&sample_channels());
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u16.to_be_bytes());
        payload.push(b't');
        let req = SoxRequest {
            cmd: SoxCmd::ReadComp,
            req_id: 1,
            payload,
        };
        let resp = handle_read_comp(&req, &tree);
        assert_eq!(resp.cmd, b'C');
        let mut r = SoxReader::new(&resp.payload);
        assert_eq!(r.read_u16(), Some(0)); // comp_id
        assert_eq!(r.read_u8(), Some(b't')); // what byte echoed back
        assert_eq!(r.read_u8(), Some(0)); // kit_id (sys)
        assert_eq!(r.read_u8(), Some(10)); // type_id (App)
        assert_eq!(r.read_str(), Some("app".into()));
        assert_eq!(r.read_u16(), Some(NO_PARENT));
        assert_eq!(r.read_u8(), Some(0xFF)); // permissions
        let child_count = r.read_u8().unwrap();
        assert_eq!(child_count, 3); // service, io, control
    }

    #[test]
    fn handle_read_comp_channel() {
        let tree = ComponentTree::from_channels(&sample_channels());
        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(b'c');
        let req = SoxRequest {
            cmd: SoxCmd::ReadComp,
            req_id: 2,
            payload,
        };
        let resp = handle_read_comp(&req, &tree);
        assert_eq!(resp.cmd, b'C');
        let mut r = SoxReader::new(&resp.payload);
        assert_eq!(r.read_u16(), Some(100));
    }

    #[test]
    fn handle_read_comp_nonexistent() {
        let tree = ComponentTree::from_channels(&[]);
        let mut payload = Vec::new();
        payload.extend_from_slice(&999u16.to_be_bytes());
        payload.push(b't');
        let req = SoxRequest {
            cmd: SoxCmd::ReadComp,
            req_id: 3,
            payload,
        };
        let resp = handle_read_comp(&req, &tree);
        assert_eq!(resp.cmd, b'!');
    }

    #[test]
    fn handle_read_comp_links_mode() {
        let tree = ComponentTree::from_channels(&sample_channels());
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u16.to_be_bytes());
        payload.push(b'l');
        let req = SoxRequest {
            cmd: SoxCmd::ReadComp,
            req_id: 4,
            payload,
        };
        let resp = handle_read_comp(&req, &tree);
        assert_eq!(resp.cmd, b'C');
        // Last byte of payload should be 0 (no links)
        assert_eq!(*resp.payload.last().unwrap(), 0);
    }

    // ---- Subscription tests ----

    #[test]
    fn subscribe_adds_watcher() {
        let mut subs = SubscriptionManager::new();
        subs.subscribe(1, 100);
        assert!(subs.is_subscribed(1, 100));
        assert_eq!(subs.total_subscriptions(), 1);
    }

    #[test]
    fn subscribe_multiple_sessions() {
        let mut subs = SubscriptionManager::new();
        subs.subscribe(1, 100);
        subs.subscribe(2, 100);
        let watchers = subs.get_watchers(100).unwrap();
        assert_eq!(watchers.len(), 2);
        assert!(watchers.contains(&1));
        assert!(watchers.contains(&2));
    }

    #[test]
    fn unsubscribe_removes_watcher() {
        let mut subs = SubscriptionManager::new();
        subs.subscribe(1, 100);
        subs.subscribe(1, 101);
        subs.unsubscribe(1, 100);
        assert!(!subs.is_subscribed(1, 100));
        assert!(subs.is_subscribed(1, 101));
    }

    #[test]
    fn unsubscribe_all_cleans_session() {
        let mut subs = SubscriptionManager::new();
        subs.subscribe(1, 100);
        subs.subscribe(1, 101);
        subs.subscribe(1, 102);
        subs.subscribe(2, 100);
        subs.unsubscribe_all(1);
        assert!(!subs.is_subscribed(1, 100));
        assert!(!subs.is_subscribed(1, 101));
        assert!(!subs.is_subscribed(1, 102));
        assert!(subs.is_subscribed(2, 100));
    }

    #[test]
    fn unsubscribe_nonexistent_is_noop() {
        let mut subs = SubscriptionManager::new();
        subs.unsubscribe(99, 100);
        subs.unsubscribe_all(99);
        assert_eq!(subs.total_subscriptions(), 0);
    }

    #[test]
    fn session_components_returns_comps() {
        let mut subs = SubscriptionManager::new();
        subs.subscribe(1, 100);
        subs.subscribe(1, 200);
        let comps = subs.session_components(1).unwrap();
        assert_eq!(comps.len(), 2);
        assert!(comps.contains(&100));
        assert!(comps.contains(&200));
    }

    // ---- COV event tests ----

    #[test]
    fn build_events_for_subscribed_components() {
        let channels = sample_channels();
        let tree = ComponentTree::from_channels(&channels);
        let mut subs = SubscriptionManager::new();
        subs.subscribe(1, 100);

        let events = subs.build_events(&[100], &tree);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, 1); // session_id
        // Event payload starts with 'e' (lowercase unsolicited event), 0xFF
        assert_eq!(events[0].1[0], b'e');
        assert_eq!(events[0].1[1], 0xFF);
    }

    #[test]
    fn build_events_no_watchers() {
        let channels = sample_channels();
        let tree = ComponentTree::from_channels(&channels);
        let subs = SubscriptionManager::new();
        let events = subs.build_events(&[100], &tree);
        assert!(events.is_empty());
    }

    #[test]
    fn build_events_multiple_watchers() {
        let channels = sample_channels();
        let tree = ComponentTree::from_channels(&channels);
        let mut subs = SubscriptionManager::new();
        subs.subscribe(1, 100);
        subs.subscribe(2, 100);
        let events = subs.build_events(&[100], &tree);
        assert_eq!(events.len(), 2);
        let session_ids: HashSet<u16> = events.iter().map(|(s, _)| *s).collect();
        assert!(session_ids.contains(&1));
        assert!(session_ids.contains(&2));
    }

    // ---- Handler integration tests ----

    #[test]
    fn handle_subscribe_via_request() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        let mut subs = SubscriptionManager::new();
        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(0xFF);
        let req = SoxRequest {
            cmd: SoxCmd::Subscribe,
            req_id: 20,
            payload,
        };
        let resp = handle_sox_request(&req, &mut tree, &mut subs, 1);
        assert_eq!(resp.cmd, b'S');
        assert!(subs.is_subscribed(1, 100));
    }

    #[test]
    fn handle_unsubscribe_via_request() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        let mut subs = SubscriptionManager::new();
        subs.subscribe(1, 100);
        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(0xFF);
        let req = SoxRequest {
            cmd: SoxCmd::Unsubscribe,
            req_id: 21,
            payload,
        };
        let resp = handle_sox_request(&req, &mut tree, &mut subs, 1);
        assert_eq!(resp.cmd, b'U');
        assert!(!subs.is_subscribed(1, 100));
    }

    #[test]
    fn handle_write_valid_slot() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        let mut subs = SubscriptionManager::new();
        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(6); // slot_id (out)
        payload.extend_from_slice(&75.0f32.to_be_bytes());
        let req = SoxRequest {
            cmd: SoxCmd::Write,
            req_id: 30,
            payload,
        };
        let resp = handle_sox_request(&req, &mut tree, &mut subs, 1);
        assert_eq!(resp.cmd, b'W');
    }

    #[test]
    fn handle_write_auto_extends_slots() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        let mut subs = SubscriptionManager::new();
        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(99); // slot beyond current len → auto-extended
        payload.extend_from_slice(&42.0f32.to_be_bytes());
        let req = SoxRequest {
            cmd: SoxCmd::Write,
            req_id: 31,
            payload,
        };
        let resp = handle_sox_request(&req, &mut tree, &mut subs, 1);
        assert_eq!(resp.cmd, b'W'); // success — slot auto-created
        // Verify the slot was created with the written value
        let comp = tree.get(100).unwrap();
        assert!(comp.slots.len() > 99);
        assert_eq!(comp.slots[99].value, SlotValue::Float(42.0));
    }

    #[test]
    fn handle_write_unknown_comp() {
        let mut tree = ComponentTree::from_channels(&[]);
        let mut subs = SubscriptionManager::new();
        let mut payload = Vec::new();
        payload.extend_from_slice(&999u16.to_be_bytes());
        payload.push(0);
        payload.extend_from_slice(&0.0f32.to_be_bytes());
        let req = SoxRequest {
            cmd: SoxCmd::Write,
            req_id: 32,
            payload,
        };
        let resp = handle_sox_request(&req, &mut tree, &mut subs, 1);
        assert_eq!(resp.cmd, b'!');
    }

    #[test]
    fn handle_unsupported_command() {
        let mut tree = ComponentTree::from_channels(&[]);
        let mut subs = SubscriptionManager::new();
        let req = SoxRequest {
            cmd: SoxCmd::Add,
            req_id: 40,
            payload: Vec::new(),
        };
        let resp = handle_sox_request(&req, &mut tree, &mut subs, 1);
        assert_eq!(resp.cmd, b'!');
    }

    // ---- WriteRequest tests ----

    #[test]
    fn parse_write_request_valid() {
        let tree = ComponentTree::from_channels(&sample_channels());
        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(6); // slot_id=6 ("out", Float type)
        payload.extend_from_slice(&72.5f32.to_be_bytes());
        let req = SoxRequest {
            cmd: SoxCmd::Write,
            req_id: 0,
            payload,
        };
        let wr = parse_write_request(&req, &tree).unwrap();
        assert_eq!(wr.comp_id, 100);
        assert_eq!(wr.slot_id, 6);
        assert_eq!(wr.value, SlotValue::Float(72.5));
    }

    #[test]
    fn parse_write_request_non_write_cmd() {
        let tree = ComponentTree::from_channels(&[]);
        let req = SoxRequest {
            cmd: SoxCmd::ReadComp,
            req_id: 0,
            payload: Vec::new(),
        };
        assert!(parse_write_request(&req, &tree).is_none());
    }

    #[test]
    fn write_request_to_channel_write() {
        let channels = sample_channels();
        let tree = ComponentTree::from_channels(&channels);
        let wr = WriteRequest {
            comp_id: 100,
            slot_id: 6, // "out" slot (index 6 in new layout)
            value: SlotValue::Float(80.0),
        };
        let (ch_id, val) = wr.to_channel_write(&tree).unwrap();
        assert_eq!(ch_id, 1113);
        assert!((val - 80.0).abs() < 0.001);
    }

    #[test]
    fn write_request_non_channel_comp() {
        let tree = ComponentTree::from_channels(&sample_channels());
        let wr = WriteRequest {
            comp_id: 0,
            slot_id: 1,
            value: SlotValue::Float(0.0),
        };
        assert!(wr.to_channel_write(&tree).is_none());
    }

    #[test]
    fn write_request_wrong_slot() {
        let tree = ComponentTree::from_channels(&sample_channels());
        let wr = WriteRequest {
            comp_id: 100,
            slot_id: 0, // "meta" slot, not the writable "out" slot
            value: SlotValue::Float(0.0),
        };
        assert!(wr.to_channel_write(&tree).is_none());
    }

    // ---- SlotValue type_id tests ----

    #[test]
    fn slot_value_type_ids() {
        assert_eq!(SlotValue::Bool(true).type_id(), SoxValueType::Bool as u8);
        assert_eq!(SlotValue::Int(0).type_id(), SoxValueType::Int as u8);
        assert_eq!(SlotValue::Long(0).type_id(), SoxValueType::Long as u8);
        assert_eq!(SlotValue::Float(0.0).type_id(), SoxValueType::Float as u8);
        assert_eq!(SlotValue::Double(0.0).type_id(), SoxValueType::Double as u8);
        assert_eq!(SlotValue::Buf(vec![]).type_id(), SoxValueType::Buf as u8);
        assert_eq!(SlotValue::Null.type_id(), SoxValueType::Void as u8);
    }

    // ---- Channel ID preservation test ----

    #[test]
    fn channel_to_component_preserves_ids() {
        let channels = vec![ChannelInfo {
            id: 5555,
            label: "special".into(),
            channel_type: "analog".into(),
            direction: "AI".into(),
            enabled: true,
            status: "ok".into(),
            cur: 42.0,
            raw: 2000.0,
        }];
        let tree = ComponentTree::from_channels(&channels);
        let comp = tree.get(100).unwrap();
        // channel ID is now at slot index 2 ("channel")
        assert_eq!(comp.slots[2].value, SlotValue::Int(5555));
    }

    // ---- Error response tests ----

    #[test]
    fn error_msg_includes_text() {
        let resp = error_msg(SoxCmd::Write, 7, "bad slot");
        let bytes = resp.to_bytes();
        assert_eq!(bytes[0], b'!');
        assert_eq!(bytes[1], 7);
        // write_str is null-terminated: "bad slot" + 0x00
        assert_eq!(bytes[2], b'b'); // first char of "bad slot"
        assert_eq!(bytes[bytes.len() - 1], 0x00); // null terminator
        assert_eq!(bytes.len(), 2 + 8 + 1); // header + "bad slot" + NUL
    }

    #[test]
    fn handle_read_comp_missing_comp_id() {
        let tree = ComponentTree::from_channels(&[]);
        let req = SoxRequest {
            cmd: SoxCmd::ReadComp,
            req_id: 50,
            payload: Vec::new(), // no comp_id
        };
        let resp = handle_read_comp(&req, &tree);
        assert_eq!(resp.cmd, b'!');
    }

    #[test]
    fn handle_read_comp_config_mode_includes_slots() {
        let tree = ComponentTree::from_channels(&sample_channels());
        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(b'c'); // config mode
        let req = SoxRequest {
            cmd: SoxCmd::ReadComp,
            req_id: 60,
            payload,
        };
        let resp = handle_read_comp(&req, &tree);
        assert_eq!(resp.cmd, b'C');
        // Config mode: comp_id + what + config slot values (no tree structure)
        let mut r = SoxReader::new(&resp.payload);
        assert_eq!(r.read_u16(), Some(100)); // comp_id
        assert_eq!(r.read_u8(), Some(b'c')); // what byte echoed back
        // Config slots: meta (Int=1) and pointQuery (Str="")
        // meta: i4(1)
        assert_eq!(r.read_i32(), Some(1));
        // pointQuery: u2(1) + 0x00 (empty string with null terminator)
        assert_eq!(r.read_u16(), Some(1)); // size=1 (just the null)
        assert_eq!(r.read_u8(), Some(0x00)); // null terminator
    }
}
