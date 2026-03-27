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
use std::path::Path;
use std::sync::Arc;

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

// ---- Links ----

/// A Sedona component link (wiring an output slot to an input slot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub from_comp: u16,
    pub from_slot: u8,
    pub to_comp: u16,
    pub to_slot: u8,
}

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
    pub links: Vec<Link>,
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
    /// Manifest-based slot schema database (shared reference).
    /// Loaded at startup from kit manifest XML files.
    pub manifest_db: Arc<ManifestDb>,
}

impl ComponentTree {
    /// Create an empty tree (with no manifest database).
    pub fn new() -> Self {
        Self {
            components: HashMap::new(),
            next_id: 0,
            channel_comp_end: CHANNEL_COMP_BASE,
            manifest_db: Arc::new(ManifestDb::new()),
        }
    }

    /// Create an empty tree with a pre-loaded manifest database.
    pub fn new_with_manifest(manifest_db: Arc<ManifestDb>) -> Self {
        Self {
            components: HashMap::new(),
            next_id: 0,
            channel_comp_end: CHANNEL_COMP_BASE,
            manifest_db,
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

    /// Add a link to the tree. Returns true if the link was added (not a duplicate).
    pub fn add_link(&mut self, from_comp: u16, from_slot: u8, to_comp: u16, to_slot: u8) -> bool {
        let link = Link { from_comp, from_slot, to_comp, to_slot };
        // Store the link on BOTH components so the editor can render wires
        // from both the source and target sides.
        if let Some(comp) = self.components.get(&to_comp) {
            if comp.links.contains(&link) {
                return false;
            }
        } else {
            return false;
        }
        if let Some(comp) = self.components.get_mut(&to_comp) {
            comp.links.push(link.clone());
        }
        if from_comp != to_comp {
            if let Some(comp) = self.components.get_mut(&from_comp) {
                if !comp.links.contains(&link) {
                    comp.links.push(link);
                }
            }
        }
        true
    }

    /// Remove a link from the tree. Returns true if the link was found and removed.
    pub fn remove_link(&mut self, from_comp: u16, from_slot: u8, to_comp: u16, to_slot: u8) -> bool {
        let link = Link { from_comp, from_slot, to_comp, to_slot };
        let mut removed = false;
        // Remove from both components
        if let Some(comp) = self.components.get_mut(&to_comp) {
            let before = comp.links.len();
            comp.links.retain(|l| l != &link);
            if comp.links.len() < before { removed = true; }
        }
        if from_comp != to_comp {
            if let Some(comp) = self.components.get_mut(&from_comp) {
                comp.links.retain(|l| l != &link);
            }
        }
        removed
    }

    /// Get all links where comp_id is involved (as source or destination).
    ///
    /// Since `add_link` stores each link on both the source and target components,
    /// the comp's own `.links` list already contains all relevant links.
    pub fn get_links(&self, comp_id: u16) -> Vec<&Link> {
        if let Some(comp) = self.components.get(&comp_id) {
            comp.links.iter().collect()
        } else {
            Vec::new()
        }
    }

    /// Reorder a parent's children to match the given order.
    /// Returns true if the parent exists and the reorder succeeded.
    /// All child IDs in the new order must be current children of the parent.
    pub fn reorder_children(&mut self, parent_id: u16, child_ids: &[u16]) -> bool {
        if let Some(parent) = self.components.get_mut(&parent_id) {
            // Validate: new order must contain exactly the same children
            let mut existing: Vec<u16> = parent.children.clone();
            let mut proposed: Vec<u16> = child_ids.to_vec();
            existing.sort();
            proposed.sort();
            if existing != proposed {
                return false;
            }
            parent.children = child_ids.to_vec();
            true
        } else {
            false
        }
    }

    /// Build a virtual component tree from engine channel data.
    ///
    /// Creates the standard Sedona tree structure with service nodes
    /// and maps each channel to a `control::NumericPoint` component.
    /// Optionally accepts a pre-loaded manifest database.
    pub fn from_channels(channels: &[ChannelInfo]) -> Self {
        Self::from_channels_with_manifest(channels, Arc::new(ManifestDb::new()))
    }

    /// Build a virtual component tree with a manifest database.
    pub fn from_channels_with_manifest(
        channels: &[ChannelInfo],
        manifest_db: Arc<ManifestDb>,
    ) -> Self {
        let mut tree = Self::new_with_manifest(manifest_db);

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
            links: Vec::new(),
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
            links: Vec::new(),
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
            links: Vec::new(),
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
            links: Vec::new(),
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
            links: Vec::new(),
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
            links: Vec::new(),
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
            links: Vec::new(),
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
                links: Vec::new(),
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

    /// Execute all links in the component tree: propagate values from source slots to target slots.
    ///
    /// Collects all link transfers first (to avoid borrow conflicts), then applies them.
    /// Returns the list of comp_ids whose slot values changed.
    pub fn execute_links(&mut self) -> Vec<u16> {
        // Phase 1: collect all transfers (source_value, target_comp, target_slot)
        let mut transfers: Vec<(SlotValue, u16, u8)> = Vec::new();
        for comp in self.components.values() {
            for link in &comp.links {
                // Only process each link once: use the copy stored on the target component
                if comp.comp_id != link.to_comp {
                    continue;
                }
                // Read source slot value
                if let Some(src_comp) = self.components.get(&link.from_comp) {
                    if let Some(slot) = src_comp.slots.get(link.from_slot as usize) {
                        transfers.push((slot.value.clone(), link.to_comp, link.to_slot));
                    }
                }
            }
        }

        // Phase 2: apply transfers, track which comp_ids changed
        let mut changed_set: HashSet<u16> = HashSet::new();
        for (value, to_comp, to_slot) in transfers {
            if let Some(comp) = self.components.get_mut(&to_comp) {
                if let Some(slot) = comp.slots.get_mut(to_slot as usize) {
                    if slot.value != value {
                        slot.value = value;
                        changed_set.insert(to_comp);
                    }
                }
            }
        }
        changed_set.into_iter().collect()
    }

    /// Execute component-specific logic (math operations) after link propagation.
    ///
    /// Evaluates arithmetic, boolean, comparator, conversion, and multiplexer components.
    /// Returns the list of comp_ids whose output slot values changed.
    pub fn execute_components(&mut self) -> Vec<u16> {
        // Phase 1: collect comp_ids that need evaluation and their current inputs
        let mut evaluations: Vec<(u16, u8, u8)> = Vec::new(); // (comp_id, kit_id, type_id)
        for comp in self.components.values() {
            match (comp.kit_id, comp.type_id) {
                // Arithmetic
                (2, 3) | (2, 49) | (2, 37) | (2, 18) |
                (2, 4) | (2, 50) | (2, 38) |
                // Unary Math
                (2, 39) | (2, 23) | (2, 34) | (2, 35) | (2, 32) | (2, 47) |
                // Boolean Logic
                (2, 5) | (2, 6) | (2, 42) | (2, 43) | (2, 40) | (2, 59) |
                // Comparator
                (2, 12) |
                // Type Conversion
                (2, 10) | (2, 22) | (2, 26) |
                // Multiplexers
                (2, 1) | (2, 11) | (2, 28) |
                // Hysteresis, SRLatch, Reset
                (2, 25) | (2, 48) | (2, 46) |
                // WriteFloat/WriteBool/WriteInt passthrough
                (2, 57) | (2, 56) | (2, 58) |
                // LSeq (linear sequencer)
                (2, 31) => {
                    evaluations.push((comp.comp_id, comp.kit_id, comp.type_id));
                }
                _ => {}
            }
        }

        // Phase 2: compute outputs
        let mut updates: Vec<(u16, Vec<(usize, SlotValue)>)> = Vec::new();
        for (comp_id, _kit_id, type_id) in &evaluations {
            if let Some(comp) = self.components.get(comp_id) {
                let mut slot_updates: Vec<(usize, SlotValue)> = Vec::new();

                match type_id {
                    // --- Original Arithmetic (2-input) ---
                    3 => {
                        // Add2: out = in1 + in2
                        let in1 = get_float(&comp.slots, 2);
                        let in2 = get_float(&comp.slots, 3);
                        slot_updates.push((1, SlotValue::Float(in1 + in2)));
                    }
                    49 => {
                        // Sub2: out = in1 - in2
                        let in1 = get_float(&comp.slots, 2);
                        let in2 = get_float(&comp.slots, 3);
                        slot_updates.push((1, SlotValue::Float(in1 - in2)));
                    }
                    37 => {
                        // Mul2: out = in1 * in2
                        let in1 = get_float(&comp.slots, 2);
                        let in2 = get_float(&comp.slots, 3);
                        slot_updates.push((1, SlotValue::Float(in1 * in2)));
                    }
                    18 => {
                        // Div2: out = in1 / in2, div0 flag
                        let in1 = get_float(&comp.slots, 2);
                        let in2 = get_float(&comp.slots, 3);
                        let div0 = in2 == 0.0;
                        let out = if div0 { 0.0 } else { in1 / in2 };
                        slot_updates.push((1, SlotValue::Float(out)));
                        slot_updates.push((4, SlotValue::Bool(div0)));
                    }

                    // --- New Arithmetic (4-input) ---
                    4 => {
                        // Add4: out = in1 + in2 + in3 + in4
                        let in1 = get_float(&comp.slots, 2);
                        let in2 = get_float(&comp.slots, 3);
                        let in3 = get_float(&comp.slots, 4);
                        let in4 = get_float(&comp.slots, 5);
                        slot_updates.push((1, SlotValue::Float(in1 + in2 + in3 + in4)));
                    }
                    50 => {
                        // Sub4: out = in1 - in2 - in3 - in4
                        let in1 = get_float(&comp.slots, 2);
                        let in2 = get_float(&comp.slots, 3);
                        let in3 = get_float(&comp.slots, 4);
                        let in4 = get_float(&comp.slots, 5);
                        slot_updates.push((1, SlotValue::Float(in1 - in2 - in3 - in4)));
                    }
                    38 => {
                        // Mul4: out = in1 * in2 * in3 * in4
                        let in1 = get_float(&comp.slots, 2);
                        let in2 = get_float(&comp.slots, 3);
                        let in3 = get_float(&comp.slots, 4);
                        let in4 = get_float(&comp.slots, 5);
                        slot_updates.push((1, SlotValue::Float(in1 * in2 * in3 * in4)));
                    }

                    // --- Unary Math ---
                    39 => {
                        // Neg: out = -in
                        let inv = get_float(&comp.slots, 2);
                        slot_updates.push((1, SlotValue::Float(-inv)));
                    }
                    23 => {
                        // FloatOffset: out = in + offset
                        let inv = get_float(&comp.slots, 2);
                        let offset = get_float(&comp.slots, 3);
                        slot_updates.push((1, SlotValue::Float(inv + offset)));
                    }
                    34 => {
                        // Max: out = max(in1, in2)
                        let in1 = get_float(&comp.slots, 2);
                        let in2 = get_float(&comp.slots, 3);
                        slot_updates.push((1, SlotValue::Float(if in1 > in2 { in1 } else { in2 })));
                    }
                    35 => {
                        // Min: out = min(in1, in2)
                        let in1 = get_float(&comp.slots, 2);
                        let in2 = get_float(&comp.slots, 3);
                        slot_updates.push((1, SlotValue::Float(if in1 < in2 { in1 } else { in2 })));
                    }
                    32 => {
                        // Limiter: out = clamp(in, lowLmt, highLmt)
                        let inv = get_float(&comp.slots, 2);
                        let low = get_float(&comp.slots, 3);
                        let high = get_float(&comp.slots, 4);
                        let clamped = if inv < low { low } else if inv > high { high } else { inv };
                        slot_updates.push((1, SlotValue::Float(clamped)));
                    }
                    47 => {
                        // Round: out = round(in, decimalPlaces)
                        let inv = get_float(&comp.slots, 2);
                        let dp = get_int(&comp.slots, 3);
                        let factor = 10.0_f32.powi(dp);
                        let rounded = (inv * factor).round() / factor;
                        slot_updates.push((1, SlotValue::Float(rounded)));
                    }

                    // --- Boolean Logic ---
                    5 => {
                        // And2: out = in1 && in2
                        let in1 = get_bool(&comp.slots, 2);
                        let in2 = get_bool(&comp.slots, 3);
                        slot_updates.push((1, SlotValue::Bool(in1 && in2)));
                    }
                    6 => {
                        // And4: out = in1 && in2 && in3 && in4
                        let in1 = get_bool(&comp.slots, 2);
                        let in2 = get_bool(&comp.slots, 3);
                        let in3 = get_bool(&comp.slots, 4);
                        let in4 = get_bool(&comp.slots, 5);
                        slot_updates.push((1, SlotValue::Bool(in1 && in2 && in3 && in4)));
                    }
                    42 => {
                        // Or2: out = in1 || in2
                        let in1 = get_bool(&comp.slots, 2);
                        let in2 = get_bool(&comp.slots, 3);
                        slot_updates.push((1, SlotValue::Bool(in1 || in2)));
                    }
                    43 => {
                        // Or4: out = in1 || in2 || in3 || in4
                        let in1 = get_bool(&comp.slots, 2);
                        let in2 = get_bool(&comp.slots, 3);
                        let in3 = get_bool(&comp.slots, 4);
                        let in4 = get_bool(&comp.slots, 5);
                        slot_updates.push((1, SlotValue::Bool(in1 || in2 || in3 || in4)));
                    }
                    40 => {
                        // Not: out = !in
                        let inv = get_bool(&comp.slots, 2);
                        slot_updates.push((1, SlotValue::Bool(!inv)));
                    }
                    59 => {
                        // Xor: out = in1 ^ in2
                        let in1 = get_bool(&comp.slots, 2);
                        let in2 = get_bool(&comp.slots, 3);
                        slot_updates.push((1, SlotValue::Bool(in1 ^ in2)));
                    }

                    // --- Comparator ---
                    12 => {
                        // Cmpr: xgy = (x > y), xey = (x == y), xly = (x < y)
                        // slots: meta=0, xgy=1, xey=2, xly=3, x=4, y=5
                        let x = get_float(&comp.slots, 4);
                        let y = get_float(&comp.slots, 5);
                        slot_updates.push((1, SlotValue::Bool(x > y)));
                        slot_updates.push((2, SlotValue::Bool(x == y)));
                        slot_updates.push((3, SlotValue::Bool(x < y)));
                    }

                    // --- Type Conversion ---
                    10 => {
                        // B2P: out(bool) = in(bool) — pass-through
                        let inv = get_bool(&comp.slots, 2);
                        slot_updates.push((1, SlotValue::Bool(inv)));
                    }
                    22 => {
                        // F2I: out(int) = in(float) as i32
                        let inv = get_float(&comp.slots, 2);
                        slot_updates.push((1, SlotValue::Int(inv as i32)));
                    }
                    26 => {
                        // I2F: out(float) = in(int) as f64
                        let inv = get_int(&comp.slots, 2);
                        slot_updates.push((1, SlotValue::Float(inv as f32)));
                    }

                    // --- Multiplexers ---
                    1 => {
                        // ASW: out = if sel then in2 else in1
                        // slots: meta=0, out=1, sel=2(bool), in1=3, in2=4
                        let sel = get_bool(&comp.slots, 2);
                        let in1 = get_float(&comp.slots, 3);
                        let in2 = get_float(&comp.slots, 4);
                        slot_updates.push((1, SlotValue::Float(if sel { in2 } else { in1 })));
                    }
                    11 => {
                        // BSW: out(bool) = if sel then in2 else in1
                        let sel = get_bool(&comp.slots, 2);
                        let in1 = get_bool(&comp.slots, 3);
                        let in2 = get_bool(&comp.slots, 4);
                        slot_updates.push((1, SlotValue::Bool(if sel { in2 } else { in1 })));
                    }
                    28 => {
                        // ISW: out(int) = if sel then in2 else in1
                        let sel = get_bool(&comp.slots, 2);
                        let in1 = get_int(&comp.slots, 3);
                        let in2 = get_int(&comp.slots, 4);
                        slot_updates.push((1, SlotValue::Int(if sel { in2 } else { in1 })));
                    }

                    // --- Hysteresis ---
                    25 => {
                        // Hysteresis: out(bool) based on in vs rising/falling thresholds
                        // slots: meta=0, out=1, in=2, rising=3(config), falling=4(config)
                        let inv = get_float(&comp.slots, 2);
                        let rising = get_float(&comp.slots, 3);
                        let falling = get_float(&comp.slots, 4);
                        let current_out = get_bool(&comp.slots, 1);
                        let new_out = if current_out {
                            inv >= falling // stay true until below falling
                        } else {
                            inv >= rising // switch to true when above rising
                        };
                        slot_updates.push((1, SlotValue::Bool(new_out)));
                    }

                    // --- SRLatch ---
                    48 => {
                        // SRLatch: set/reset latch
                        // slots: meta=0, out=1, set=2(bool), reset=3(bool)
                        let set = get_bool(&comp.slots, 2);
                        let reset = get_bool(&comp.slots, 3);
                        let current = get_bool(&comp.slots, 1);
                        let new_out = if reset { false } else if set { true } else { current };
                        slot_updates.push((1, SlotValue::Bool(new_out)));
                    }

                    // --- Reset (range remapping) ---
                    46 => {
                        // Reset: remap input from one range to another
                        // slots: meta=0, out=1, in=2, inLow=3(config), inHigh=4(config),
                        //        outLow=5(config), outHigh=6(config)
                        let inv = get_float(&comp.slots, 2);
                        let in_low = get_float(&comp.slots, 3);
                        let in_high = get_float(&comp.slots, 4);
                        let out_low = get_float(&comp.slots, 5);
                        let out_high = get_float(&comp.slots, 6);
                        let range = in_high - in_low;
                        let out = if range == 0.0 {
                            out_low
                        } else {
                            let pct = (inv - in_low) / range;
                            out_low + pct * (out_high - out_low)
                        };
                        slot_updates.push((1, SlotValue::Float(out)));
                    }

                    // --- WriteFloat passthrough ---
                    57 => {
                        // WriteFloat: out = in (runtime passthrough)
                        // slots: meta=0, out=1, in=2 (+ override slots)
                        let inv = get_float(&comp.slots, 2);
                        slot_updates.push((1, SlotValue::Float(inv)));
                    }

                    // --- WriteBool passthrough ---
                    56 => {
                        // WriteBool: out = in
                        let inv = get_bool(&comp.slots, 2);
                        slot_updates.push((1, SlotValue::Bool(inv)));
                    }

                    // --- WriteInt passthrough ---
                    58 => {
                        // WriteInt: out = in
                        let inv = get_int(&comp.slots, 2);
                        slot_updates.push((1, SlotValue::Int(inv)));
                    }

                    // --- LSeq (Linear Sequencer) ---
                    31 => {
                        // LSeq: divides input range into N stages
                        // slots: meta=0, out=1, in=2, numStages=3(config), rampTime=4(config)
                        let inv = get_float(&comp.slots, 2);
                        let num_stages = get_int(&comp.slots, 3);
                        if num_stages > 0 {
                            let stage = (inv * num_stages as f32).floor() as i32;
                            let clamped = stage.max(0).min(num_stages);
                            slot_updates.push((1, SlotValue::Int(clamped)));
                        }
                    }

                    _ => {}
                }
                if !slot_updates.is_empty() {
                    updates.push((*comp_id, slot_updates));
                }
            }
        }

        // Phase 3: apply updates, track changes
        let mut changed: Vec<u16> = Vec::new();
        for (comp_id, slot_updates) in updates {
            if let Some(comp) = self.components.get_mut(&comp_id) {
                let mut comp_changed = false;
                for (slot_idx, new_value) in slot_updates {
                    if let Some(slot) = comp.slots.get_mut(slot_idx) {
                        if slot.value != new_value {
                            slot.value = new_value;
                            comp_changed = true;
                        }
                    }
                }
                if comp_changed {
                    changed.push(comp_id);
                }
            }
        }
        changed
    }
}

/// Extract a float value from a slot, coercing Int to f32 if needed.
fn get_float(slots: &[VirtualSlot], idx: usize) -> f32 {
    slots.get(idx).and_then(|s| match &s.value {
        SlotValue::Float(v) => Some(*v),
        SlotValue::Int(v) => Some(*v as f32),
        _ => None,
    }).unwrap_or(0.0)
}

/// Extract a bool value from a slot.
fn get_bool(slots: &[VirtualSlot], idx: usize) -> bool {
    slots.get(idx).and_then(|s| match &s.value {
        SlotValue::Bool(v) => Some(*v),
        _ => None,
    }).unwrap_or(false)
}

/// Extract an int value from a slot, coercing Float to i32 if needed.
fn get_int(slots: &[VirtualSlot], idx: usize) -> i32 {
    slots.get(idx).and_then(|s| match &s.value {
        SlotValue::Int(v) => Some(*v),
        SlotValue::Float(v) => Some(*v as i32),
        _ => None,
    }).unwrap_or(0)
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

// ---- Manifest database ----

/// A slot definition parsed from a kit manifest XML.
#[derive(Debug, Clone)]
pub struct ManifestSlot {
    pub name: String,
    pub type_id: u8,
    pub flags: u8,
    pub default_value: SlotValue,
}

/// Database of all component types and their slots, parsed from kit manifest XML files.
///
/// Maps `(kit_index, type_id)` to the full slot list (including inherited `meta` slot).
/// This replaces the hardcoded `default_slots_for_type` function.
#[derive(Debug, Clone, Default)]
pub struct ManifestDb {
    /// (kit_index, type_id) -> ordered list of slots for that component type.
    types: HashMap<(u8, u8), Vec<ManifestSlot>>,
}

impl ManifestDb {
    /// Create an empty manifest database.
    pub fn new() -> Self {
        Self {
            types: HashMap::new(),
        }
    }

    /// Load manifest XML files from a directory.
    ///
    /// For each kit in `DEFAULT_KITS`, looks for `{manifests_dir}/{kit_name}/{kit_name}-{checksum}.xml`
    /// or `{manifests_dir}/{kit_name}.xml` (flat layout). Missing files are silently skipped
    /// (the hardcoded fallback in `default_slots_for_type` will be used instead).
    ///
    /// On the BeagleBone: `/home/eacio/sandstar/etc/manifests/`
    /// In the repo:       `SedonaRepo/.../manifests/`
    pub fn load(manifests_dir: &str) -> Self {
        let mut db = Self::new();
        let dir = Path::new(manifests_dir);

        for (kit_index, kit) in DEFAULT_KITS.iter().enumerate() {
            let kit_index = kit_index as u8;
            // Try subdirectory layout first: {dir}/{kitName}/{kitName}-{checksum}.xml
            let subdir_path = dir
                .join(kit.name)
                .join(format!("{}-{:08x}.xml", kit.name, kit.checksum));
            // Then flat layout: {dir}/{kitName}.xml
            let flat_path = dir.join(format!("{}.xml", kit.name));
            // Also try flat with checksum: {dir}/{kitName}-{checksum}.xml
            let flat_checksum_path = dir.join(format!("{}-{:08x}.xml", kit.name, kit.checksum));

            let xml_path = if subdir_path.exists() {
                subdir_path
            } else if flat_path.exists() {
                flat_path
            } else if flat_checksum_path.exists() {
                flat_checksum_path
            } else {
                tracing::debug!(kit = kit.name, "manifest XML not found, using hardcoded fallback");
                continue;
            };

            match std::fs::read_to_string(&xml_path) {
                Ok(xml) => {
                    let count = db.parse_kit_manifest(&xml, kit_index);
                    tracing::info!(
                        kit = kit.name,
                        kit_index,
                        types = count,
                        path = %xml_path.display(),
                        "loaded manifest"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        kit = kit.name,
                        path = %xml_path.display(),
                        err = %e,
                        "failed to read manifest XML"
                    );
                }
            }
        }

        tracing::info!(
            total_types = db.types.len(),
            "ManifestDb loaded"
        );
        db
    }

    /// Parse a single kit manifest XML and insert types into the database.
    /// Returns the number of types parsed.
    fn parse_kit_manifest(&mut self, xml: &str, kit_index: u8) -> usize {
        use quick_xml::events::Event;
        use quick_xml::Reader;

        let mut reader = Reader::from_str(xml);
        let mut count = 0;

        // Track current type being parsed
        let mut current_type_id: Option<u8> = None;
        let mut current_base: Option<String> = None;
        let mut current_slots: Vec<ManifestSlot> = Vec::new();

        loop {
            let event = reader.read_event();
            // Determine if this is a self-closing element (no End event will follow)
            let is_empty = matches!(&event, Ok(Event::Empty(_)));

            match event {
                Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                    let local_name = e.local_name();
                    let tag = std::str::from_utf8(local_name.as_ref()).unwrap_or("");

                    if tag == "type" {
                        // Starting a new type definition
                        let mut id: Option<u8> = None;
                        let mut base: Option<String> = None;

                        for attr in e.attributes().flatten() {
                            let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                            let val = std::str::from_utf8(&attr.value).unwrap_or("");
                            match key {
                                "id" => id = val.parse().ok(),
                                "base" => base = Some(val.to_string()),
                                _ => {}
                            }
                        }

                        if let Some(type_id) = id {
                            // Flush previous type if any
                            if let Some(prev_type_id) = current_type_id.take() {
                                self.insert_type(kit_index, prev_type_id, &current_base, &current_slots);
                                count += 1;
                            }
                            current_type_id = Some(type_id);
                            current_base = base;
                            current_slots.clear();

                            // Self-closing <type .../> has no slots, flush immediately
                            if is_empty {
                                self.insert_type(kit_index, type_id, &current_base, &current_slots);
                                count += 1;
                                current_type_id = None;
                                current_base = None;
                            }
                        }
                    }

                    if tag == "slot" && current_type_id.is_some() {
                        // Parse slot attributes
                        let mut name = String::new();
                        let mut slot_type = String::new();
                        let mut flags_str = String::new();
                        let mut default_str: Option<String> = None;

                        for attr in e.attributes().flatten() {
                            let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                            let val = std::str::from_utf8(&attr.value).unwrap_or("");
                            match key {
                                "name" => name = val.to_string(),
                                "type" => slot_type = val.to_string(),
                                "flags" => flags_str = val.to_string(),
                                "default" => default_str = Some(val.to_string()),
                                _ => {}
                            }
                        }

                        let type_id = sedona_type_to_sox(&slot_type);
                        let flags = sedona_flags_to_slot_flags(&flags_str);
                        let default_value = parse_default_value(type_id, default_str.as_deref());

                        current_slots.push(ManifestSlot {
                            name,
                            type_id,
                            flags,
                            default_value,
                        });
                    }
                }
                Ok(Event::End(ref e)) => {
                    let local_name = e.local_name();
                    let tag = std::str::from_utf8(local_name.as_ref()).unwrap_or("");
                    if tag == "type" {
                        // Flush current type
                        if let Some(type_id) = current_type_id.take() {
                            self.insert_type(kit_index, type_id, &current_base, &current_slots);
                            count += 1;
                        }
                        current_base = None;
                        current_slots.clear();
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    tracing::warn!(kit_index, err = %e, "XML parse error in manifest");
                    break;
                }
                _ => {}
            }
        }

        // Flush any remaining type
        if let Some(type_id) = current_type_id {
            self.insert_type(kit_index, type_id, &current_base, &current_slots);
            count += 1;
        }

        count
    }

    /// Insert a parsed type into the database, prepending inherited `meta` slot
    /// for types that extend `sys::Component`.
    fn insert_type(
        &mut self,
        kit_index: u8,
        type_id: u8,
        base: &Option<String>,
        own_slots: &[ManifestSlot],
    ) {
        let mut slots = Vec::new();

        // If this type extends sys::Component (directly or transitively),
        // prepend the inherited `meta` slot.
        let has_base = base.as_ref().is_some_and(|b| !b.is_empty());
        if has_base {
            // Check if own_slots already includes a `meta` slot (some manifests redefine it)
            let has_meta = own_slots.iter().any(|s| s.name == "meta");
            if !has_meta {
                slots.push(ManifestSlot {
                    name: "meta".into(),
                    type_id: SoxValueType::Int as u8,
                    flags: SLOT_FLAG_CONFIG,
                    default_value: SlotValue::Int(1),
                });
            }
        }

        slots.extend_from_slice(own_slots);
        self.types.insert((kit_index, type_id), slots);
    }

    /// Look up the slot schema for a component type.
    ///
    /// Returns `None` if no manifest was loaded for this (kit_id, type_id).
    pub fn get_slots(&self, kit_id: u8, type_id: u8) -> Option<&Vec<ManifestSlot>> {
        self.types.get(&(kit_id, type_id))
    }

    /// Get the total number of types in the database.
    pub fn type_count(&self) -> usize {
        self.types.len()
    }

    /// Convert manifest slots to virtual slots (with default values).
    pub fn slots_to_virtual(slots: &[ManifestSlot]) -> Vec<VirtualSlot> {
        slots
            .iter()
            .map(|ms| VirtualSlot {
                name: ms.name.clone(),
                type_id: ms.type_id,
                flags: ms.flags,
                value: ms.default_value.clone(),
            })
            .collect()
    }
}

/// Map a Sedona type string from manifest XML to a SOX value type ID.
fn sedona_type_to_sox(sedona_type: &str) -> u8 {
    match sedona_type {
        "int" => SoxValueType::Int as u8,
        "float" => SoxValueType::Float as u8,
        "bool" => SoxValueType::Bool as u8,
        "void" => SoxValueType::Void as u8,
        "sys::Buf" => SoxValueType::Buf as u8,
        "byte" => SoxValueType::Byte as u8,
        "short" => SoxValueType::Short as u8,
        "long" => SoxValueType::Long as u8,
        "double" => SoxValueType::Double as u8,
        // Unknown types default to Int (safest for unknown wire format)
        _ => SoxValueType::Int as u8,
    }
}

/// Map Sedona flag characters from manifest XML to slot flag bitmask.
///
/// - 'c' = config (0x01)
/// - 'a' = action (0x02)
/// - 's' = string display hint (affects Buf rendering, not flag bits)
/// - 'o' = operator
/// - no 'c' or 'a' = runtime (0x04)
fn sedona_flags_to_slot_flags(flags_str: &str) -> u8 {
    let mut flags: u8 = 0;
    if flags_str.contains('c') {
        flags |= SLOT_FLAG_CONFIG;
    }
    if flags_str.contains('a') {
        flags |= SLOT_FLAG_ACTION;
    }
    if flags_str.contains('o') {
        flags |= SLOT_FLAG_OPERATOR;
    }
    // If neither config nor action, it's a runtime slot
    if flags & (SLOT_FLAG_CONFIG | SLOT_FLAG_ACTION) == 0 {
        flags |= SLOT_FLAG_RUNTIME;
    }
    flags
}

/// Parse a default value string from manifest XML into a `SlotValue`.
fn parse_default_value(type_id: u8, default_str: Option<&str>) -> SlotValue {
    match default_str {
        None => default_for_type(type_id),
        Some(s) if s.is_empty() => {
            // Empty default: for Buf/Str types means empty string, otherwise zero
            if type_id == SoxValueType::Buf as u8 {
                SlotValue::Str(String::new())
            } else {
                default_for_type(type_id)
            }
        }
        Some(s) => {
            match type_id {
                t if t == SoxValueType::Bool as u8 => {
                    SlotValue::Bool(s == "true" || s == "1")
                }
                t if t == SoxValueType::Int as u8 => {
                    SlotValue::Int(s.parse().unwrap_or(0))
                }
                t if t == SoxValueType::Float as u8 => {
                    SlotValue::Float(s.parse().unwrap_or(0.0))
                }
                t if t == SoxValueType::Double as u8 => {
                    SlotValue::Double(s.parse().unwrap_or(0.0))
                }
                t if t == SoxValueType::Long as u8 => {
                    SlotValue::Long(s.parse().unwrap_or(0))
                }
                t if t == SoxValueType::Byte as u8 || t == SoxValueType::Short as u8 => {
                    SlotValue::Int(s.parse().unwrap_or(0))
                }
                t if t == SoxValueType::Buf as u8 => {
                    SlotValue::Str(s.to_string())
                }
                t if t == SoxValueType::Void as u8 => SlotValue::Null,
                _ => default_for_type(type_id),
            }
        }
    }
}

/// Return the zero/default value for a given SOX type.
fn default_for_type(type_id: u8) -> SlotValue {
    match type_id {
        t if t == SoxValueType::Bool as u8 => SlotValue::Bool(false),
        t if t == SoxValueType::Int as u8 => SlotValue::Int(0),
        t if t == SoxValueType::Float as u8 => SlotValue::Float(0.0),
        t if t == SoxValueType::Double as u8 => SlotValue::Double(0.0),
        t if t == SoxValueType::Long as u8 => SlotValue::Long(0),
        t if t == SoxValueType::Byte as u8 => SlotValue::Int(0),
        t if t == SoxValueType::Short as u8 => SlotValue::Int(0),
        t if t == SoxValueType::Buf as u8 => SlotValue::Str(String::new()),
        _ => SlotValue::Null,
    }
}

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

    /// Build a CONFIG COV event for a single component.
    ///
    /// Returns `(session_id, event_bytes)` pairs for each subscriber.
    /// The event contains only config-flagged slot values (what='c').
    /// Action slots are never serialized.
    pub fn build_config_events(
        &self,
        comp_id: u16,
        tree: &ComponentTree,
    ) -> Vec<(u16, Vec<u8>)> {
        let mut events = Vec::new();
        let Some(comp) = tree.get(comp_id) else {
            return events;
        };
        let Some(watchers) = self.subscriptions.get(&comp_id) else {
            return events;
        };

        // Build raw event bytes: ['e', 0xFF, comp_id, 'c', config_slot_values...]
        let mut payload = Vec::with_capacity(64);
        payload.push(b'e');
        payload.push(0xFF);
        payload.extend_from_slice(&comp_id.to_be_bytes());
        payload.push(b'c'); // what = config

        // Write only config-flagged slot values in schema order
        for slot in &comp.slots {
            if slot.flags & SLOT_FLAG_ACTION != 0 { continue; }
            if slot.flags & SLOT_FLAG_CONFIG == 0 { continue; }
            encode_slot_value_raw(&mut payload, &slot.value);
        }

        for &session_id in watchers {
            events.push((session_id, payload.clone()));
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
        SoxCmd::Link => handle_link(request, tree),
        SoxCmd::Reorder => handle_reorder(request, tree),
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
            // Links: repeating [u2 fromComp, u1 fromSlot, u2 toComp, u1 toSlot] + u2 0xFFFF terminator
            for link in &comp.links {
                resp.write_u16(link.from_comp);
                resp.write_u8(link.from_slot);
                resp.write_u16(link.to_comp);
                resp.write_u8(link.to_slot);
            }
            resp.write_u16(0xFFFF); // terminator
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
                    for link in &comp.links {
                        resp.write_u16(link.from_comp);
                        resp.write_u8(link.from_slot);
                        resp.write_u16(link.to_comp);
                        resp.write_u8(link.to_slot);
                    }
                    resp.write_u16(0xFFFF);
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

/// Build default slots for a component based on its kit_id and type_id.
///
/// Known types get their full slot schema from the kit manifest.
/// Unknown types get a minimal [meta] slot and can be auto-extended on write.
fn default_slots_for_type(kit_id: u8, type_id: u8) -> Vec<VirtualSlot> {
    match (kit_id, type_id) {
        // control::ConstFloat (kit 2, type 14)
        // Manifest slots: meta(Int,config), out(Float,config), set(Float,action), setNull(Void,action)
        (2, 14) => vec![
            VirtualSlot {
                name: "meta".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(1),
            },
            VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Float as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Float(0.0),
            },
            VirtualSlot {
                name: "set".into(),
                type_id: SoxValueType::Float as u8,
                flags: SLOT_FLAG_ACTION,
                value: SlotValue::Float(0.0),
            },
            VirtualSlot {
                name: "setNull".into(),
                type_id: SoxValueType::Void as u8,
                flags: SLOT_FLAG_ACTION,
                value: SlotValue::Null,
            },
        ],
        // control::ConstInt (kit 2, type 15)
        // Manifest: meta(Int,config), out(Int,config), set(Int,action)
        (2, 15) => vec![
            VirtualSlot { name: "meta".into(), type_id: SoxValueType::Int as u8, flags: SLOT_FLAG_CONFIG, value: SlotValue::Int(1) },
            VirtualSlot { name: "out".into(), type_id: SoxValueType::Int as u8, flags: SLOT_FLAG_CONFIG, value: SlotValue::Int(0) },
            VirtualSlot { name: "set".into(), type_id: SoxValueType::Int as u8, flags: SLOT_FLAG_ACTION, value: SlotValue::Int(0) },
        ],
        // control::Add2 (kit 2, type 3), Sub2 (49), Mul2 (37)
        // Manifest: meta(Int,config), out(Float,runtime), in1(Float,runtime), in2(Float,runtime)
        (2, 3) | (2, 49) | (2, 37) => vec![
            VirtualSlot { name: "meta".into(), type_id: SoxValueType::Int as u8, flags: SLOT_FLAG_CONFIG, value: SlotValue::Int(1) },
            VirtualSlot { name: "out".into(), type_id: SoxValueType::Float as u8, flags: SLOT_FLAG_RUNTIME, value: SlotValue::Float(0.0) },
            VirtualSlot { name: "in1".into(), type_id: SoxValueType::Float as u8, flags: SLOT_FLAG_RUNTIME, value: SlotValue::Float(0.0) },
            VirtualSlot { name: "in2".into(), type_id: SoxValueType::Float as u8, flags: SLOT_FLAG_RUNTIME, value: SlotValue::Float(0.0) },
        ],
        // control::Div2 (kit 2, type 18)
        // Manifest: meta(Int,config), out(Float,runtime), in1(Float,runtime), in2(Float,runtime), div0(Bool,runtime)
        (2, 18) => vec![
            VirtualSlot { name: "meta".into(), type_id: SoxValueType::Int as u8, flags: SLOT_FLAG_CONFIG, value: SlotValue::Int(1) },
            VirtualSlot { name: "out".into(), type_id: SoxValueType::Float as u8, flags: SLOT_FLAG_RUNTIME, value: SlotValue::Float(0.0) },
            VirtualSlot { name: "in1".into(), type_id: SoxValueType::Float as u8, flags: SLOT_FLAG_RUNTIME, value: SlotValue::Float(0.0) },
            VirtualSlot { name: "in2".into(), type_id: SoxValueType::Float as u8, flags: SLOT_FLAG_RUNTIME, value: SlotValue::Float(0.0) },
            VirtualSlot { name: "div0".into(), type_id: SoxValueType::Bool as u8, flags: SLOT_FLAG_RUNTIME, value: SlotValue::Bool(false) },
        ],
        // control::ConstBool (kit 2, type 13)
        // Manifest: meta, out(Bool,config), setTrue(Void,action), setFalse(Void,action), setNull(Void,action)
        (2, 13) => vec![
            VirtualSlot { name: "meta".into(), type_id: SoxValueType::Int as u8, flags: SLOT_FLAG_CONFIG, value: SlotValue::Int(1) },
            VirtualSlot { name: "out".into(), type_id: SoxValueType::Bool as u8, flags: SLOT_FLAG_CONFIG, value: SlotValue::Bool(false) },
            VirtualSlot { name: "setTrue".into(), type_id: SoxValueType::Void as u8, flags: SLOT_FLAG_ACTION, value: SlotValue::Null },
            VirtualSlot { name: "setFalse".into(), type_id: SoxValueType::Void as u8, flags: SLOT_FLAG_ACTION, value: SlotValue::Null },
            VirtualSlot { name: "setNull".into(), type_id: SoxValueType::Void as u8, flags: SLOT_FLAG_ACTION, value: SlotValue::Null },
        ],
        // control::WriteBool (kit 2, type 56)
        // Manifest: meta, in(Bool,config), out(Bool,runtime), setTrue/setFalse/setNull(actions)
        (2, 56) => vec![
            VirtualSlot { name: "meta".into(), type_id: SoxValueType::Int as u8, flags: SLOT_FLAG_CONFIG, value: SlotValue::Int(1) },
            VirtualSlot { name: "in".into(), type_id: SoxValueType::Bool as u8, flags: SLOT_FLAG_CONFIG, value: SlotValue::Bool(false) },
            VirtualSlot { name: "out".into(), type_id: SoxValueType::Bool as u8, flags: SLOT_FLAG_RUNTIME, value: SlotValue::Bool(false) },
            VirtualSlot { name: "setTrue".into(), type_id: SoxValueType::Void as u8, flags: SLOT_FLAG_ACTION, value: SlotValue::Null },
            VirtualSlot { name: "setFalse".into(), type_id: SoxValueType::Void as u8, flags: SLOT_FLAG_ACTION, value: SlotValue::Null },
            VirtualSlot { name: "setNull".into(), type_id: SoxValueType::Void as u8, flags: SLOT_FLAG_ACTION, value: SlotValue::Null },
        ],
        // control::WriteFloat (kit 2, type 57)
        // Manifest: meta, in(Float,config), out(Float,runtime), set(Float,action), setNull(Void,action)
        (2, 57) => vec![
            VirtualSlot { name: "meta".into(), type_id: SoxValueType::Int as u8, flags: SLOT_FLAG_CONFIG, value: SlotValue::Int(1) },
            VirtualSlot { name: "in".into(), type_id: SoxValueType::Float as u8, flags: SLOT_FLAG_CONFIG, value: SlotValue::Float(0.0) },
            VirtualSlot { name: "out".into(), type_id: SoxValueType::Float as u8, flags: SLOT_FLAG_RUNTIME, value: SlotValue::Float(0.0) },
            VirtualSlot { name: "set".into(), type_id: SoxValueType::Float as u8, flags: SLOT_FLAG_ACTION, value: SlotValue::Float(0.0) },
            VirtualSlot { name: "setNull".into(), type_id: SoxValueType::Void as u8, flags: SLOT_FLAG_ACTION, value: SlotValue::Null },
        ],
        // control::WriteInt (kit 2, type 58)
        // Manifest: meta, in(Int,config), out(Int,runtime), set(Int,action)
        (2, 58) => vec![
            VirtualSlot { name: "meta".into(), type_id: SoxValueType::Int as u8, flags: SLOT_FLAG_CONFIG, value: SlotValue::Int(1) },
            VirtualSlot { name: "in".into(), type_id: SoxValueType::Int as u8, flags: SLOT_FLAG_CONFIG, value: SlotValue::Int(0) },
            VirtualSlot { name: "out".into(), type_id: SoxValueType::Int as u8, flags: SLOT_FLAG_RUNTIME, value: SlotValue::Int(0) },
            VirtualSlot { name: "set".into(), type_id: SoxValueType::Int as u8, flags: SLOT_FLAG_ACTION, value: SlotValue::Int(0) },
        ],
        // Default: meta slot + auto-extend on write for unknown types
        _ => vec![
            VirtualSlot {
                name: "meta".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(1),
            },
        ],
    }
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
    // Try manifest database first, fall back to hardcoded defaults
    let slots = if let Some(manifest_slots) = tree.manifest_db.get_slots(kit_id, type_id) {
        ManifestDb::slots_to_virtual(manifest_slots)
    } else {
        default_slots_for_type(kit_id, type_id)
    };
    let comp = VirtualComponent {
        comp_id: new_id,
        parent_id,
        name,
        type_name: format!("kit{}::type{}", kit_id, type_id),
        kit_id,
        type_id,
        children: Vec::new(),
        slots,
        links: Vec::new(),
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

/// link ('l') — add or delete a component link.
///
/// Request: u1 subcmd ('a'=add, 'd'=delete), u2 fromCompId, u1 fromSlotId, u2 toCompId, u1 toSlotId
/// Response: 'L' + replyNum
pub fn handle_link(req: &SoxRequest, tree: &mut ComponentTree) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let subcmd = match reader.read_u8() {
        Some(b) => b,
        None => return error_msg(req.cmd, req.req_id, "missing subcmd"),
    };
    let from_comp = match reader.read_u16() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing fromCompId"),
    };
    let from_slot = match reader.read_u8() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing fromSlotId"),
    };
    let to_comp = match reader.read_u16() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing toCompId"),
    };
    let to_slot = match reader.read_u8() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing toSlotId"),
    };

    match subcmd {
        b'a' => {
            if tree.add_link(from_comp, from_slot, to_comp, to_slot) {
                tracing::info!(from_comp, from_slot, to_comp, to_slot, "SOX: link added");
                SoxResponse::success(SoxCmd::Link, req.req_id)
            } else {
                error_msg(req.cmd, req.req_id, "link add failed")
            }
        }
        b'd' => {
            if tree.remove_link(from_comp, from_slot, to_comp, to_slot) {
                tracing::info!(from_comp, from_slot, to_comp, to_slot, "SOX: link removed");
                SoxResponse::success(SoxCmd::Link, req.req_id)
            } else {
                error_msg(req.cmd, req.req_id, "link not found")
            }
        }
        _ => error_msg(req.cmd, req.req_id, "unknown link subcmd"),
    }
}

/// reorder ('o') — reorder a parent component's children.
///
/// Request: u2 parentCompId, u1 childCount, u2[] childIds
/// Response: 'O' + replyNum
pub fn handle_reorder(req: &SoxRequest, tree: &mut ComponentTree) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let parent_id = match reader.read_u16() {
        Some(id) => id,
        None => return error_msg(req.cmd, req.req_id, "missing parentId"),
    };
    let count = match reader.read_u8() {
        Some(c) => c,
        None => return error_msg(req.cmd, req.req_id, "missing childCount"),
    };
    let mut child_ids = Vec::with_capacity(count as usize);
    for _ in 0..count {
        match reader.read_u16() {
            Some(id) => child_ids.push(id),
            None => return error_msg(req.cmd, req.req_id, "missing childId"),
        }
    }

    if tree.reorder_children(parent_id, &child_ids) {
        tracing::info!(parent_id, count, "SOX: children reordered");
        SoxResponse::success(SoxCmd::Reorder, req.req_id)
    } else {
        error_msg(req.cmd, req.req_id, "reorder failed")
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

    // Look up the action slot name and the "out" (or "in") slot type
    let (action_name, target_slot, target_type) = {
        let comp = tree.get(comp_id);
        let action = comp.and_then(|c| c.slots.get(slot_id as usize)).map(|s| s.name.clone());
        // Find the writable output: "out" for Const*, "in" for Write*
        let target = comp.and_then(|c| {
            c.slots.iter().find(|s| s.name == "out" && s.flags & SLOT_FLAG_ACTION == 0)
                .or_else(|| c.slots.iter().find(|s| s.name == "in" && s.flags & SLOT_FLAG_ACTION == 0))
        });
        let tname = target.map(|s| s.name.clone()).unwrap_or_default();
        let ttype = target.map(|s| s.type_id).unwrap_or(SoxValueType::Float as u8);
        (action.unwrap_or_default(), tname, ttype)
    };

    tracing::info!(comp_id, slot_id, action = %action_name, target = %target_slot, "SOX: invoke");

    // Handle action based on name
    let new_value = match action_name.as_str() {
        "set" => {
            // Parse argument based on target type
            if target_type == SoxValueType::Int as u8 {
                reader.read_i32().map(SlotValue::Int)
            } else if target_type == SoxValueType::Bool as u8 {
                reader.read_u8().map(|v| SlotValue::Bool(v != 0))
            } else {
                reader.read_f32().map(SlotValue::Float)
            }
        }
        "setTrue" => Some(SlotValue::Bool(true)),
        "setFalse" => Some(SlotValue::Bool(false)),
        "setNull" => Some(SlotValue::Null),
        _ => {
            // Unknown action — try parsing as float
            reader.read_f32().map(SlotValue::Float)
        }
    };

    if let Some(val) = new_value {
        if let Some(comp) = tree.get_mut(comp_id) {
            for slot in comp.slots.iter_mut() {
                if slot.name == target_slot {
                    slot.value = val.clone();
                    tracing::info!(comp_id, slot = %slot.name, ?val, "SOX: action applied");
                    break;
                }
            }
        }
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
            links: Vec::new(),
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
            links: Vec::new(),
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
            links: Vec::new(),
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
        // No links — payload should contain compId(0) + what('l') + 0xFFFF terminator
        // Payload: [0x00, 0x00, b'l', 0xFF, 0xFF]
        let payload = &resp.payload;
        assert_eq!(payload.len(), 5); // u2 compId + u1 what + u2 terminator
        assert_eq!(payload[3], 0xFF);
        assert_eq!(payload[4], 0xFF);
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

    // ---- ManifestDb tests ----

    #[test]
    fn manifest_db_new_is_empty() {
        let db = ManifestDb::new();
        assert_eq!(db.type_count(), 0);
        assert!(db.get_slots(0, 0).is_none());
    }

    #[test]
    fn manifest_db_parse_sys_component() {
        // Minimal sys manifest with Component type (id=9, has meta slot)
        let xml = r#"<?xml version='1.0'?>
<kitManifest name="sys" checksum="d3984c51" version="1.2.28">
<type id="9" name="Component" sizeof="60">
  <slot id="0" name="meta" type="int" flags="c" default="1"/>
</type>
</kitManifest>"#;

        let mut db = ManifestDb::new();
        let count = db.parse_kit_manifest(xml, 0); // kit_index=0 for sys
        assert_eq!(count, 1);
        let slots = db.get_slots(0, 9).unwrap();
        // Component has no base, so no inherited meta — just its own meta slot
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].name, "meta");
        assert_eq!(slots[0].type_id, SoxValueType::Int as u8);
        assert_eq!(slots[0].flags, SLOT_FLAG_CONFIG);
        assert_eq!(slots[0].default_value, SlotValue::Int(1));
    }

    #[test]
    fn manifest_db_parse_type_with_base() {
        // Type with base="sys::Component" should get inherited meta slot prepended
        let xml = r#"<?xml version='1.0'?>
<kitManifest name="control" checksum="808b7db3" version="1.2.28">
<type id="14" name="ConstFloat" sizeof="64" base="sys::Component">
  <slot id="0" name="out" type="float" flags="c"/>
  <slot id="1" name="set" type="float" flags="a"/>
</type>
</kitManifest>"#;

        let mut db = ManifestDb::new();
        let count = db.parse_kit_manifest(xml, 2); // kit_index=2 for control
        assert_eq!(count, 1);
        let slots = db.get_slots(2, 14).unwrap();
        // Should have: meta (inherited) + out + set = 3 slots
        assert_eq!(slots.len(), 3);
        assert_eq!(slots[0].name, "meta");
        assert_eq!(slots[0].flags, SLOT_FLAG_CONFIG);
        assert_eq!(slots[1].name, "out");
        assert_eq!(slots[1].type_id, SoxValueType::Float as u8);
        assert_eq!(slots[1].flags, SLOT_FLAG_CONFIG);
        assert_eq!(slots[2].name, "set");
        assert_eq!(slots[2].type_id, SoxValueType::Float as u8);
        assert_eq!(slots[2].flags, SLOT_FLAG_ACTION);
    }

    #[test]
    fn manifest_db_parse_self_closing_type() {
        // Self-closing type (no slots, like sys::void)
        let xml = r#"<?xml version='1.0'?>
<kitManifest name="sys" checksum="d3984c51" version="1.2.28">
<type id="0" name="void" sizeof="0"/>
<type id="1" name="bool" sizeof="1"/>
</kitManifest>"#;

        let mut db = ManifestDb::new();
        let count = db.parse_kit_manifest(xml, 0);
        assert_eq!(count, 2);
        // void (no base, no slots)
        let slots = db.get_slots(0, 0).unwrap();
        assert_eq!(slots.len(), 0);
        // bool (no base, no slots)
        let slots = db.get_slots(0, 1).unwrap();
        assert_eq!(slots.len(), 0);
    }

    #[test]
    fn manifest_db_parse_eacio_analog_input() {
        let xml = r#"<?xml version='1.0'?>
<kitManifest name="EacIo" checksum="6f9da65b" version="1.2.30">
<type id="0" name="AnalogInput" sizeof="368" base="sys::Component">
  <slot id="0" name="channelName" type="sys::Buf" flags="s" default=""/>
  <slot id="1" name="channel" type="int"/>
  <slot id="2" name="pointQuery" type="sys::Buf" flags="cs"/>
  <slot id="3" name="pointQuerySize" type="int"/>
  <slot id="4" name="pointQueryStatus" type="bool"/>
  <slot id="5" name="out" type="float" default="0.0"/>
  <slot id="6" name="curStatus" type="sys::Buf" flags="s" default="na"/>
  <slot id="7" name="enabled" type="bool"/>
  <slot id="8" name="query" type="void" flags="a"/>
</type>
</kitManifest>"#;

        let mut db = ManifestDb::new();
        let count = db.parse_kit_manifest(xml, 1); // kit_index=1 for EacIo
        assert_eq!(count, 1);
        let slots = db.get_slots(1, 0).unwrap();
        // meta (inherited) + 9 own slots = 10
        assert_eq!(slots.len(), 10);
        assert_eq!(slots[0].name, "meta");
        assert_eq!(slots[1].name, "channelName");
        assert_eq!(slots[1].type_id, SoxValueType::Buf as u8);
        assert_eq!(slots[1].flags, SLOT_FLAG_RUNTIME); // 's' flag alone = runtime
        assert_eq!(slots[6].name, "out");
        assert_eq!(slots[6].type_id, SoxValueType::Float as u8);
        assert_eq!(slots[6].default_value, SlotValue::Float(0.0));
        assert_eq!(slots[7].name, "curStatus");
        assert_eq!(slots[7].default_value, SlotValue::Str("na".into()));
        assert_eq!(slots[9].name, "query");
        assert_eq!(slots[9].flags, SLOT_FLAG_ACTION);
    }

    #[test]
    fn manifest_db_sedona_type_mapping() {
        assert_eq!(sedona_type_to_sox("int"), SoxValueType::Int as u8);
        assert_eq!(sedona_type_to_sox("float"), SoxValueType::Float as u8);
        assert_eq!(sedona_type_to_sox("bool"), SoxValueType::Bool as u8);
        assert_eq!(sedona_type_to_sox("void"), SoxValueType::Void as u8);
        assert_eq!(sedona_type_to_sox("sys::Buf"), SoxValueType::Buf as u8);
        assert_eq!(sedona_type_to_sox("byte"), SoxValueType::Byte as u8);
        assert_eq!(sedona_type_to_sox("short"), SoxValueType::Short as u8);
        assert_eq!(sedona_type_to_sox("long"), SoxValueType::Long as u8);
        assert_eq!(sedona_type_to_sox("double"), SoxValueType::Double as u8);
        // Unknown type defaults to Int
        assert_eq!(sedona_type_to_sox("somethingElse"), SoxValueType::Int as u8);
    }

    #[test]
    fn manifest_db_sedona_flags_mapping() {
        assert_eq!(sedona_flags_to_slot_flags("c"), SLOT_FLAG_CONFIG);
        assert_eq!(sedona_flags_to_slot_flags("a"), SLOT_FLAG_ACTION);
        assert_eq!(sedona_flags_to_slot_flags("cs"), SLOT_FLAG_CONFIG); // config + string hint
        assert_eq!(sedona_flags_to_slot_flags("s"), SLOT_FLAG_RUNTIME); // string hint alone = runtime
        assert_eq!(sedona_flags_to_slot_flags(""), SLOT_FLAG_RUNTIME);  // no flags = runtime
        assert_eq!(sedona_flags_to_slot_flags("o"), SLOT_FLAG_OPERATOR | SLOT_FLAG_RUNTIME);
    }

    #[test]
    fn manifest_db_default_value_parsing() {
        let int_type = SoxValueType::Int as u8;
        let float_type = SoxValueType::Float as u8;
        let bool_type = SoxValueType::Bool as u8;
        let buf_type = SoxValueType::Buf as u8;

        assert_eq!(parse_default_value(int_type, Some("42")), SlotValue::Int(42));
        assert_eq!(parse_default_value(float_type, Some("3.14")), SlotValue::Float(3.14));
        assert_eq!(parse_default_value(bool_type, Some("true")), SlotValue::Bool(true));
        assert_eq!(parse_default_value(bool_type, Some("false")), SlotValue::Bool(false));
        assert_eq!(parse_default_value(buf_type, Some("hello")), SlotValue::Str("hello".into()));
        assert_eq!(parse_default_value(buf_type, Some("")), SlotValue::Str(String::new()));
        assert_eq!(parse_default_value(int_type, None), SlotValue::Int(0));
        assert_eq!(parse_default_value(float_type, None), SlotValue::Float(0.0));
    }

    #[test]
    fn manifest_db_slots_to_virtual() {
        let manifest_slots = vec![
            ManifestSlot {
                name: "meta".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                default_value: SlotValue::Int(1),
            },
            ManifestSlot {
                name: "out".into(),
                type_id: SoxValueType::Float as u8,
                flags: SLOT_FLAG_CONFIG,
                default_value: SlotValue::Float(0.0),
            },
        ];
        let virtual_slots = ManifestDb::slots_to_virtual(&manifest_slots);
        assert_eq!(virtual_slots.len(), 2);
        assert_eq!(virtual_slots[0].name, "meta");
        assert_eq!(virtual_slots[0].value, SlotValue::Int(1));
        assert_eq!(virtual_slots[1].name, "out");
        assert_eq!(virtual_slots[1].value, SlotValue::Float(0.0));
    }

    #[test]
    fn manifest_db_handle_add_uses_manifest() {
        // Load a manifest with ConstFloat, then verify handle_add uses it
        let xml = r#"<?xml version='1.0'?>
<kitManifest name="control" checksum="808b7db3" version="1.2.28">
<type id="14" name="ConstFloat" sizeof="64" base="sys::Component">
  <slot id="0" name="out" type="float" flags="c"/>
  <slot id="1" name="set" type="float" flags="a"/>
</type>
</kitManifest>"#;

        let mut db = ManifestDb::new();
        db.parse_kit_manifest(xml, 2);
        let manifest_db = Arc::new(db);

        let mut tree = ComponentTree::new_with_manifest(manifest_db);
        // Add root and a parent folder
        tree.add(VirtualComponent {
            comp_id: 0,
            parent_id: NO_PARENT,
            name: "app".into(),
            type_name: "sys::App".into(),
            kit_id: 0,
            type_id: 10,
            children: Vec::new(),
            slots: Vec::new(),
            links: Vec::new(),
        });
        tree.add(VirtualComponent {
            comp_id: 6,
            parent_id: 0,
            name: "control".into(),
            type_name: "sys::Folder".into(),
            kit_id: 0,
            type_id: 11,
            children: Vec::new(),
            slots: Vec::new(),
            links: Vec::new(),
        });

        // Add a ConstFloat (kit=2, type=14) under control folder
        let mut payload = Vec::new();
        payload.extend_from_slice(&6u16.to_be_bytes()); // parentId=6
        payload.push(2);  // kitId=2 (control)
        payload.push(14); // typeId=14 (ConstFloat)
        // name (null-terminated)
        payload.extend_from_slice(b"myConst\0");

        let req = SoxRequest {
            cmd: SoxCmd::Add,
            req_id: 1,
            payload,
        };
        let resp = handle_add(&req, &mut tree);
        assert_eq!(resp.cmd, b'A');

        // Check the new component has manifest-derived slots
        let new_id = u16::from_be_bytes([resp.payload[0], resp.payload[1]]);
        let comp = tree.get(new_id).unwrap();
        assert_eq!(comp.slots.len(), 3); // meta + out + set
        assert_eq!(comp.slots[0].name, "meta");
        assert_eq!(comp.slots[1].name, "out");
        assert_eq!(comp.slots[1].type_id, SoxValueType::Float as u8);
        assert_eq!(comp.slots[2].name, "set");
        assert_eq!(comp.slots[2].flags, SLOT_FLAG_ACTION);
    }

    #[test]
    fn manifest_db_handle_add_falls_back_to_hardcoded() {
        // With empty manifest db, handle_add should use hardcoded defaults
        let mut tree = ComponentTree::new(); // no manifest
        tree.add(VirtualComponent {
            comp_id: 0,
            parent_id: NO_PARENT,
            name: "app".into(),
            type_name: "sys::App".into(),
            kit_id: 0,
            type_id: 10,
            children: Vec::new(),
            slots: Vec::new(),
            links: Vec::new(),
        });

        // Add a ConstFloat without manifest — should use hardcoded fallback
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u16.to_be_bytes()); // parentId=0
        payload.push(2);  // kitId=2 (control)
        payload.push(14); // typeId=14 (ConstFloat)
        payload.extend_from_slice(b"c\0");

        let req = SoxRequest {
            cmd: SoxCmd::Add,
            req_id: 1,
            payload,
        };
        let resp = handle_add(&req, &mut tree);
        assert_eq!(resp.cmd, b'A');

        let new_id = u16::from_be_bytes([resp.payload[0], resp.payload[1]]);
        let comp = tree.get(new_id).unwrap();
        // Hardcoded ConstFloat has 4 slots: meta, out, set, setNull
        assert_eq!(comp.slots.len(), 4);
        assert_eq!(comp.slots[0].name, "meta");
        assert_eq!(comp.slots[1].name, "out");
    }

    #[test]
    fn manifest_db_load_from_repo_manifests() {
        // Test loading from the actual SedonaRepo manifests in the workspace
        let manifests_dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../SedonaRepo/2026-03-11_21-56-18/manifests"
        );
        let path = std::path::Path::new(manifests_dir);
        if !path.exists() {
            // Skip test if manifests not available (CI, etc.)
            return;
        }
        let db = ManifestDb::load(manifests_dir);
        // Should have loaded types from multiple kits
        assert!(db.type_count() > 20, "Expected >20 types, got {}", db.type_count());

        // Verify specific types we know exist
        // sys::Component (kit=0, type=9)
        let sys_comp = db.get_slots(0, 9);
        assert!(sys_comp.is_some(), "sys::Component not found");
        assert_eq!(sys_comp.unwrap()[0].name, "meta");

        // control::ConstFloat (kit=2, type=14)
        let const_float = db.get_slots(2, 14);
        assert!(const_float.is_some(), "control::ConstFloat not found");
        let cf_slots = const_float.unwrap();
        assert_eq!(cf_slots[0].name, "meta"); // inherited
        assert_eq!(cf_slots[1].name, "out");
        assert_eq!(cf_slots[2].name, "set");

        // EacIo::AnalogInput (kit=1, type=0)
        let ai = db.get_slots(1, 0);
        assert!(ai.is_some(), "EacIo::AnalogInput not found");
        let ai_slots = ai.unwrap();
        assert_eq!(ai_slots[0].name, "meta"); // inherited
        assert_eq!(ai_slots[1].name, "channelName");

        // control::Add2 (kit=2, type=3)
        let add2 = db.get_slots(2, 3);
        assert!(add2.is_some(), "control::Add2 not found");
    }

    #[test]
    fn manifest_db_multiple_types_per_kit() {
        let xml = r#"<?xml version='1.0'?>
<kitManifest name="control" checksum="808b7db3" version="1.2.28">
<type id="3" name="Add2" sizeof="72" base="sys::Component">
  <slot id="0" name="out" type="float"/>
  <slot id="1" name="in1" type="float"/>
  <slot id="2" name="in2" type="float"/>
</type>
<type id="14" name="ConstFloat" sizeof="64" base="sys::Component">
  <slot id="0" name="out" type="float" flags="c"/>
  <slot id="1" name="set" type="float" flags="a"/>
</type>
<type id="18" name="Div2" sizeof="76" base="sys::Component">
  <slot id="0" name="out" type="float"/>
  <slot id="1" name="in1" type="float"/>
  <slot id="2" name="in2" type="float"/>
  <slot id="3" name="div0" type="bool"/>
</type>
</kitManifest>"#;

        let mut db = ManifestDb::new();
        let count = db.parse_kit_manifest(xml, 2);
        assert_eq!(count, 3);

        // Add2: meta + 3 slots = 4
        let add2 = db.get_slots(2, 3).unwrap();
        assert_eq!(add2.len(), 4);
        assert_eq!(add2[0].name, "meta");
        assert_eq!(add2[1].name, "out");

        // ConstFloat: meta + 2 slots = 3
        let cf = db.get_slots(2, 14).unwrap();
        assert_eq!(cf.len(), 3);

        // Div2: meta + 4 slots = 5
        let div2 = db.get_slots(2, 18).unwrap();
        assert_eq!(div2.len(), 5);
        assert_eq!(div2[4].name, "div0");
        assert_eq!(div2[4].type_id, SoxValueType::Bool as u8);
    }

    // ---- Link tests ----

    #[test]
    fn handle_link_add() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        // Add a link: from comp 100 slot 6 -> to comp 101 slot 1
        let mut payload = Vec::new();
        payload.push(b'a'); // subcmd = add
        payload.extend_from_slice(&100u16.to_be_bytes()); // fromCompId
        payload.push(6); // fromSlotId
        payload.extend_from_slice(&101u16.to_be_bytes()); // toCompId
        payload.push(1); // toSlotId
        let req = SoxRequest { cmd: SoxCmd::Link, req_id: 10, payload };
        let resp = handle_link(&req, &mut tree);
        assert_eq!(resp.cmd, b'L');
        assert_eq!(resp.req_id, 10);
        // Verify link is stored on the destination component
        let comp = tree.get(101).unwrap();
        assert_eq!(comp.links.len(), 1);
        assert_eq!(comp.links[0].from_comp, 100);
        assert_eq!(comp.links[0].from_slot, 6);
        assert_eq!(comp.links[0].to_comp, 101);
        assert_eq!(comp.links[0].to_slot, 1);
    }

    #[test]
    fn handle_link_delete() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        // First add a link
        tree.add_link(100, 6, 101, 1);
        assert_eq!(tree.get(101).unwrap().links.len(), 1);
        // Now delete it via handler
        let mut payload = Vec::new();
        payload.push(b'd'); // subcmd = delete
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(6);
        payload.extend_from_slice(&101u16.to_be_bytes());
        payload.push(1);
        let req = SoxRequest { cmd: SoxCmd::Link, req_id: 11, payload };
        let resp = handle_link(&req, &mut tree);
        assert_eq!(resp.cmd, b'L');
        assert_eq!(tree.get(101).unwrap().links.len(), 0);
    }

    #[test]
    fn handle_link_add_duplicate_fails() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        tree.add_link(100, 6, 101, 1);
        // Try adding the same link again — should fail
        let mut payload = Vec::new();
        payload.push(b'a');
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(6);
        payload.extend_from_slice(&101u16.to_be_bytes());
        payload.push(1);
        let req = SoxRequest { cmd: SoxCmd::Link, req_id: 12, payload };
        let resp = handle_link(&req, &mut tree);
        assert_eq!(resp.cmd, b'!'); // error
    }

    #[test]
    fn handle_link_delete_nonexistent_fails() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        let mut payload = Vec::new();
        payload.push(b'd');
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(6);
        payload.extend_from_slice(&101u16.to_be_bytes());
        payload.push(1);
        let req = SoxRequest { cmd: SoxCmd::Link, req_id: 13, payload };
        let resp = handle_link(&req, &mut tree);
        assert_eq!(resp.cmd, b'!'); // error — link not found
    }

    #[test]
    fn handle_link_unknown_subcmd_fails() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        let mut payload = Vec::new();
        payload.push(b'x'); // unknown subcmd
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(6);
        payload.extend_from_slice(&101u16.to_be_bytes());
        payload.push(1);
        let req = SoxRequest { cmd: SoxCmd::Link, req_id: 14, payload };
        let resp = handle_link(&req, &mut tree);
        assert_eq!(resp.cmd, b'!');
    }

    #[test]
    fn read_comp_links_with_data() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        // Add two links to component 101
        tree.add_link(100, 6, 101, 1);
        tree.add_link(100, 7, 101, 2);
        // Read links for comp 101
        let mut payload = Vec::new();
        payload.extend_from_slice(&101u16.to_be_bytes());
        payload.push(b'l');
        let req = SoxRequest { cmd: SoxCmd::ReadComp, req_id: 15, payload };
        let resp = handle_read_comp(&req, &tree);
        assert_eq!(resp.cmd, b'C');
        // Payload: u2 compId(101) + u1 what('l') + 2 links + u2 terminator
        // Each link: u2 fromComp + u1 fromSlot + u2 toComp + u1 toSlot = 6 bytes
        // Total: 3 + 12 + 2 = 17 bytes
        let p = &resp.payload;
        assert_eq!(p.len(), 17);
        // First link: from=100, slot=6, to=101, slot=1
        assert_eq!(u16::from_be_bytes([p[3], p[4]]), 100); // fromComp
        assert_eq!(p[5], 6); // fromSlot
        assert_eq!(u16::from_be_bytes([p[6], p[7]]), 101); // toComp
        assert_eq!(p[8], 1); // toSlot
        // Second link
        assert_eq!(u16::from_be_bytes([p[9], p[10]]), 100);
        assert_eq!(p[11], 7);
        assert_eq!(u16::from_be_bytes([p[12], p[13]]), 101);
        assert_eq!(p[14], 2);
        // Terminator
        assert_eq!(u16::from_be_bytes([p[15], p[16]]), 0xFFFF);
    }

    // ---- Reorder tests ----

    #[test]
    fn handle_reorder_success() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        // The io folder (comp 5) has children from sample_channels
        let io = tree.get(5).unwrap();
        let original_children = io.children.clone();
        assert!(original_children.len() >= 2, "need at least 2 children to reorder");
        // Reverse the children order
        let mut reversed = original_children.clone();
        reversed.reverse();
        let mut payload = Vec::new();
        payload.extend_from_slice(&5u16.to_be_bytes()); // parentId
        payload.push(reversed.len() as u8); // count
        for &id in &reversed {
            payload.extend_from_slice(&id.to_be_bytes());
        }
        let req = SoxRequest { cmd: SoxCmd::Reorder, req_id: 20, payload };
        let resp = handle_reorder(&req, &mut tree);
        assert_eq!(resp.cmd, b'O');
        assert_eq!(tree.get(5).unwrap().children, reversed);
    }

    #[test]
    fn handle_reorder_wrong_children_fails() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        // Try reordering with a child that doesn't belong
        let mut payload = Vec::new();
        payload.extend_from_slice(&5u16.to_be_bytes()); // parentId = io folder
        payload.push(1); // count
        payload.extend_from_slice(&999u16.to_be_bytes()); // bogus child
        let req = SoxRequest { cmd: SoxCmd::Reorder, req_id: 21, payload };
        let resp = handle_reorder(&req, &mut tree);
        assert_eq!(resp.cmd, b'!');
    }

    #[test]
    fn handle_reorder_nonexistent_parent_fails() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        let mut payload = Vec::new();
        payload.extend_from_slice(&999u16.to_be_bytes()); // nonexistent parent
        payload.push(0); // count=0
        let req = SoxRequest { cmd: SoxCmd::Reorder, req_id: 22, payload };
        let resp = handle_reorder(&req, &mut tree);
        assert_eq!(resp.cmd, b'!');
    }

    #[test]
    fn get_links_returns_both_directions() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        tree.add_link(100, 6, 101, 1);
        tree.add_link(101, 6, 100, 2);
        // Links for comp 100: one as source (from=100), one as destination (to=100)
        let links = tree.get_links(100);
        assert_eq!(links.len(), 2);
        // Links for comp 101: one as destination (to=101), one as source (from=101)
        let links = tree.get_links(101);
        assert_eq!(links.len(), 2);
    }

    // ---- execute_links tests ----

    /// Helper: create an Add2 component (kit_id=2, type_id=3) with standard slots:
    /// slot 0 = meta (Int), slot 1 = out (Float), slot 2 = in1 (Float), slot 3 = in2 (Float)
    fn make_math_comp(comp_id: u16, parent_id: u16, name: &str, kit_id: u8, type_id: u8) -> VirtualComponent {
        VirtualComponent {
            comp_id,
            parent_id,
            name: name.into(),
            type_name: format!("math::{name}"),
            kit_id,
            type_id,
            children: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 4, flags: 0, value: SlotValue::Float(0.0) },
                VirtualSlot { name: "in1".into(), type_id: 4, flags: 0, value: SlotValue::Float(0.0) },
                VirtualSlot { name: "in2".into(), type_id: 4, flags: 0, value: SlotValue::Float(0.0) },
            ],
            links: Vec::new(),
        }
    }

    /// Helper: create a simple component with a single float output slot at index 1
    fn make_source_comp(comp_id: u16, parent_id: u16, name: &str, out_value: f32) -> VirtualComponent {
        VirtualComponent {
            comp_id,
            parent_id,
            name: name.into(),
            type_name: "func::ConstFloat".into(),
            kit_id: 2,
            type_id: 255, // arbitrary non-executable type
            children: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 4, flags: 0, value: SlotValue::Float(out_value) },
            ],
            links: Vec::new(),
        }
    }

    #[test]
    fn execute_links_propagates_value() {
        let mut tree = ComponentTree::new();
        // Source component with out=42.0 at slot 1
        tree.add(make_source_comp(200, NO_PARENT, "src", 42.0));
        // Target Add2 component with in1 at slot 2
        tree.add(make_math_comp(201, NO_PARENT, "add", 2, 3));
        // Link: src.out(slot 1) -> add.in1(slot 2)
        tree.add_link(200, 1, 201, 2);

        let changed = tree.execute_links();
        assert!(changed.contains(&201), "target should be in changed list");

        let target = tree.get(201).unwrap();
        match &target.slots[2].value {
            SlotValue::Float(v) => assert_eq!(*v, 42.0),
            other => panic!("expected Float, got {:?}", other),
        }
    }

    #[test]
    fn execute_links_no_change_returns_empty() {
        let mut tree = ComponentTree::new();
        tree.add(make_source_comp(200, NO_PARENT, "src", 0.0));
        tree.add(make_math_comp(201, NO_PARENT, "add", 2, 3));
        // Link src.out(0.0) -> add.in1 (already 0.0)
        tree.add_link(200, 1, 201, 2);

        let changed = tree.execute_links();
        assert!(changed.is_empty(), "no value change should mean empty list");
    }

    #[test]
    fn execute_links_chain_propagation() {
        // Test: src(42.0) -> add1.in1, then add1.out -> add2.in1
        // After one execute_links call, add1.in1=42.0 but add1.out is still 0.0
        // (execute_components hasn't run yet), so add2.in1 should get 0.0
        let mut tree = ComponentTree::new();
        tree.add(make_source_comp(200, NO_PARENT, "src", 42.0));
        tree.add(make_math_comp(201, NO_PARENT, "add1", 2, 3));
        tree.add(make_math_comp(202, NO_PARENT, "add2", 2, 3));
        tree.add_link(200, 1, 201, 2); // src.out -> add1.in1
        tree.add_link(201, 1, 202, 2); // add1.out -> add2.in1

        let changed = tree.execute_links();
        // add1.in1 changed from 0.0 to 42.0
        assert!(changed.contains(&201));
        // add1.out is still 0.0 so add2.in1 stays 0.0 — no change
        let add2 = tree.get(202).unwrap();
        match &add2.slots[2].value {
            SlotValue::Float(v) => assert_eq!(*v, 0.0),
            other => panic!("expected Float, got {:?}", other),
        }
    }

    // ---- execute_components tests ----

    #[test]
    fn execute_components_add2() {
        let mut tree = ComponentTree::new();
        let mut add = make_math_comp(200, NO_PARENT, "add", 2, 3);
        add.slots[2].value = SlotValue::Float(10.0); // in1
        add.slots[3].value = SlotValue::Float(20.0); // in2
        tree.add(add);

        let changed = tree.execute_components();
        assert!(changed.contains(&200));
        let comp = tree.get(200).unwrap();
        match &comp.slots[1].value {
            SlotValue::Float(v) => assert_eq!(*v, 30.0),
            other => panic!("expected Float(30.0), got {:?}", other),
        }
    }

    #[test]
    fn execute_components_sub2() {
        let mut tree = ComponentTree::new();
        let mut sub = make_math_comp(200, NO_PARENT, "sub", 2, 49);
        sub.slots[2].value = SlotValue::Float(50.0);
        sub.slots[3].value = SlotValue::Float(20.0);
        tree.add(sub);

        let changed = tree.execute_components();
        assert!(changed.contains(&200));
        let comp = tree.get(200).unwrap();
        match &comp.slots[1].value {
            SlotValue::Float(v) => assert_eq!(*v, 30.0),
            other => panic!("expected Float(30.0), got {:?}", other),
        }
    }

    #[test]
    fn execute_components_mul2() {
        let mut tree = ComponentTree::new();
        let mut mul = make_math_comp(200, NO_PARENT, "mul", 2, 37);
        mul.slots[2].value = SlotValue::Float(6.0);
        mul.slots[3].value = SlotValue::Float(7.0);
        tree.add(mul);

        let changed = tree.execute_components();
        assert!(changed.contains(&200));
        let comp = tree.get(200).unwrap();
        match &comp.slots[1].value {
            SlotValue::Float(v) => assert_eq!(*v, 42.0),
            other => panic!("expected Float(42.0), got {:?}", other),
        }
    }

    #[test]
    fn execute_components_div2_normal() {
        let mut tree = ComponentTree::new();
        let mut div = make_math_comp(200, NO_PARENT, "div", 2, 18);
        // Add div0 slot at index 4
        div.slots.push(VirtualSlot { name: "div0".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) });
        div.slots[2].value = SlotValue::Float(100.0);
        div.slots[3].value = SlotValue::Float(4.0);
        tree.add(div);

        let changed = tree.execute_components();
        assert!(changed.contains(&200));
        let comp = tree.get(200).unwrap();
        match &comp.slots[1].value {
            SlotValue::Float(v) => assert_eq!(*v, 25.0),
            other => panic!("expected Float(25.0), got {:?}", other),
        }
        match &comp.slots[4].value {
            SlotValue::Bool(v) => assert!(!v, "div0 should be false"),
            other => panic!("expected Bool(false), got {:?}", other),
        }
    }

    #[test]
    fn execute_components_div2_by_zero() {
        let mut tree = ComponentTree::new();
        let mut div = make_math_comp(200, NO_PARENT, "div", 2, 18);
        div.slots.push(VirtualSlot { name: "div0".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) });
        div.slots[2].value = SlotValue::Float(100.0);
        div.slots[3].value = SlotValue::Float(0.0); // divide by zero
        tree.add(div);

        let changed = tree.execute_components();
        assert!(changed.contains(&200));
        let comp = tree.get(200).unwrap();
        match &comp.slots[1].value {
            SlotValue::Float(v) => assert_eq!(*v, 0.0, "div by zero should produce 0.0"),
            other => panic!("expected Float(0.0), got {:?}", other),
        }
        match &comp.slots[4].value {
            SlotValue::Bool(v) => assert!(v, "div0 flag should be true"),
            other => panic!("expected Bool(true), got {:?}", other),
        }
    }

    #[test]
    fn execute_components_no_change_when_already_computed() {
        let mut tree = ComponentTree::new();
        let mut add = make_math_comp(200, NO_PARENT, "add", 2, 3);
        add.slots[2].value = SlotValue::Float(10.0);
        add.slots[3].value = SlotValue::Float(20.0);
        add.slots[1].value = SlotValue::Float(30.0); // already correct
        tree.add(add);

        let changed = tree.execute_components();
        assert!(changed.is_empty(), "no change when output already correct");
    }

    #[test]
    fn execute_components_ignores_unknown_types() {
        let mut tree = ComponentTree::new();
        // kit_id=2, type_id=255 is not an executable component — should be ignored
        tree.add(make_source_comp(200, NO_PARENT, "const", 99.0));

        let changed = tree.execute_components();
        assert!(changed.is_empty());
    }

    // ---- Helper: create a math component with N float input slots ----
    fn make_math_comp_n(comp_id: u16, parent_id: u16, name: &str, kit_id: u8, type_id: u8, num_inputs: usize) -> VirtualComponent {
        let mut slots = vec![
            VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
            VirtualSlot { name: "out".into(), type_id: 4, flags: 0, value: SlotValue::Float(0.0) },
        ];
        for i in 0..num_inputs {
            slots.push(VirtualSlot { name: format!("in{}", i + 1), type_id: 4, flags: 0, value: SlotValue::Float(0.0) });
        }
        VirtualComponent {
            comp_id, parent_id, name: name.into(), type_name: format!("math::{name}"),
            kit_id, type_id, children: Vec::new(), slots, links: Vec::new(),
        }
    }

    /// Helper: create a bool-logic component with N bool input slots
    fn make_bool_comp(comp_id: u16, name: &str, kit_id: u8, type_id: u8, num_inputs: usize) -> VirtualComponent {
        let mut slots = vec![
            VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
            VirtualSlot { name: "out".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) },
        ];
        for i in 0..num_inputs {
            slots.push(VirtualSlot { name: format!("in{}", i + 1), type_id: 0, flags: 0, value: SlotValue::Bool(false) });
        }
        VirtualComponent {
            comp_id, parent_id: NO_PARENT, name: name.into(), type_name: format!("logic::{name}"),
            kit_id, type_id, children: Vec::new(), slots, links: Vec::new(),
        }
    }

    // ---- Arithmetic (4-input) tests ----

    #[test]
    fn execute_add4() {
        let mut tree = ComponentTree::new();
        let mut c = make_math_comp_n(200, NO_PARENT, "add4", 2, 4, 4);
        c.slots[2].value = SlotValue::Float(1.0);
        c.slots[3].value = SlotValue::Float(2.0);
        c.slots[4].value = SlotValue::Float(3.0);
        c.slots[5].value = SlotValue::Float(4.0);
        tree.add(c);
        let changed = tree.execute_components();
        assert!(changed.contains(&200));
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(10.0));
    }

    #[test]
    fn execute_sub4() {
        let mut tree = ComponentTree::new();
        let mut c = make_math_comp_n(200, NO_PARENT, "sub4", 2, 50, 4);
        c.slots[2].value = SlotValue::Float(100.0);
        c.slots[3].value = SlotValue::Float(10.0);
        c.slots[4].value = SlotValue::Float(20.0);
        c.slots[5].value = SlotValue::Float(30.0);
        tree.add(c);
        let changed = tree.execute_components();
        assert!(changed.contains(&200));
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(40.0));
    }

    #[test]
    fn execute_mul4() {
        let mut tree = ComponentTree::new();
        let mut c = make_math_comp_n(200, NO_PARENT, "mul4", 2, 38, 4);
        c.slots[2].value = SlotValue::Float(2.0);
        c.slots[3].value = SlotValue::Float(3.0);
        c.slots[4].value = SlotValue::Float(4.0);
        c.slots[5].value = SlotValue::Float(5.0);
        tree.add(c);
        let changed = tree.execute_components();
        assert!(changed.contains(&200));
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(120.0));
    }

    // ---- Unary Math tests ----

    #[test]
    fn execute_neg() {
        let mut tree = ComponentTree::new();
        let mut c = make_math_comp_n(200, NO_PARENT, "neg", 2, 39, 1);
        c.slots[2].value = SlotValue::Float(42.5);
        tree.add(c);
        let changed = tree.execute_components();
        assert!(changed.contains(&200));
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(-42.5));
    }

    #[test]
    fn execute_float_offset() {
        let mut tree = ComponentTree::new();
        let mut c = make_math_comp_n(200, NO_PARENT, "foff", 2, 23, 2);
        c.slots[2].value = SlotValue::Float(10.0);
        c.slots[3].value = SlotValue::Float(5.5);
        tree.add(c);
        let changed = tree.execute_components();
        assert!(changed.contains(&200));
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(15.5));
    }

    #[test]
    fn execute_max() {
        let mut tree = ComponentTree::new();
        let mut c = make_math_comp_n(200, NO_PARENT, "max", 2, 34, 2);
        c.slots[2].value = SlotValue::Float(3.0);
        c.slots[3].value = SlotValue::Float(7.0);
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(7.0));
    }

    #[test]
    fn execute_min() {
        let mut tree = ComponentTree::new();
        let mut c = make_math_comp_n(200, NO_PARENT, "min", 2, 35, 2);
        c.slots[2].value = SlotValue::Float(3.0);
        c.slots[3].value = SlotValue::Float(7.0);
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(3.0));
    }

    #[test]
    fn execute_limiter_clamps_low() {
        let mut tree = ComponentTree::new();
        let mut c = make_math_comp_n(200, NO_PARENT, "lim", 2, 32, 3);
        c.slots[2].value = SlotValue::Float(-5.0); // in
        c.slots[3].value = SlotValue::Float(0.0);  // lowLmt
        c.slots[4].value = SlotValue::Float(100.0); // highLmt
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(0.0));
    }

    #[test]
    fn execute_limiter_clamps_high() {
        let mut tree = ComponentTree::new();
        let mut c = make_math_comp_n(200, NO_PARENT, "lim", 2, 32, 3);
        c.slots[2].value = SlotValue::Float(150.0);
        c.slots[3].value = SlotValue::Float(0.0);
        c.slots[4].value = SlotValue::Float(100.0);
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(100.0));
    }

    #[test]
    fn execute_limiter_passthrough() {
        let mut tree = ComponentTree::new();
        let mut c = make_math_comp_n(200, NO_PARENT, "lim", 2, 32, 3);
        c.slots[2].value = SlotValue::Float(50.0);
        c.slots[3].value = SlotValue::Float(0.0);
        c.slots[4].value = SlotValue::Float(100.0);
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(50.0));
    }

    #[test]
    fn execute_round() {
        let mut tree = ComponentTree::new();
        let mut c = make_math_comp_n(200, NO_PARENT, "rnd", 2, 47, 1);
        // Add decimalPlaces config slot at index 3
        c.slots.push(VirtualSlot { name: "decimalPlaces".into(), type_id: 1, flags: 0, value: SlotValue::Int(2) });
        c.slots[2].value = SlotValue::Float(3.14159);
        tree.add(c);
        tree.execute_components();
        let out = match tree.get(200).unwrap().slots[1].value {
            SlotValue::Float(v) => v,
            _ => panic!("expected Float"),
        };
        assert!((out - 3.14).abs() < 0.005, "expected ~3.14, got {out}");
    }

    // ---- Boolean Logic tests ----

    #[test]
    fn execute_and2_true() {
        let mut tree = ComponentTree::new();
        let mut c = make_bool_comp(200, "and2", 2, 5, 2);
        c.slots[2].value = SlotValue::Bool(true);
        c.slots[3].value = SlotValue::Bool(true);
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(true));
    }

    #[test]
    fn execute_and2_false() {
        let mut tree = ComponentTree::new();
        let mut c = make_bool_comp(200, "and2", 2, 5, 2);
        c.slots[2].value = SlotValue::Bool(true);
        c.slots[3].value = SlotValue::Bool(false);
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(false));
    }

    #[test]
    fn execute_and4() {
        let mut tree = ComponentTree::new();
        let mut c = make_bool_comp(200, "and4", 2, 6, 4);
        c.slots[2].value = SlotValue::Bool(true);
        c.slots[3].value = SlotValue::Bool(true);
        c.slots[4].value = SlotValue::Bool(true);
        c.slots[5].value = SlotValue::Bool(false);
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(false));
    }

    #[test]
    fn execute_or2() {
        let mut tree = ComponentTree::new();
        let mut c = make_bool_comp(200, "or2", 2, 42, 2);
        c.slots[2].value = SlotValue::Bool(false);
        c.slots[3].value = SlotValue::Bool(true);
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(true));
    }

    #[test]
    fn execute_or4_all_false() {
        let mut tree = ComponentTree::new();
        let c = make_bool_comp(200, "or4", 2, 43, 4);
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(false));
    }

    #[test]
    fn execute_not() {
        let mut tree = ComponentTree::new();
        let mut c = make_bool_comp(200, "not", 2, 40, 1);
        c.slots[2].value = SlotValue::Bool(false);
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(true));
    }

    #[test]
    fn execute_xor_diff() {
        let mut tree = ComponentTree::new();
        let mut c = make_bool_comp(200, "xor", 2, 59, 2);
        c.slots[2].value = SlotValue::Bool(true);
        c.slots[3].value = SlotValue::Bool(false);
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(true));
    }

    #[test]
    fn execute_xor_same() {
        let mut tree = ComponentTree::new();
        let mut c = make_bool_comp(200, "xor", 2, 59, 2);
        c.slots[2].value = SlotValue::Bool(true);
        c.slots[3].value = SlotValue::Bool(true);
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(false));
    }

    // ---- Comparator test ----

    #[test]
    fn execute_cmpr() {
        let mut tree = ComponentTree::new();
        // Cmpr: meta=0, xgy=1, xey=2, xly=3, x=4, y=5
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "cmpr".into(),
            type_name: "math::Cmpr".into(), kit_id: 2, type_id: 12,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "xgy".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) },
                VirtualSlot { name: "xey".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) },
                VirtualSlot { name: "xly".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) },
                VirtualSlot { name: "x".into(), type_id: 4, flags: 0, value: SlotValue::Float(10.0) },
                VirtualSlot { name: "y".into(), type_id: 4, flags: 0, value: SlotValue::Float(5.0) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        let comp = tree.get(200).unwrap();
        assert_eq!(comp.slots[1].value, SlotValue::Bool(true),  "x > y");
        assert_eq!(comp.slots[2].value, SlotValue::Bool(false), "x == y");
        assert_eq!(comp.slots[3].value, SlotValue::Bool(false), "x < y");
    }

    #[test]
    fn execute_cmpr_equal() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "cmpr".into(),
            type_name: "math::Cmpr".into(), kit_id: 2, type_id: 12,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "xgy".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) },
                VirtualSlot { name: "xey".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) },
                VirtualSlot { name: "xly".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) },
                VirtualSlot { name: "x".into(), type_id: 4, flags: 0, value: SlotValue::Float(7.0) },
                VirtualSlot { name: "y".into(), type_id: 4, flags: 0, value: SlotValue::Float(7.0) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        let comp = tree.get(200).unwrap();
        assert_eq!(comp.slots[1].value, SlotValue::Bool(false), "x > y");
        assert_eq!(comp.slots[2].value, SlotValue::Bool(true),  "x == y");
        assert_eq!(comp.slots[3].value, SlotValue::Bool(false), "x < y");
    }

    // ---- Type Conversion tests ----

    #[test]
    fn execute_b2p() {
        let mut tree = ComponentTree::new();
        let mut c = make_bool_comp(200, "b2p", 2, 10, 1);
        c.slots[2].value = SlotValue::Bool(true);
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(true));
    }

    #[test]
    fn execute_f2i() {
        let mut tree = ComponentTree::new();
        let mut c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "f2i".into(),
            type_name: "math::F2I".into(), kit_id: 2, type_id: 22,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "in".into(), type_id: 4, flags: 0, value: SlotValue::Float(42.7) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Int(42));
    }

    #[test]
    fn execute_i2f() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "i2f".into(),
            type_name: "math::I2F".into(), kit_id: 2, type_id: 26,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 4, flags: 0, value: SlotValue::Float(0.0) },
                VirtualSlot { name: "in".into(), type_id: 1, flags: 0, value: SlotValue::Int(99) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(99.0));
    }

    // ---- Multiplexer tests ----

    #[test]
    fn execute_asw_sel_false() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "asw".into(),
            type_name: "math::ASW".into(), kit_id: 2, type_id: 1,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 4, flags: 0, value: SlotValue::Float(0.0) },
                VirtualSlot { name: "sel".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) },
                VirtualSlot { name: "in1".into(), type_id: 4, flags: 0, value: SlotValue::Float(10.0) },
                VirtualSlot { name: "in2".into(), type_id: 4, flags: 0, value: SlotValue::Float(20.0) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(10.0));
    }

    #[test]
    fn execute_asw_sel_true() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "asw".into(),
            type_name: "math::ASW".into(), kit_id: 2, type_id: 1,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 4, flags: 0, value: SlotValue::Float(0.0) },
                VirtualSlot { name: "sel".into(), type_id: 0, flags: 0, value: SlotValue::Bool(true) },
                VirtualSlot { name: "in1".into(), type_id: 4, flags: 0, value: SlotValue::Float(10.0) },
                VirtualSlot { name: "in2".into(), type_id: 4, flags: 0, value: SlotValue::Float(20.0) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(20.0));
    }

    #[test]
    fn execute_bsw() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "bsw".into(),
            type_name: "logic::BSW".into(), kit_id: 2, type_id: 11,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) },
                VirtualSlot { name: "sel".into(), type_id: 0, flags: 0, value: SlotValue::Bool(true) },
                VirtualSlot { name: "in1".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) },
                VirtualSlot { name: "in2".into(), type_id: 0, flags: 0, value: SlotValue::Bool(true) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(true));
    }

    #[test]
    fn execute_isw() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "isw".into(),
            type_name: "math::ISW".into(), kit_id: 2, type_id: 28,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "sel".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) },
                VirtualSlot { name: "in1".into(), type_id: 1, flags: 0, value: SlotValue::Int(42) },
                VirtualSlot { name: "in2".into(), type_id: 1, flags: 0, value: SlotValue::Int(99) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Int(42));
    }

    // ---- Hysteresis, SRLatch, Reset, Write*, LSeq tests ----

    #[test]
    fn execute_hysteresis_rising() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "hyst".into(),
            type_name: "control::Hysteresis".into(), kit_id: 2, type_id: 25,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) },
                VirtualSlot { name: "in".into(), type_id: 2, flags: 0, value: SlotValue::Float(75.0) },
                VirtualSlot { name: "rising".into(), type_id: 2, flags: SLOT_FLAG_CONFIG, value: SlotValue::Float(72.0) },
                VirtualSlot { name: "falling".into(), type_id: 2, flags: SLOT_FLAG_CONFIG, value: SlotValue::Float(68.0) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        // in=75 > rising=72, out was false → switches to true
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(true));
    }

    #[test]
    fn execute_hysteresis_deadband() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "hyst".into(),
            type_name: "control::Hysteresis".into(), kit_id: 2, type_id: 25,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 0, flags: 0, value: SlotValue::Bool(true) },
                VirtualSlot { name: "in".into(), type_id: 2, flags: 0, value: SlotValue::Float(70.0) },
                VirtualSlot { name: "rising".into(), type_id: 2, flags: SLOT_FLAG_CONFIG, value: SlotValue::Float(72.0) },
                VirtualSlot { name: "falling".into(), type_id: 2, flags: SLOT_FLAG_CONFIG, value: SlotValue::Float(68.0) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        // in=70, out=true, 70 >= falling(68) → stays true (in deadband)
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(true));
    }

    #[test]
    fn execute_hysteresis_falling() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "hyst".into(),
            type_name: "control::Hysteresis".into(), kit_id: 2, type_id: 25,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 0, flags: 0, value: SlotValue::Bool(true) },
                VirtualSlot { name: "in".into(), type_id: 2, flags: 0, value: SlotValue::Float(65.0) },
                VirtualSlot { name: "rising".into(), type_id: 2, flags: SLOT_FLAG_CONFIG, value: SlotValue::Float(72.0) },
                VirtualSlot { name: "falling".into(), type_id: 2, flags: SLOT_FLAG_CONFIG, value: SlotValue::Float(68.0) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        // in=65, out=true, 65 < falling(68) → switches to false
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(false));
    }

    #[test]
    fn execute_sr_latch_set() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "sr".into(),
            type_name: "control::SRLatch".into(), kit_id: 2, type_id: 48,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) },
                VirtualSlot { name: "set".into(), type_id: 0, flags: 0, value: SlotValue::Bool(true) },
                VirtualSlot { name: "reset".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(true));
    }

    #[test]
    fn execute_sr_latch_reset_wins() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "sr".into(),
            type_name: "control::SRLatch".into(), kit_id: 2, type_id: 48,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 0, flags: 0, value: SlotValue::Bool(true) },
                VirtualSlot { name: "set".into(), type_id: 0, flags: 0, value: SlotValue::Bool(true) },
                VirtualSlot { name: "reset".into(), type_id: 0, flags: 0, value: SlotValue::Bool(true) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        // reset wins over set
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(false));
    }

    #[test]
    fn execute_reset_remap() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "rst".into(),
            type_name: "control::Reset".into(), kit_id: 2, type_id: 46,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 2, flags: 0, value: SlotValue::Float(0.0) },
                VirtualSlot { name: "in".into(), type_id: 2, flags: 0, value: SlotValue::Float(50.0) },
                VirtualSlot { name: "inLow".into(), type_id: 2, flags: SLOT_FLAG_CONFIG, value: SlotValue::Float(0.0) },
                VirtualSlot { name: "inHigh".into(), type_id: 2, flags: SLOT_FLAG_CONFIG, value: SlotValue::Float(100.0) },
                VirtualSlot { name: "outLow".into(), type_id: 2, flags: SLOT_FLAG_CONFIG, value: SlotValue::Float(55.0) },
                VirtualSlot { name: "outHigh".into(), type_id: 2, flags: SLOT_FLAG_CONFIG, value: SlotValue::Float(85.0) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        // 50% of [0,100] → 50% of [55,85] = 70.0
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(70.0));
    }

    #[test]
    fn execute_write_float_passthrough() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "wf".into(),
            type_name: "control::WriteFloat".into(), kit_id: 2, type_id: 57,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 2, flags: 0, value: SlotValue::Float(0.0) },
                VirtualSlot { name: "in".into(), type_id: 2, flags: 0, value: SlotValue::Float(42.5) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(42.5));
    }

    #[test]
    fn execute_write_bool_passthrough() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "wb".into(),
            type_name: "control::WriteBool".into(), kit_id: 2, type_id: 56,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 0, flags: 0, value: SlotValue::Bool(false) },
                VirtualSlot { name: "in".into(), type_id: 0, flags: 0, value: SlotValue::Bool(true) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(true));
    }

    #[test]
    fn execute_lseq() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200, parent_id: NO_PARENT, name: "seq".into(),
            type_name: "control::LSeq".into(), kit_id: 2, type_id: 31,
            children: Vec::new(), links: Vec::new(),
            slots: vec![
                VirtualSlot { name: "meta".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "out".into(), type_id: 1, flags: 0, value: SlotValue::Int(0) },
                VirtualSlot { name: "in".into(), type_id: 2, flags: 0, value: SlotValue::Float(0.6) },
                VirtualSlot { name: "numStages".into(), type_id: 1, flags: SLOT_FLAG_CONFIG, value: SlotValue::Int(4) },
            ],
        };
        tree.add(c);
        tree.execute_components();
        // 0.6 * 4 = 2.4, floor = 2
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Int(2));
    }

    // ---- Combined link + component execution ----

    #[test]
    fn links_then_components_end_to_end() {
        let mut tree = ComponentTree::new();
        // Two ConstFloat sources
        tree.add(make_source_comp(200, NO_PARENT, "a", 10.0));
        tree.add(make_source_comp(201, NO_PARENT, "b", 20.0));
        // Add2 component
        tree.add(make_math_comp(202, NO_PARENT, "add", 2, 3));
        // Wire: a.out -> add.in1, b.out -> add.in2
        tree.add_link(200, 1, 202, 2);
        tree.add_link(201, 1, 202, 3);

        // Step 1: propagate links
        let link_changed = tree.execute_links();
        assert!(link_changed.contains(&202));

        // Step 2: execute components
        let comp_changed = tree.execute_components();
        assert!(comp_changed.contains(&202));

        // Verify output: 10.0 + 20.0 = 30.0
        let add = tree.get(202).unwrap();
        match &add.slots[1].value {
            SlotValue::Float(v) => assert_eq!(*v, 30.0),
            other => panic!("expected Float(30.0), got {:?}", other),
        }
    }

    #[test]
    fn two_cycle_chain_propagation() {
        // src(5.0) -> mul.in1, const(3.0) -> mul.in2, mul.out -> add.in1, const2(7.0) -> add.in2
        // After cycle 1: mul computes 5*3=15, but add gets stale mul.out
        // After cycle 2: add gets 15 and computes 15+7=22
        let mut tree = ComponentTree::new();
        tree.add(make_source_comp(200, NO_PARENT, "src", 5.0));
        tree.add(make_source_comp(201, NO_PARENT, "c3", 3.0));
        tree.add(make_source_comp(203, NO_PARENT, "c7", 7.0));
        tree.add(make_math_comp(202, NO_PARENT, "mul", 2, 37)); // Mul2
        tree.add(make_math_comp(204, NO_PARENT, "add", 2, 3));  // Add2

        tree.add_link(200, 1, 202, 2); // src -> mul.in1
        tree.add_link(201, 1, 202, 3); // c3 -> mul.in2
        tree.add_link(202, 1, 204, 2); // mul.out -> add.in1
        tree.add_link(203, 1, 204, 3); // c7 -> add.in2

        // Cycle 1
        tree.execute_links();
        tree.execute_components();

        // Cycle 2
        tree.execute_links();
        tree.execute_components();

        // mul.out = 5*3 = 15
        let mul = tree.get(202).unwrap();
        match &mul.slots[1].value {
            SlotValue::Float(v) => assert_eq!(*v, 15.0),
            other => panic!("expected 15.0, got {:?}", other),
        }
        // add.out = 15 + 7 = 22
        let add = tree.get(204).unwrap();
        match &add.slots[1].value {
            SlotValue::Float(v) => assert_eq!(*v, 22.0),
            other => panic!("expected 22.0, got {:?}", other),
        }
    }
}
