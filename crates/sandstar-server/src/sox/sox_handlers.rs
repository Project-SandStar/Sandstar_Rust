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
}

impl ComponentTree {
    /// Create an empty tree.
    pub fn new() -> Self {
        Self {
            components: HashMap::new(),
            next_id: 0,
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
    #[allow(dead_code)]
    pub fn comp_ids(&self) -> Vec<u16> {
        self.components.keys().copied().collect()
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

        tree
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
            value: SlotValue::Str(format!("{:?}", ch.status)),
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
            resp.write_str(v);
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
fn encode_slot_value_raw(buf: &mut Vec<u8>, value: &SlotValue) {
    match value {
        SlotValue::Bool(v) => buf.push(if *v { 1 } else { 0 }),
        SlotValue::Int(v) => buf.extend_from_slice(&v.to_be_bytes()),
        SlotValue::Long(v) => buf.extend_from_slice(&v.to_be_bytes()),
        SlotValue::Float(v) => buf.extend_from_slice(&v.to_be_bytes()),
        SlotValue::Double(v) => buf.extend_from_slice(&v.to_be_bytes()),
        SlotValue::Str(s) => {
            // Buf/Str property: u2 length + raw bytes (Sedona Buf binary encoding)
            let bytes = s.as_bytes();
            buf.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            buf.extend_from_slice(bytes);
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
    /// Build COV event payloads for changed components.
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
    tree: &ComponentTree,
    subscriptions: &mut SubscriptionManager,
    session_id: u16,
) -> SoxResponse {
    match request.cmd {
        SoxCmd::ReadSchema | SoxCmd::ReadSchemaDetail => handle_read_schema(request),
        SoxCmd::ReadVersion => handle_read_version(request),
        SoxCmd::ReadComp => handle_read_comp(request, tree),
        SoxCmd::Subscribe => handle_subscribe(request, subscriptions, session_id, tree),
        SoxCmd::Unsubscribe => handle_unsubscribe(request, subscriptions, session_id),
        SoxCmd::Write => handle_write(request, tree),
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

/// subscribe ('s') — register for COV events.
///
/// SOX 1.1 batch format: u1 mask, u1 count, [u2 compId...]
/// If count=0: subscribe to all components (tree-wide).
/// Legacy format: u2 compId, u1 whatMask (detected by payload length).
fn handle_subscribe(
    req: &SoxRequest,
    subs: &mut SubscriptionManager,
    session_id: u16,
    tree: &ComponentTree,
) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);

    // SOX 1.1 batch format: first byte is mask, second is count
    let mask = reader.read_u8().unwrap_or(0xFF);
    let count = reader.read_u8().unwrap_or(0);

    if count == 0 && mask == 0xFF {
        // Subscribe to ALL components (subscribeToAllTreeEvents)
        for comp_id in tree.comp_ids() {
            subs.subscribe(session_id, comp_id);
        }
        tracing::info!(session = session_id, "SOX: subscribed to all components");
    } else {
        // Subscribe to specific components
        for _ in 0..count {
            if let Some(comp_id) = reader.read_u16() {
                subs.subscribe(session_id, comp_id);
            }
        }
        tracing::info!(session = session_id, mask, count, "SOX: batch subscribe");
    }

    SoxResponse::success(SoxCmd::Subscribe, req.req_id)
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

/// write ('w') -- write a slot value on a component.
///
/// Request payload: u2 compId, u1 slotId, u1 typeId, value.
fn handle_write(req: &SoxRequest, tree: &ComponentTree) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let comp_id = match reader.read_u16() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing compId"),
    };
    let slot_id = match reader.read_u8() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing slotId"),
    };
    let type_id = match reader.read_u8() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing typeId"),
    };
    let _value = match decode_slot_value(&mut reader, type_id) {
        Some(v) => v,
        None => return error_msg(req.cmd, req.req_id, "bad value"),
    };

    // Verify the component exists.
    let Some(comp) = tree.get(comp_id) else {
        return error_msg(req.cmd, req.req_id, "unknown comp");
    };

    // Verify the slot exists.
    if (slot_id as usize) >= comp.slots.len() {
        return error_msg(req.cmd, req.req_id, "bad slot");
    }

    // Build a success response. The actual engine write will be
    // handled by the caller using the parsed WriteRequest.
    let mut resp = SoxResponse::success(SoxCmd::Write, req.req_id);
    resp.write_u16(comp_id);
    resp.write_u8(slot_id);
    resp
}

/// Extract write request details from a SOX write command.
///
/// Returns a `WriteRequest` if the request is a valid write.
pub fn parse_write_request(req: &SoxRequest) -> Option<WriteRequest> {
    if req.cmd != SoxCmd::Write {
        return None;
    }
    let mut reader = SoxReader::new(&req.payload);
    let comp_id = reader.read_u16()?;
    let slot_id = reader.read_u8()?;
    let type_id = reader.read_u8()?;
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
    /// The "in" slot (index 1) is the writable input.
    pub fn to_channel_write(&self, tree: &ComponentTree) -> Option<(u32, f64)> {
        if self.comp_id < CHANNEL_COMP_BASE {
            return None;
        }
        // Slot 1 is "in" (the writable input).
        if self.slot_id != 1 {
            return None;
        }
        let comp = tree.get(self.comp_id)?;
        // Extract channel ID from the "channelId" slot (index 5).
        let channel_id = match &comp.slots.get(5)?.value {
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
        assert_eq!(ch.slots.len(), 7);
        assert_eq!(ch.slots[0].name, "out");
        assert_eq!(ch.slots[1].name, "in");
        assert_eq!(ch.slots[2].name, "status");
        assert_eq!(ch.slots[3].name, "enabled");
        assert_eq!(ch.slots[4].name, "label");
        assert_eq!(ch.slots[5].name, "channelId");
        assert_eq!(ch.slots[6].name, "raw");

        assert_eq!(ch.slots[0].value, SlotValue::Float(72.5));
        assert_eq!(ch.slots[1].value, SlotValue::Float(72.5));
        assert_eq!(ch.slots[2].value, SlotValue::Int(0)); // ok -> 0
        assert_eq!(ch.slots[3].value, SlotValue::Bool(true));
        assert_eq!(ch.slots[5].value, SlotValue::Int(1113));
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
        assert_eq!(resp.payload, vec![2, b'h', b'i']);
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
        assert_eq!(bytes[0], b'N');
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
        assert_eq!(resp.cmd, b'V');
        assert_eq!(resp.req_id, 5);
        assert_eq!(resp.payload[0], DEFAULT_KITS.len() as u8);
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
        assert_eq!(r.read_u8(), Some(0)); // kit_id
        assert_eq!(r.read_u8(), Some(0)); // type_id
        assert_eq!(r.read_str(), Some("app".into()));
        assert_eq!(r.read_u16(), Some(NO_PARENT));
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
        // Event payload starts with 'E' (response byte for Event), 0xFF
        assert_eq!(events[0].1[0], b'E');
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
        let tree = ComponentTree::from_channels(&sample_channels());
        let mut subs = SubscriptionManager::new();
        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(0xFF);
        let req = SoxRequest {
            cmd: SoxCmd::Subscribe,
            req_id: 20,
            payload,
        };
        let resp = handle_sox_request(&req, &tree, &mut subs, 1);
        assert_eq!(resp.cmd, b'S');
        assert!(subs.is_subscribed(1, 100));
    }

    #[test]
    fn handle_unsubscribe_via_request() {
        let tree = ComponentTree::from_channels(&sample_channels());
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
        let resp = handle_sox_request(&req, &tree, &mut subs, 1);
        assert_eq!(resp.cmd, b'U');
        assert!(!subs.is_subscribed(1, 100));
    }

    #[test]
    fn handle_write_valid_slot() {
        let tree = ComponentTree::from_channels(&sample_channels());
        let mut subs = SubscriptionManager::new();
        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(1); // slot_id (in)
        payload.push(SoxValueType::Float as u8);
        payload.extend_from_slice(&75.0f32.to_be_bytes());
        let req = SoxRequest {
            cmd: SoxCmd::Write,
            req_id: 30,
            payload,
        };
        let resp = handle_sox_request(&req, &tree, &mut subs, 1);
        assert_eq!(resp.cmd, b'W');
    }

    #[test]
    fn handle_write_bad_slot() {
        let tree = ComponentTree::from_channels(&sample_channels());
        let mut subs = SubscriptionManager::new();
        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(99); // invalid slot
        payload.push(SoxValueType::Float as u8);
        payload.extend_from_slice(&0.0f32.to_be_bytes());
        let req = SoxRequest {
            cmd: SoxCmd::Write,
            req_id: 31,
            payload,
        };
        let resp = handle_sox_request(&req, &tree, &mut subs, 1);
        assert_eq!(resp.cmd, b'!');
    }

    #[test]
    fn handle_write_unknown_comp() {
        let tree = ComponentTree::from_channels(&[]);
        let mut subs = SubscriptionManager::new();
        let mut payload = Vec::new();
        payload.extend_from_slice(&999u16.to_be_bytes());
        payload.push(0);
        payload.push(SoxValueType::Float as u8);
        payload.extend_from_slice(&0.0f32.to_be_bytes());
        let req = SoxRequest {
            cmd: SoxCmd::Write,
            req_id: 32,
            payload,
        };
        let resp = handle_sox_request(&req, &tree, &mut subs, 1);
        assert_eq!(resp.cmd, b'!');
    }

    #[test]
    fn handle_unsupported_command() {
        let tree = ComponentTree::from_channels(&[]);
        let mut subs = SubscriptionManager::new();
        let req = SoxRequest {
            cmd: SoxCmd::Add,
            req_id: 40,
            payload: Vec::new(),
        };
        let resp = handle_sox_request(&req, &tree, &mut subs, 1);
        assert_eq!(resp.cmd, b'!');
    }

    // ---- WriteRequest tests ----

    #[test]
    fn parse_write_request_valid() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(1); // slot_id
        payload.push(SoxValueType::Float as u8);
        payload.extend_from_slice(&72.5f32.to_be_bytes());
        let req = SoxRequest {
            cmd: SoxCmd::Write,
            req_id: 0,
            payload,
        };
        let wr = parse_write_request(&req).unwrap();
        assert_eq!(wr.comp_id, 100);
        assert_eq!(wr.slot_id, 1);
        assert_eq!(wr.value, SlotValue::Float(72.5));
    }

    #[test]
    fn parse_write_request_non_write_cmd() {
        let req = SoxRequest {
            cmd: SoxCmd::ReadComp,
            req_id: 0,
            payload: Vec::new(),
        };
        assert!(parse_write_request(&req).is_none());
    }

    #[test]
    fn write_request_to_channel_write() {
        let channels = sample_channels();
        let tree = ComponentTree::from_channels(&channels);
        let wr = WriteRequest {
            comp_id: 100,
            slot_id: 1,
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
            slot_id: 0, // "out" slot, not writable
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
        assert_eq!(comp.slots[5].value, SlotValue::Int(5555));
    }

    // ---- Error response tests ----

    #[test]
    fn error_msg_includes_text() {
        let resp = error_msg(SoxCmd::Write, 7, "bad slot");
        let bytes = resp.to_bytes();
        assert_eq!(bytes[0], b'!');
        assert_eq!(bytes[1], 7);
        assert_eq!(bytes[2], 8); // "bad slot" is 8 bytes
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
        // Parse past comp structure to verify slot count is present
        let mut r = SoxReader::new(&resp.payload);
        r.read_u16(); // comp_id
        r.read_u8(); // kit_id
        r.read_u8(); // type_id
        r.read_str(); // name
        r.read_u16(); // parent_id
        let child_count = r.read_u8().unwrap();
        for _ in 0..child_count {
            r.read_u16(); // child_id
        }
        let slot_count = r.read_u8().unwrap();
        assert_eq!(slot_count, 7); // 7 slots for a channel component
    }
}
