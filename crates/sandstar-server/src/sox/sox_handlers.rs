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
use serde::{Deserialize, Serialize};

use super::sox_protocol::{SoxCmd, SoxReader, SoxRequest, SoxResponse, SoxValueType};

/// Batch of component slot updates: (comp_id, slot_updates, optional_state_update).
type ComponentUpdates = Vec<(u16, Vec<(usize, SlotValue)>, Option<ComponentState>)>;

// ---- Slot values ----

/// A slot value in the virtual component tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub from_comp: u16,
    pub from_slot: u8,
    pub to_comp: u16,
    pub to_slot: u8,
}

/// Why a link was not added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// The source or the target component does not exist.
    NotFound,
    /// A slot index is past the end of its component's slots.
    BadSlot,
    /// The same link is already there.
    Duplicate,
    /// The link would close a loop in the dataflow graph.
    Cycle,
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LinkError::NotFound => "link component not found",
            LinkError::BadSlot => "link slot out of range",
            LinkError::Duplicate => "link already exists",
            LinkError::Cycle => "link would create a cycle",
        })
    }
}

/// What a tree holds that should not be there (see
/// [`ComponentTree::integrity_report`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IntegrityReport {
    /// Components whose parent does not exist.
    pub orphans: usize,
    /// Links with a missing end or a slot index out of range.
    pub bad_links: usize,
}

// ---- Virtual component ----

/// A virtual Sedona component mapped from engine data.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualSlot {
    pub name: String,
    pub type_id: u8,
    pub flags: u8,
    pub value: SlotValue,
}

// ---- Component state (for stateful execution types) ----

/// Per-component execution state for stateful components (timers, counters, ramps).
///
/// Stored in a HashMap on ComponentTree, keyed by comp_id.
/// Initialized lazily on first execution tick.
#[derive(Debug, Clone, Default)]
pub struct ComponentState {
    /// Tick counter (used by DlyOn/DlyOff for delay counting).
    pub counter: i32,
    /// Direction flag (used by Ramp: 1=up, -1=down).
    pub direction: i32,
    /// Previous boolean input value (used by Count for edge detection).
    pub prev_bool: bool,
}

// ---- Component tree ----

/// No parent sentinel (root component).
pub const NO_PARENT: u16 = 0xFFFF;

/// Ids below this are the system components (app and services). They are
/// never deleted, neither by request nor as part of a subtree.
pub const FIRST_USER_COMP_ID: u16 = 7;

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
    /// Component IDs that were added by the editor (not from channel config).
    /// These are persisted to disk so they survive restarts.
    user_added_ids: HashSet<u16>,
    /// Whether the tree has unsaved changes to user-added components.
    dirty: bool,
    /// File path for persisting user-added components.
    /// Defaults to `sox_components.json` in the config directory.
    persist_path: Option<String>,
    /// Per-component execution state for stateful components (timers, counters, ramps).
    component_state: HashMap<u16, ComponentState>,
    /// Channel IDs that are output channels (direction = "out"/"AO"/"DO"/"do"/"ao").
    /// Populated at tree build time from ChannelInfo.direction.
    output_channel_ids: HashSet<i32>,
    /// Reverse index: component name → comp_id for O(1) name lookups.
    /// Updated on add, rename, and remove. Used by the channel bridge
    /// ("chXXXX" ConstFloat matching) to avoid O(n) scans.
    name_to_comp: HashMap<String, u16>,
    /// Name intern table for deduplication and fast ID-based comparison.
    pub name_table: super::name_intern::NameInternTable,
}

/// JSON persistence format for user-added SOX components.
#[derive(Debug, Serialize, Deserialize)]
pub struct PersistData {
    /// User-added components (not channel-mapped or system).
    pub components: Vec<VirtualComponent>,
    /// The set of component IDs that are user-added.
    pub user_added_ids: Vec<u16>,
    /// The next available component ID counter.
    pub next_comp_id: u16,
    /// Optional: interned name table for fast startup.
    /// Old JSON files without this field work fine (defaults to empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub name_table: Vec<String>,
}

impl ComponentTree {
    /// Create an empty tree (with no manifest database).
    pub fn new() -> Self {
        Self {
            components: HashMap::new(),
            next_id: 0,
            channel_comp_end: CHANNEL_COMP_BASE,
            manifest_db: Arc::new(ManifestDb::new()),
            user_added_ids: HashSet::new(),
            dirty: false,
            persist_path: None,
            component_state: HashMap::new(),
            output_channel_ids: HashSet::new(),
            name_to_comp: HashMap::new(),
            name_table: super::name_intern::NameInternTable::new(),
        }
    }

    /// Create an empty tree with a pre-loaded manifest database.
    pub fn new_with_manifest(manifest_db: Arc<ManifestDb>) -> Self {
        Self {
            components: HashMap::new(),
            next_id: 0,
            channel_comp_end: CHANNEL_COMP_BASE,
            output_channel_ids: HashSet::new(),
            manifest_db,
            user_added_ids: HashSet::new(),
            dirty: false,
            persist_path: None,
            component_state: HashMap::new(),
            name_to_comp: HashMap::new(),
            name_table: super::name_intern::NameInternTable::new(),
        }
    }

    /// Add a component to the tree. Also registers it as a child of its parent.
    ///
    /// The component's name is interned in the name table and indexed in the
    /// name-to-comp reverse map for O(1) lookups.
    pub fn add(&mut self, comp: VirtualComponent) {
        let comp_id = comp.comp_id;
        let parent_id = comp.parent_id;
        if comp_id >= self.next_id {
            self.next_id = comp_id + 1;
        }
        // Update name index and intern table.
        if !comp.name.is_empty() {
            self.name_to_comp.insert(comp.name.clone(), comp_id);
            self.intern_name(&comp.name);
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
        // Remove from name index (only if this comp still owns the name mapping).
        if let Some(&mapped_id) = self.name_to_comp.get(&comp.name) {
            if mapped_id == comp_id {
                self.name_to_comp.remove(&comp.name);
            }
        }
        Some(comp)
    }

    /// A component and everything below it, the component first. Empty if
    /// it does not exist.
    ///
    /// Only `parent_id` decides what is below: a `children` list can name
    /// a component that belongs elsewhere (a saved file from before reorder
    /// was checked), and following it would take that component along.
    /// System components are never a descendant. Each id is visited once,
    /// so a loop in the data ends.
    pub fn subtree_ids(&self, comp_id: u16) -> Vec<u16> {
        if !self.components.contains_key(&comp_id) {
            return Vec::new();
        }
        let mut by_parent: HashMap<u16, Vec<u16>> = HashMap::new();
        for comp in self.components.values() {
            if comp.parent_id != NO_PARENT && comp.parent_id != comp.comp_id {
                by_parent
                    .entry(comp.parent_id)
                    .or_default()
                    .push(comp.comp_id);
            }
        }
        let mut seen: HashSet<u16> = HashSet::from([comp_id]);
        let mut order = vec![comp_id];
        let mut next = 0;
        while let Some(&id) = order.get(next) {
            next += 1;
            for &child in by_parent.get(&id).map(Vec::as_slice).unwrap_or_default() {
                if child >= FIRST_USER_COMP_ID && seen.insert(child) {
                    order.push(child);
                }
            }
        }
        order
    }

    /// Remove a component together with everything below it. The links of
    /// every removed component go from the other end too, and the removed
    /// ids leave the user-added set. Returns the removed ids (the component
    /// first), or `None` if it does not exist.
    ///
    /// A child left behind would keep running and keep being saved while
    /// pointing at a parent that is gone (R-61).
    pub fn remove_subtree(&mut self, comp_id: u16) -> Option<Vec<u16>> {
        let ids = self.subtree_ids(comp_id);
        let top_parent = self.components.get(&comp_id)?.parent_id;
        let gone: HashSet<u16> = ids.iter().copied().collect();

        // One pass over what goes, then one `retain` per surviving peer:
        // the work grows with the size of the subtree, not with its square.
        let mut peers: HashSet<u16> = HashSet::new();
        for id in &ids {
            if let Some(comp) = self.components.remove(id) {
                for l in &comp.links {
                    for end in [l.from_comp, l.to_comp] {
                        if !gone.contains(&end) {
                            peers.insert(end);
                        }
                    }
                }
                if self.name_to_comp.get(&comp.name) == Some(id) {
                    self.name_to_comp.remove(&comp.name);
                }
            }
            self.user_added_ids.remove(id);
            self.component_state.remove(id);
        }
        for peer in peers {
            if let Some(comp) = self.components.get_mut(&peer) {
                comp.links
                    .retain(|l| !gone.contains(&l.from_comp) && !gone.contains(&l.to_comp));
            }
        }
        if let Some(parent) = self.components.get_mut(&top_parent) {
            parent.children.retain(|id| !gone.contains(id));
        }
        Some(ids)
    }

    /// Whether `slot` can be a slot of `comp`. A type the manifest knows
    /// has exactly its manifest slots. A type it does not know starts with
    /// `meta` only and gets its slots as they are written, so any index is
    /// possible there and none is refused.
    fn slot_can_exist(&self, comp: &VirtualComponent, slot: u8) -> bool {
        usize::from(slot) < comp.slots.len()
            || self
                .manifest_db
                .get_slots(comp.kit_id, comp.type_id)
                .is_none()
    }

    /// Count what should not be in the tree: components whose parent is
    /// missing, and links with a missing end or a slot its type does not
    /// have. A link is stored on both ends and counted once.
    pub fn integrity_report(&self) -> IntegrityReport {
        let orphans = self
            .components
            .values()
            .filter(|c| {
                c.parent_id != NO_PARENT
                    && c.parent_id != c.comp_id
                    && !self.components.contains_key(&c.parent_id)
            })
            .count();
        let end_ok = |id: u16, slot: u8| {
            self.components
                .get(&id)
                .is_some_and(|c| self.slot_can_exist(c, slot))
        };
        let mut bad: HashSet<(u16, u8, u16, u8)> = HashSet::new();
        for comp in self.components.values() {
            for l in &comp.links {
                let ok = end_ok(l.from_comp, l.from_slot) && end_ok(l.to_comp, l.to_slot);
                if !ok {
                    bad.insert((l.from_comp, l.from_slot, l.to_comp, l.to_slot));
                }
            }
        }
        IntegrityReport {
            orphans,
            bad_links: bad.len(),
        }
    }

    /// Names some component in the tree still carries.
    fn live_names(&self) -> HashSet<&str> {
        self.components
            .values()
            .map(|c| c.name.as_str())
            .filter(|n| !n.is_empty())
            .collect()
    }

    /// Intern a component name. Renames and removals leave old names in the
    /// table, so a full table first drops names no component carries (R-47);
    /// only a table full of names in use refuses the new one.
    fn intern_name(&self, name: &str) {
        if self.name_table.intern(name).is_valid() {
            return;
        }
        let live = self.live_names();
        self.name_table.retain(|n| live.contains(n));
        if !self.name_table.intern(name).is_valid() {
            self.name_table.warn_full_once();
        }
    }

    /// Rename a component. Returns true if found and renamed.
    ///
    /// Updates the name-to-comp reverse index and interns the new name.
    pub fn rename(&mut self, comp_id: u16, new_name: String) -> bool {
        if let Some(comp) = self.components.get_mut(&comp_id) {
            // Remove old name from index (only if this comp owns it).
            if let Some(&mapped_id) = self.name_to_comp.get(&comp.name) {
                if mapped_id == comp_id {
                    self.name_to_comp.remove(&comp.name);
                }
            }
            comp.name = new_name.clone();
            // Index and intern the new name.
            if !new_name.is_empty() {
                self.name_to_comp.insert(new_name.clone(), comp_id);
                self.intern_name(&new_name);
            }
            true
        } else {
            false
        }
    }

    /// Look up a component ID by name in O(1) via the reverse index.
    ///
    /// This is used by the channel bridge to find ConstFloat "chXXXX"
    /// components without scanning all components.
    pub fn lookup_by_name(&self, name: &str) -> Option<u16> {
        self.name_to_comp.get(name).copied()
    }

    /// Check if adding a link from `from_comp` to `to_comp` would create a cycle.
    /// Uses DFS from `to_comp` following existing links to see if we can reach `from_comp`.
    /// Self-loops (from_comp == to_comp) are allowed — the editor uses these.
    pub(crate) fn would_create_cycle(&self, from_comp: u16, to_comp: u16) -> bool {
        // Self-loops are allowed (editor uses them for internal wiring).
        if from_comp == to_comp {
            return false;
        }
        // DFS from to_comp: follow outgoing links (where to_comp is the source)
        // to see if we ever reach from_comp.
        let mut stack = vec![to_comp];
        let mut visited = std::collections::HashSet::new();
        let mut depth = 0u32;
        const MAX_DEPTH: u32 = 100;
        while let Some(current) = stack.pop() {
            if current == from_comp {
                return true;
            }
            if depth >= MAX_DEPTH {
                // Bail out on pathologically deep graphs — treat as cycle.
                return true;
            }
            if !visited.insert(current) {
                continue;
            }
            depth += 1;
            // Follow all outgoing links from `current` (where current is from_comp of the link).
            if let Some(comp) = self.components.get(&current) {
                for link in &comp.links {
                    if link.from_comp == current && !visited.contains(&link.to_comp) {
                        stack.push(link.to_comp);
                    }
                }
            }
        }
        false
    }

    /// Add a link to the tree. Returns true if the link was added (not a duplicate).
    pub fn add_link(&mut self, from_comp: u16, from_slot: u8, to_comp: u16, to_slot: u8) -> bool {
        self.try_add_link(from_comp, from_slot, to_comp, to_slot)
            .is_ok()
    }

    /// Add a link, or say why not. Both components must exist, and a slot
    /// index must be one the component's type has (see `slot_can_exist`); a
    /// link to nothing would stay in the tree and in the saved file without
    /// ever carrying a value.
    pub fn try_add_link(
        &mut self,
        from_comp: u16,
        from_slot: u8,
        to_comp: u16,
        to_slot: u8,
    ) -> Result<(), LinkError> {
        let link = Link {
            from_comp,
            from_slot,
            to_comp,
            to_slot,
        };
        let (Some(from), Some(to)) = (
            self.components.get(&from_comp),
            self.components.get(&to_comp),
        ) else {
            return Err(LinkError::NotFound);
        };
        if !self.slot_can_exist(from, from_slot) || !self.slot_can_exist(to, to_slot) {
            return Err(LinkError::BadSlot);
        }
        // The link is stored on BOTH components so the editor can render
        // wires from both the source and target sides.
        if to.links.contains(&link) {
            return Err(LinkError::Duplicate);
        }
        // Reject if the link would create a cycle in the dataflow graph.
        if self.would_create_cycle(from_comp, to_comp) {
            tracing::warn!(
                from_comp,
                to_comp,
                "SOX: link rejected — would create cycle"
            );
            return Err(LinkError::Cycle);
        }

        // DDC semantics: a single input slot has at most one incoming link.
        // If the editor wires a new source onto a target slot that already
        // has an incoming link from a different source, replace the old
        // link rather than letting both linger and have execute_links
        // pick a winner by insertion order. The editor's drag-drop UX
        // expects the latest wire to be authoritative.
        let preexisting: Option<(u16, u8)> = self.components.get(&to_comp).and_then(|c| {
            c.links
                .iter()
                .find(|l| l.to_comp == to_comp && l.to_slot == to_slot)
                .map(|l| (l.from_comp, l.from_slot))
        });
        if let Some((old_from_comp, old_from_slot)) = preexisting {
            tracing::info!(
                old_from = old_from_comp,
                old_slot = old_from_slot,
                new_from = from_comp,
                new_slot = from_slot,
                to_comp,
                to_slot,
                "SOX: replacing pre-existing incoming link on target slot"
            );
            self.remove_link(old_from_comp, old_from_slot, to_comp, to_slot);
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
        Ok(())
    }

    /// Remove a link from the tree. Returns true if the link was found and removed.
    pub fn remove_link(
        &mut self,
        from_comp: u16,
        from_slot: u8,
        to_comp: u16,
        to_slot: u8,
    ) -> bool {
        let link = Link {
            from_comp,
            from_slot,
            to_comp,
            to_slot,
        };
        let mut removed = false;
        // Remove from both components
        if let Some(comp) = self.components.get_mut(&to_comp) {
            let before = comp.links.len();
            comp.links.retain(|l| l != &link);
            if comp.links.len() < before {
                removed = true;
            }
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
            kit_id: 0,   // sys
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
            kit_id: 0,   // sys
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
            type_id: 1, // SoxService
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
            kit_id: 0,   // sys
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
            kit_id: 0,   // sys
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
            kit_id: 0,   // sys
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
            kit_id: 0,   // sys
            type_id: 11, // Folder
            children: Vec::new(),
            slots: Vec::new(),
            links: Vec::new(),
        });

        // Map each channel to a component under io (compId = 100 + index)
        // Auto-layout: grid of 6 columns, spaced 30 units apart
        for (i, ch) in channels.iter().enumerate() {
            let comp_id = CHANNEL_COMP_BASE + i as u16;
            let col = (i % 6) as u8;
            let row = (i / 6) as u8;
            let x = 2 + col * 30; // grid x position
            let y = 2 + row * 12; // grid y position
            tree.add(VirtualComponent {
                comp_id,
                parent_id: 5, // io folder
                name: format!("ch_{}", ch.id),
                type_name: channel_type_name(&ch.direction),
                kit_id: 1,  // EacIo kit (index 1 in DEFAULT_KITS)
                type_id: 0, // AnalogInput
                children: Vec::new(),
                slots: channel_slots_with_pos(ch, x, y),
                links: Vec::new(),
            });
        }

        // Record the upper bound of channel comp_ids
        tree.channel_comp_end = CHANNEL_COMP_BASE + channels.len() as u16;

        // Record which channel IDs are outputs so the bridge can detect them at runtime.
        for ch in channels {
            let d = ch.direction.to_lowercase();
            if d == "out" || d == "ao" || d == "do" {
                tree.output_channel_ids.insert(ch.id as i32);
            }
        }

        tree
    }

    /// Check if a comp_id is a channel-mapped component with a valid manifest schema.
    pub fn is_channel_comp(&self, comp_id: u16) -> bool {
        comp_id >= CHANNEL_COMP_BASE && comp_id < self.channel_comp_end
    }

    /// Check if a comp_id was added by the editor (user-added, not from channel config).
    pub fn is_user_added(&self, comp_id: u16) -> bool {
        self.user_added_ids.contains(&comp_id)
    }

    /// Mark a component as user-added (persisted across restarts).
    pub fn mark_user_added(&mut self, comp_id: u16) {
        self.user_added_ids.insert(comp_id);
        self.dirty = true;
    }

    /// Mark the tree as dirty (needs saving).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Check and clear the dirty flag. Returns true if the tree had unsaved changes.
    pub fn take_dirty(&mut self) -> bool {
        let was_dirty = self.dirty;
        self.dirty = false;
        was_dirty
    }

    /// Set the file path for persistence.
    pub fn set_persist_path(&mut self, path: String) {
        self.persist_path = Some(path);
    }

    /// Resolve the persistence file path.
    ///
    /// Priority: environment variable > explicit path > default Linux path > current dir fallback.
    pub fn resolve_persist_path(&self) -> String {
        if let Ok(env_path) = std::env::var("SANDSTAR_SOX_PERSIST_PATH") {
            return env_path;
        }
        if let Some(ref path) = self.persist_path {
            return path.clone();
        }
        // Default: Linux config dir, or current dir fallback
        let default_linux = "/home/eacio/sandstar/etc/config/sox_components.json";
        if Path::new("/home/eacio/sandstar/etc/config").exists() {
            default_linux.to_string()
        } else {
            "sox_components.json".to_string()
        }
    }

    /// Save user-added components to disk as JSON.
    ///
    /// Only components tracked in `user_added_ids` are saved.
    /// Channel-mapped and system components are excluded.
    pub fn save_user_components(&self) -> Result<(), String> {
        let path = self.resolve_persist_path();
        let user_comps: Vec<&VirtualComponent> = self
            .user_added_ids
            .iter()
            .filter_map(|id| self.components.get(id))
            .collect();

        // Also collect all links that involve user-added components
        // (already stored on the components themselves, so they serialize with the comp)

        // Serialize the name table for fast restore on startup. Only names a
        // component still carries are written: the table keeps every name
        // ever used (renames add, nothing removes), and persisting all of it
        // carried a full table across restarts (R-47).
        let live_names = self.live_names();
        let name_entries: Vec<String> = self
            .name_table
            .all_names()
            .into_iter()
            .map(|(_, name)| name)
            .filter(|name| live_names.contains(name.as_str()))
            .collect();

        let data = PersistData {
            components: user_comps.into_iter().cloned().collect(),
            user_added_ids: self.user_added_ids.iter().copied().collect(),
            next_comp_id: self.next_id,
            name_table: name_entries,
        };

        let json =
            serde_json::to_string_pretty(&data).map_err(|e| format!("serialize error: {e}"))?;

        // Write atomically: write to tmp file then rename
        let tmp_path = format!("{path}.tmp");
        std::fs::write(&tmp_path, &json).map_err(|e| format!("write error ({tmp_path}): {e}"))?;
        std::fs::rename(&tmp_path, &path)
            .map_err(|e| format!("rename error ({tmp_path} -> {path}): {e}"))?;

        tracing::info!(
            path,
            components = data.components.len(),
            "SOX: saved user components to disk"
        );
        Ok(())
    }

    /// Load user-added components from disk.
    ///
    /// Restores components, user_added_ids, and next_comp_id counter.
    /// If the file doesn't exist, this is a no-op (fresh start).
    pub fn load_user_components(&mut self) -> Result<usize, String> {
        let path = self.resolve_persist_path();
        let path_ref = Path::new(&path);
        if !path_ref.exists() {
            tracing::debug!(path, "SOX: no persistence file found, starting fresh");
            return Ok(0);
        }

        let json =
            std::fs::read_to_string(&path).map_err(|e| format!("read error ({path}): {e}"))?;

        let data: PersistData =
            serde_json::from_str(&json).map_err(|e| format!("deserialize error ({path}): {e}"))?;

        let count = data.components.len();

        // Restore interned name table entries (from persistence). A full
        // table is fine here: component names below go through intern_name,
        // which drops whatever the file listed but nothing uses.
        for name in &data.name_table {
            self.name_table.intern(name);
        }

        // Restore user_added_ids
        for &id in &data.user_added_ids {
            self.user_added_ids.insert(id);
        }

        // Restore components into the tree
        for comp in data.components {
            let comp_id = comp.comp_id;
            let parent_id = comp.parent_id;
            // Update name index and intern table.
            if !comp.name.is_empty() {
                self.name_to_comp.insert(comp.name.clone(), comp_id);
                self.intern_name(&comp.name);
            }
            // Insert the component
            self.components.insert(comp_id, comp);
            // Register as child of parent
            if parent_id != NO_PARENT && parent_id != comp_id {
                if let Some(parent) = self.components.get_mut(&parent_id) {
                    if !parent.children.contains(&comp_id) {
                        parent.children.push(comp_id);
                    }
                }
            }
        }

        // Restore next_comp_id (ensure it's at least as high as loaded value)
        if data.next_comp_id > self.next_id {
            self.next_id = data.next_comp_id;
        }
        // Also check against max existing comp_id
        if let Some(&max_id) = self.components.keys().max() {
            if max_id >= self.next_id {
                self.next_id = max_id + 1;
            }
        }

        tracing::info!(
            path,
            count,
            next_id = self.next_id,
            "SOX: loaded user components from disk"
        );
        // Report only: nothing is dropped from what the file held.
        let report = self.integrity_report();
        if report != IntegrityReport::default() {
            tracing::warn!(
                path,
                orphans = report.orphans,
                bad_links = report.bad_links,
                "SOX: saved components hold orphans or links to nothing"
            );
        }
        Ok(count)
    }

    /// Update slot values for channel-mapped components from fresh channel data.
    ///
    /// Returns the list of comp_ids that had value changes (for COV events).
    pub fn update_from_channels(&mut self, channels: &[ChannelInfo]) -> Vec<u16> {
        let mut changed = Vec::new();
        // Track output channel value changes so we can sync them to ConstFloat bridges.
        let mut output_ch_values: Vec<(i32, SlotValue)> = Vec::new();

        for (i, ch) in channels.iter().enumerate() {
            let comp_id = CHANNEL_COMP_BASE + i as u16;
            if let Some(comp) = self.components.get_mut(&comp_id) {
                let mut new_slots = channel_slots(ch);
                // Preserve existing meta value (contains canvas position coordinates)
                if let (Some(existing_meta), Some(new_meta)) =
                    (comp.slots.first(), new_slots.first_mut())
                {
                    if existing_meta.name == "meta" && new_meta.name == "meta" {
                        new_meta.value = existing_meta.value.clone();
                    }
                }
                if slots_differ(&comp.slots, &new_slots) {
                    // If this is an output channel, capture its new "out" value (slot 6)
                    // so we can propagate it to the linked ConstFloat bridge.
                    if self.output_channel_ids.contains(&(ch.id as i32)) {
                        if let Some(out_slot) = new_slots.get(6) {
                            output_ch_values.push((ch.id as i32, out_slot.value.clone()));
                        }
                    }
                    comp.slots = new_slots;
                    changed.push(comp_id);
                }
            }
        }

        // Sync output channel values → ConstFloat bridges ("chXXXX" components).
        // When the browser writes to an output channel via the engine, the ConstFloat
        // must pick up the new value so Sedona sees it and the bridge stays consistent.
        for (ch_id, value) in output_ch_values {
            let ch_name = format!("ch{ch_id}");
            // O(1) lookup via name_to_comp reverse index (replaces O(n) scan).
            let cf_id = self
                .name_to_comp
                .get(&ch_name)
                .copied()
                .filter(|id| self.user_added_ids.contains(id));
            if let Some(cf_id) = cf_id {
                if let Some(comp) = self.components.get_mut(&cf_id) {
                    if let Some(out_slot) = comp.slots.get_mut(1) {
                        // slot 1 = "out" for ConstFloat
                        if out_slot.value != value {
                            out_slot.value = value;
                            changed.push(cf_id);
                        }
                    }
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
                (2, 31) |
                // Stateful: DlyOff, DlyOn, Count, Ramp, Tstat, UpDn
                (2, 19) | (2, 20) | (2, 16) | (2, 44) | (2, 54) | (2, 55) |
                // ChannelRead: reads sensor value from a channel component
                (1, 100) => {
                    evaluations.push((comp.comp_id, comp.kit_id, comp.type_id));
                }
                _ => {}
            }
        }

        // Phase 2: compute outputs
        // state_update: optional new ComponentState to write back after computation
        let mut updates: ComponentUpdates = Vec::new();
        for (comp_id, _kit_id, type_id) in &evaluations {
            if let Some(comp) = self.components.get(comp_id) {
                let mut slot_updates: Vec<(usize, SlotValue)> = Vec::new();
                let mut state_update: Option<ComponentState> = None;

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
                        let clamped = if inv < low {
                            low
                        } else if inv > high {
                            high
                        } else {
                            inv
                        };
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
                        let new_out = if reset {
                            false
                        } else if set {
                            true
                        } else {
                            current
                        };
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
                        // WriteFloat: out = in (runtime passthrough).
                        // Look slots up by name — manifest order varies (the
                        // editor's manifest emits `meta=0, in=1, out=2`, the
                        // Sedona "stock" slot order is `meta=0, out=1, in=2`).
                        // Hardcoded indexes used to silently swap reads/writes
                        // and cancel link propagation in a per-tick loop.
                        let in_idx = slot_index_by_name(&comp.slots, "in").unwrap_or(2);
                        let out_idx = slot_index_by_name(&comp.slots, "out").unwrap_or(1);
                        let inv = get_float(&comp.slots, in_idx);
                        slot_updates.push((out_idx, SlotValue::Float(inv)));
                    }

                    // --- WriteBool passthrough ---
                    56 => {
                        // WriteBool: out = in (see WriteFloat note above).
                        let in_idx = slot_index_by_name(&comp.slots, "in").unwrap_or(2);
                        let out_idx = slot_index_by_name(&comp.slots, "out").unwrap_or(1);
                        let inv = get_bool(&comp.slots, in_idx);
                        slot_updates.push((out_idx, SlotValue::Bool(inv)));
                    }

                    // --- WriteInt passthrough ---
                    58 => {
                        // WriteInt: out = in (see WriteFloat note above).
                        let in_idx = slot_index_by_name(&comp.slots, "in").unwrap_or(2);
                        let out_idx = slot_index_by_name(&comp.slots, "out").unwrap_or(1);
                        let inv = get_int(&comp.slots, in_idx);
                        slot_updates.push((out_idx, SlotValue::Int(inv)));
                    }

                    // --- ChannelRead (kit 1, type 100): reads sensor value from channel ---
                    // Handled separately below (needs cross-component lookup)
                    _ if comp.kit_id == 1 && *type_id == 100 => {}

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

                    // --- DlyOn (Delay-on timer, type 20) ---
                    20 => {
                        // slots: meta=0, out=1(bool), in=2(bool), delay=3(float, seconds)
                        // When in=true, count ticks. After delay seconds (1 tick/s), set out=true.
                        // When in=false, reset counter, out=false immediately.
                        let input = get_bool(&comp.slots, 2);
                        let delay = get_float(&comp.slots, 3);
                        let mut st = self
                            .component_state
                            .get(comp_id)
                            .cloned()
                            .unwrap_or_default();
                        if input {
                            st.counter += 1;
                            let out = st.counter >= delay as i32;
                            slot_updates.push((1, SlotValue::Bool(out)));
                        } else {
                            st.counter = 0;
                            slot_updates.push((1, SlotValue::Bool(false)));
                        }
                        state_update = Some(st);
                    }

                    // --- DlyOff (Delay-off timer, type 19) ---
                    19 => {
                        // slots: meta=0, out=1(bool), in=2(bool), delay=3(float, seconds)
                        // When in=false, count ticks. After delay seconds, set out=false.
                        // When in=true, reset counter, out=true immediately.
                        let input = get_bool(&comp.slots, 2);
                        let delay = get_float(&comp.slots, 3);
                        let mut st = self
                            .component_state
                            .get(comp_id)
                            .cloned()
                            .unwrap_or_default();
                        if !input {
                            st.counter += 1;
                            let out = st.counter < delay as i32;
                            slot_updates.push((1, SlotValue::Bool(out)));
                        } else {
                            st.counter = 0;
                            slot_updates.push((1, SlotValue::Bool(true)));
                        }
                        state_update = Some(st);
                    }

                    // --- Count (Edge-triggered counter, type 16) ---
                    16 => {
                        // slots: meta=0, out=1(int), in=2(bool), preset=3(int, config)
                        // Count rising edges of in. If preset>0, reset to 0 at preset.
                        let input = get_bool(&comp.slots, 2);
                        let preset = get_int(&comp.slots, 3);
                        let mut st = self
                            .component_state
                            .get(comp_id)
                            .cloned()
                            .unwrap_or_default();
                        let current_out = get_int(&comp.slots, 1);
                        let mut new_out = current_out;
                        // Detect rising edge: prev=false, current=true
                        if input && !st.prev_bool {
                            new_out += 1;
                            if preset > 0 && new_out >= preset {
                                new_out = 0;
                            }
                        }
                        st.prev_bool = input;
                        slot_updates.push((1, SlotValue::Int(new_out)));
                        state_update = Some(st);
                    }

                    // --- Ramp (Triangle wave, type 44) ---
                    44 => {
                        // slots: meta=0, out=1(float), min=2(float), max=3(float), step=4(float)
                        // Each tick, increment/decrement out by step. Reverse at boundaries.
                        let min_val = get_float(&comp.slots, 2);
                        let max_val = get_float(&comp.slots, 3);
                        let step = get_float(&comp.slots, 4);
                        let current_out = get_float(&comp.slots, 1);
                        let mut st = self
                            .component_state
                            .get(comp_id)
                            .cloned()
                            .unwrap_or_default();
                        // Initialize direction if not set (0 means uninitialized)
                        if st.direction == 0 {
                            st.direction = 1; // start going up
                        }
                        let mut new_out = current_out + step * st.direction as f32;
                        if new_out >= max_val {
                            new_out = max_val;
                            st.direction = -1;
                        } else if new_out <= min_val {
                            new_out = min_val;
                            st.direction = 1;
                        }
                        slot_updates.push((1, SlotValue::Float(new_out)));
                        state_update = Some(st);
                    }

                    // --- Tstat (Thermostat with deadband, type 54) ---
                    54 => {
                        // Real manifest slots:
                        //   meta=0, diff=1(float,config=deadband), isHeating=2(bool,config),
                        //   sp=3(float,config), cv=4(float,runtime=process variable),
                        //   out=5(bool,readonly), raise=6(bool,readonly), lower=7(bool,readonly)
                        let diff = get_float(&comp.slots, 1); // deadband
                        let is_heating = get_bool(&comp.slots, 2); // heating mode
                        let sp = get_float(&comp.slots, 3); // setpoint
                        let cv = get_float(&comp.slots, 4); // current value (PV)
                        let current_out = get_bool(&comp.slots, 5);
                        let half_db = diff / 2.0;

                        let (out, raise, lower) = if is_heating {
                            // Heating mode: turn on when cold, off when warm
                            if cv < sp - half_db {
                                (true, true, false) // need heat
                            } else if cv > sp + half_db {
                                (false, false, true) // warm enough
                            } else {
                                (current_out, false, false) // deadband
                            }
                        } else {
                            // Cooling mode: turn on when hot, off when cool
                            if cv > sp + half_db {
                                (true, false, true) // need cooling
                            } else if cv < sp - half_db {
                                (false, true, false) // cool enough
                            } else {
                                (current_out, false, false) // deadband
                            }
                        };
                        slot_updates.push((5, SlotValue::Bool(out)));
                        slot_updates.push((6, SlotValue::Bool(raise)));
                        slot_updates.push((7, SlotValue::Bool(lower)));
                    }

                    // --- UpDn (Up/down accumulator, type 55) ---
                    55 => {
                        // slots: meta=0, out=1(float), up=2(bool), dn=3(bool),
                        //        step=4(float, config), min=5(float, config), max=6(float, config)
                        let up = get_bool(&comp.slots, 2);
                        let dn = get_bool(&comp.slots, 3);
                        let step = get_float(&comp.slots, 4);
                        let min_val = get_float(&comp.slots, 5);
                        let max_val = get_float(&comp.slots, 6);
                        let mut out = get_float(&comp.slots, 1);
                        if up {
                            out += step;
                            if out > max_val {
                                out = max_val;
                            }
                        }
                        if dn {
                            out -= step;
                            if out < min_val {
                                out = min_val;
                            }
                        }
                        slot_updates.push((1, SlotValue::Float(out)));
                    }

                    _ => {}
                }
                if !slot_updates.is_empty() || state_update.is_some() {
                    updates.push((*comp_id, slot_updates, state_update));
                }
            }
        }

        // Phase 3: apply updates, track changes
        let mut changed: Vec<u16> = Vec::new();
        for (comp_id, slot_updates, state_update) in updates {
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
            // Apply component state updates (timers, counters, ramp direction)
            if let Some(st) = state_update {
                self.component_state.insert(comp_id, st);
            }
        }

        // Phase 4: Sensor bridge — read sensor value from channel components.
        // Supports two mechanisms:
        // A) ChannelRead (kit 1, type 100): channelId in slot 1
        // B) Any ConstFloat/WriteFloat whose "out" slot (index 1) has a channel link:
        //    if the component NAME is "ch_XXXX" (e.g., "ch_1713"), auto-pull from channel XXXX
        // Collect channel bridge info: (user_comp_id, channel_id, out_slot_idx, is_output_channel)
        let mut channel_bridges: Vec<(u16, i32, usize)> = Vec::new();
        let mut channel_writes: Vec<(u16, i32, usize)> = Vec::new(); // for output channels

        for comp in self.components.values() {
            if comp.kit_id == 1 && comp.type_id == 100 {
                let ch_id = get_int(&comp.slots, 1);
                if ch_id > 0 {
                    channel_bridges.push((comp.comp_id, ch_id, 2));
                }
            } else if self.user_added_ids.contains(&comp.comp_id) && comp.name.starts_with("ch") {
                let num_str = comp.name.trim_start_matches("ch").trim_start_matches('_');
                if let Ok(ch_id) = num_str.parse::<i32>() {
                    if ch_id > 0 {
                        // Check if this is an output channel using the direction set
                        // populated at tree build time from ChannelInfo.direction.
                        let is_output_ch = self.output_channel_ids.contains(&ch_id);

                        if is_output_ch {
                            // Output channel: write FROM ConstFloat TO channel
                            channel_writes.push((comp.comp_id, ch_id, 1));
                        } else {
                            // Input channel: read FROM channel TO ConstFloat
                            channel_bridges.push((comp.comp_id, ch_id, 1));
                        }
                    }
                }
            }
        }

        // INPUT channels: copy sensor value → ConstFloat
        for (comp_id, channel_id, out_slot_idx) in channel_bridges {
            let sensor_value = self
                .components
                .values()
                .find(|c| {
                    self.is_channel_comp(c.comp_id)
                        && c.slots
                            .get(2)
                            .map(|s| s.value == SlotValue::Int(channel_id))
                            .unwrap_or(false)
                })
                .and_then(|c| c.slots.get(6))
                .map(|s| s.value.clone());

            if let Some(value) = sensor_value {
                if let Some(comp) = self.components.get_mut(&comp_id) {
                    if let Some(out_slot) = comp.slots.get_mut(out_slot_idx) {
                        if out_slot.value != value {
                            out_slot.value = value;
                            changed.push(comp_id);
                        }
                    }
                }
            }
        }

        // OUTPUT channels: copy ConstFloat value → channel component's out slot
        // (The actual hardware write happens in collect_channel_writes via the engine)
        for (comp_id, channel_id, slot_idx) in channel_writes {
            let user_value = self
                .components
                .get(&comp_id)
                .and_then(|c| c.slots.get(slot_idx))
                .map(|s| s.value.clone());

            if let Some(value) = user_value {
                // Find the channel component and update its out slot (6)
                let ch_comp_id = self
                    .components
                    .values()
                    .find(|c| {
                        self.is_channel_comp(c.comp_id)
                            && c.slots
                                .get(2)
                                .map(|s| s.value == SlotValue::Int(channel_id))
                                .unwrap_or(false)
                    })
                    .map(|c| c.comp_id);

                if let Some(cid) = ch_comp_id {
                    if let Some(comp) = self.components.get_mut(&cid) {
                        if let Some(out_slot) = comp.slots.get_mut(6) {
                            if out_slot.value != value {
                                out_slot.value = value;
                                changed.push(cid);
                            }
                        }
                    }
                }
            }
        }

        changed
    }

    /// Collect channel write commands produced by logic→channel dataflow.
    ///
    /// After `execute_links` and `execute_components`, some link targets may be
    /// channel components (comp_id in [CHANNEL_COMP_BASE, channel_comp_end)).
    /// When a logic component output is wired to a channel component's writable
    /// slot (slot 6 = "out"), the value should be forwarded to the engine as a
    /// channel write so it reaches real hardware.
    ///
    /// `changed_ids` is the union of comp_ids changed by execute_links and
    /// execute_components.
    ///
    /// Returns a list of (channel_id, value) pairs to send to the engine.
    pub fn collect_channel_writes(&self, changed_ids: &[u16]) -> Vec<(u32, f64)> {
        let mut writes: Vec<(u32, f64)> = Vec::new();
        for &comp_id in changed_ids {
            if !self.is_channel_comp(comp_id) {
                continue;
            }
            let comp = match self.components.get(&comp_id) {
                Some(c) => c,
                None => continue,
            };
            // Extract the engine channel_id from slot 2 ("channel").
            let channel_id = match comp.slots.get(2) {
                Some(slot) => match &slot.value {
                    SlotValue::Int(id) => *id as u32,
                    _ => continue,
                },
                None => continue,
            };
            // Allow forwarding if:
            // A) A link targets slot 6 (wire from logic component), OR
            // B) This is an output channel (bridge from ConstFloat "chXXXX")
            let has_link_to_out = comp
                .links
                .iter()
                .any(|link| link.to_comp == comp_id && link.to_slot == 6);
            let is_output = self.output_channel_ids.contains(&(channel_id as i32));
            if !has_link_to_out && !is_output {
                continue;
            }
            // Read the current "out" slot value (slot 6).
            let value = match comp.slots.get(6) {
                Some(slot) => match &slot.value {
                    SlotValue::Float(v) => *v as f64,
                    SlotValue::Double(v) => *v,
                    SlotValue::Int(v) => *v as f64,
                    _ => continue,
                },
                None => continue,
            };
            writes.push((channel_id, value));
        }
        writes
    }
}

/// Extract a float value from a slot, coercing Int to f32 if needed.
/// Find a slot's index by its name. Use this instead of hardcoded
/// indexes for components whose `executable` Rust handler talks to
/// slots by semantic role (`in`, `out`, `in1`, ...). The manifest the
/// editor consumes can emit slots in a different order than the
/// historical Sedona "stock" ordering, so an integer index is unsafe.
fn slot_index_by_name(slots: &[VirtualSlot], name: &str) -> Option<usize> {
    slots.iter().position(|s| s.name == name)
}

fn get_float(slots: &[VirtualSlot], idx: usize) -> f32 {
    slots
        .get(idx)
        .and_then(|s| match &s.value {
            SlotValue::Float(v) => Some(*v),
            SlotValue::Int(v) => Some(*v as f32),
            _ => None,
        })
        .unwrap_or(0.0)
}

/// Extract a bool value from a slot.
fn get_bool(slots: &[VirtualSlot], idx: usize) -> bool {
    slots
        .get(idx)
        .and_then(|s| match &s.value {
            SlotValue::Bool(v) => Some(*v),
            _ => None,
        })
        .unwrap_or(false)
}

/// Extract an int value from a slot, coercing Float to i32 if needed.
fn get_int(slots: &[VirtualSlot], idx: usize) -> i32 {
    slots
        .get(idx)
        .and_then(|s| match &s.value {
            SlotValue::Int(v) => Some(*v),
            SlotValue::Float(v) => Some(*v as i32),
            _ => None,
        })
        .unwrap_or(0)
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
/// Encode canvas position into meta slot value.
/// Meta bit layout: bits 0-3 = security, bits 16-23 = x, bits 24-31 = y
fn encode_meta_with_pos(x: u8, y: u8) -> i32 {
    0x01 | ((x as i32) << 16) | ((y as i32) << 24)
}

fn channel_slots_with_pos(ch: &ChannelInfo, x: u8, y: u8) -> Vec<VirtualSlot> {
    let mut slots = channel_slots(ch);
    // Set canvas position in meta slot
    if let Some(meta) = slots.first_mut() {
        meta.value = SlotValue::Int(encode_meta_with_pos(x, y));
    }
    slots
}

fn channel_slots(ch: &ChannelInfo) -> Vec<VirtualSlot> {
    vec![
        // Inherited from sys::Component
        VirtualSlot {
            name: "meta".into(),
            type_id: SoxValueType::Int as u8,
            flags: SLOT_FLAG_CONFIG,
            value: SlotValue::Int(1), // default meta value (no position)
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
        t if t == SoxValueType::Double as u8 => reader.read_f64().map(SlotValue::Double),
        t if t == SoxValueType::Buf as u8 => {
            let len = reader.read_u16()? as usize;
            let bytes = reader.read_bytes(len)?;
            Some(SlotValue::Buf(bytes.to_vec()))
        }
        // Treat Byte/Short as Int for simplicity
        t if t == SoxValueType::Byte as u8 => reader.read_u8().map(|v| SlotValue::Int(v as i32)),
        t if t == SoxValueType::Short as u8 => reader.read_u16().map(|v| SlotValue::Int(v as i32)),
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
    KitInfo {
        name: "sys",
        checksum: 0xd3984c51,
        version: "1.2.28",
    },
    KitInfo {
        name: "EacIo",
        checksum: 0x6f9da65b,
        version: "1.2.30",
    },
    KitInfo {
        name: "control",
        checksum: 0x808b7db3,
        version: "1.2.28",
    },
    KitInfo {
        name: "driver",
        checksum: 0xb4cc82ce,
        version: "1.2.28",
    },
    KitInfo {
        name: "func",
        checksum: 0x821b7396,
        version: "1.2.28",
    },
    KitInfo {
        name: "hvac",
        checksum: 0x7264c67c,
        version: "1.2.28",
    },
    KitInfo {
        name: "inet",
        checksum: 0x25648ba7,
        version: "1.2.28",
    },
    KitInfo {
        name: "logic",
        checksum: 0x9fe95ce1,
        version: "1.2.28",
    },
    KitInfo {
        name: "math",
        checksum: 0xc22b255c,
        version: "1.2.28",
    },
    KitInfo {
        name: "platUnix",
        checksum: 0x751711ab,
        version: "1.2.28",
    },
    KitInfo {
        name: "pricomp",
        checksum: 0xb5cd6698,
        version: "1.2.28",
    },
    KitInfo {
        name: "shaystack",
        checksum: 0xedf7a27c,
        version: "1.2",
    },
    KitInfo {
        name: "sox",
        checksum: 0x397a84dd,
        version: "1.2.28",
    },
    KitInfo {
        name: "types",
        checksum: 0x10936551,
        version: "1.2.28",
    },
    KitInfo {
        name: "web",
        checksum: 0x0d0dd007,
        version: "1.2.29",
    },
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
    /// (kit_index, type_name) -> type_id for resolving base type inheritance.
    type_name_lookup: HashMap<(u8, String), u8>,
}

impl ManifestDb {
    /// Create an empty manifest database.
    pub fn new() -> Self {
        Self {
            types: HashMap::new(),
            type_name_lookup: HashMap::new(),
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
                tracing::debug!(
                    kit = kit.name,
                    "manifest XML not found, using hardcoded fallback"
                );
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

        // Register custom ChannelRead type (EacIo kit, type 100)
        // This allows users to bridge real sensor values into control logic
        db.types.insert(
            (1, 100),
            vec![
                ManifestSlot {
                    name: "meta".into(),
                    type_id: SoxValueType::Int as u8,
                    flags: SLOT_FLAG_CONFIG,
                    default_value: SlotValue::Int(1),
                },
                ManifestSlot {
                    name: "channelId".into(),
                    type_id: SoxValueType::Int as u8,
                    flags: SLOT_FLAG_CONFIG,
                    default_value: SlotValue::Int(0),
                },
                ManifestSlot {
                    name: "out".into(),
                    type_id: SoxValueType::Float as u8,
                    flags: SLOT_FLAG_RUNTIME,
                    default_value: SlotValue::Float(0.0),
                },
                ManifestSlot {
                    name: "status".into(),
                    type_id: SoxValueType::Buf as u8,
                    flags: SLOT_FLAG_RUNTIME,
                    default_value: SlotValue::Str(String::new()),
                },
            ],
        );
        db.type_name_lookup
            .insert((1, "ChannelRead".to_string()), 100);

        // Register hardcoded control types so the palette works even without manifest XMLs.
        // Only insert if not already loaded from a manifest file (manifests take priority).
        let hardcoded_types: Vec<(u8, u8, &str, Vec<ManifestSlot>)> = vec![
            (
                2,
                14,
                "ConstFloat",
                vec![
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
                    ManifestSlot {
                        name: "set".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_ACTION,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "setNull".into(),
                        type_id: SoxValueType::Void as u8,
                        flags: SLOT_FLAG_ACTION,
                        default_value: SlotValue::Null,
                    },
                ],
            ),
            (
                2,
                15,
                "ConstInt",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(0),
                    },
                    ManifestSlot {
                        name: "set".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_ACTION,
                        default_value: SlotValue::Int(0),
                    },
                ],
            ),
            (
                2,
                13,
                "ConstBool",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "setTrue".into(),
                        type_id: SoxValueType::Void as u8,
                        flags: SLOT_FLAG_ACTION,
                        default_value: SlotValue::Null,
                    },
                    ManifestSlot {
                        name: "setFalse".into(),
                        type_id: SoxValueType::Void as u8,
                        flags: SLOT_FLAG_ACTION,
                        default_value: SlotValue::Null,
                    },
                    ManifestSlot {
                        name: "setNull".into(),
                        type_id: SoxValueType::Void as u8,
                        flags: SLOT_FLAG_ACTION,
                        default_value: SlotValue::Null,
                    },
                ],
            ),
            (
                2,
                3,
                "Add2",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                ],
            ),
            (
                2,
                49,
                "Sub2",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                ],
            ),
            (
                2,
                37,
                "Mul2",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                ],
            ),
            (
                2,
                18,
                "Div2",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "div0".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                ],
            ),
            (
                2,
                56,
                "WriteBool",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "setTrue".into(),
                        type_id: SoxValueType::Void as u8,
                        flags: SLOT_FLAG_ACTION,
                        default_value: SlotValue::Null,
                    },
                    ManifestSlot {
                        name: "setFalse".into(),
                        type_id: SoxValueType::Void as u8,
                        flags: SLOT_FLAG_ACTION,
                        default_value: SlotValue::Null,
                    },
                    ManifestSlot {
                        name: "setNull".into(),
                        type_id: SoxValueType::Void as u8,
                        flags: SLOT_FLAG_ACTION,
                        default_value: SlotValue::Null,
                    },
                ],
            ),
            (
                2,
                57,
                "WriteFloat",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "set".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_ACTION,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "setNull".into(),
                        type_id: SoxValueType::Void as u8,
                        flags: SLOT_FLAG_ACTION,
                        default_value: SlotValue::Null,
                    },
                ],
            ),
            (
                2,
                58,
                "WriteInt",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(0),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Int(0),
                    },
                    ManifestSlot {
                        name: "set".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_ACTION,
                        default_value: SlotValue::Int(0),
                    },
                ],
            ),
            // --- 4-input Arithmetic ---
            (
                2,
                4,
                "Add4",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in3".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in4".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                ],
            ),
            (
                2,
                50,
                "Sub4",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in3".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in4".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                ],
            ),
            (
                2,
                38,
                "Mul4",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in3".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in4".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                ],
            ),
            // --- Unary Math ---
            (
                2,
                39,
                "Neg",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                ],
            ),
            (
                2,
                23,
                "FloatOffset",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "offset".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(0.0),
                    },
                ],
            ),
            (
                2,
                34,
                "Max",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                ],
            ),
            (
                2,
                35,
                "Min",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                ],
            ),
            (
                2,
                32,
                "Limiter",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "lowLmt".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "highLmt".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(100.0),
                    },
                ],
            ),
            (
                2,
                47,
                "Round",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "decimalPlaces".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(2),
                    },
                ],
            ),
            // --- Boolean Logic ---
            (
                2,
                5,
                "And2",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                ],
            ),
            (
                2,
                6,
                "And4",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in3".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in4".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                ],
            ),
            (
                2,
                42,
                "Or2",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                ],
            ),
            (
                2,
                43,
                "Or4",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in3".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in4".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                ],
            ),
            (
                2,
                40,
                "Not",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                ],
            ),
            (
                2,
                59,
                "Xor",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                ],
            ),
            // --- Comparator ---
            (
                2,
                12,
                "Cmpr",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "xgy".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "xey".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "xly".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "x".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "y".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                ],
            ),
            // --- Type Conversion ---
            (
                2,
                10,
                "B2P",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                ],
            ),
            (
                2,
                22,
                "F2I",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Int(0),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                ],
            ),
            (
                2,
                26,
                "I2F",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Int(0),
                    },
                ],
            ),
            // --- Multiplexers / Switches ---
            (
                2,
                1,
                "ASW",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "sel".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                ],
            ),
            (
                2,
                11,
                "BSW",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "sel".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                ],
            ),
            (
                2,
                28,
                "ISW",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Int(0),
                    },
                    ManifestSlot {
                        name: "sel".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in1".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Int(0),
                    },
                    ManifestSlot {
                        name: "in2".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Int(0),
                    },
                ],
            ),
            // --- Hysteresis / Latches ---
            (
                2,
                25,
                "Hysteresis",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "rising".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(75.0),
                    },
                    ManifestSlot {
                        name: "falling".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(70.0),
                    },
                ],
            ),
            (
                2,
                48,
                "SRLatch",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "set".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "reset".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                ],
            ),
            (
                2,
                46,
                "Reset",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "inLow".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "inHigh".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(100.0),
                    },
                    ManifestSlot {
                        name: "outLow".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "outHigh".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(100.0),
                    },
                ],
            ),
            // --- Sequencer ---
            (
                2,
                31,
                "LSeq",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Int(0),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "numStages".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(4),
                    },
                    ManifestSlot {
                        name: "rampTime".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(0.0),
                    },
                ],
            ),
            // --- Stateful: Timers ---
            (
                2,
                20,
                "DlyOn",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "delay".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(5.0),
                    },
                ],
            ),
            (
                2,
                19,
                "DlyOff",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "delay".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(5.0),
                    },
                ],
            ),
            (
                2,
                16,
                "Count",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Int(0),
                    },
                    ManifestSlot {
                        name: "in".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "preset".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(0),
                    },
                ],
            ),
            (
                2,
                44,
                "Ramp",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "min".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "max".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(100.0),
                    },
                    ManifestSlot {
                        name: "step".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(1.0),
                    },
                ],
            ),
            // --- Tstat (Thermostat with deadband) ---
            (
                2,
                54,
                "Tstat",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "diff".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(2.0),
                    },
                    ManifestSlot {
                        name: "isHeating".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Bool(true),
                    },
                    ManifestSlot {
                        name: "sp".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(72.0),
                    },
                    ManifestSlot {
                        name: "cv".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "raise".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "lower".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                ],
            ),
            // --- UpDn (Up/down accumulator) ---
            (
                2,
                55,
                "UpDn",
                vec![
                    ManifestSlot {
                        name: "meta".into(),
                        type_id: SoxValueType::Int as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Int(1),
                    },
                    ManifestSlot {
                        name: "out".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "up".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "dn".into(),
                        type_id: SoxValueType::Bool as u8,
                        flags: SLOT_FLAG_RUNTIME,
                        default_value: SlotValue::Bool(false),
                    },
                    ManifestSlot {
                        name: "step".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(1.0),
                    },
                    ManifestSlot {
                        name: "min".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(0.0),
                    },
                    ManifestSlot {
                        name: "max".into(),
                        type_id: SoxValueType::Float as u8,
                        flags: SLOT_FLAG_CONFIG,
                        default_value: SlotValue::Float(100.0),
                    },
                ],
            ),
        ];
        for (kit_id, type_id, type_name, slots) in hardcoded_types {
            if let std::collections::hash_map::Entry::Vacant(e) = db.types.entry((kit_id, type_id))
            {
                e.insert(slots);
                db.type_name_lookup
                    .insert((kit_id, type_name.to_string()), type_id);
            }
        }

        tracing::info!(total_types = db.types.len(), "ManifestDb loaded");
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
                        let mut type_name = String::new();

                        for attr in e.attributes().flatten() {
                            let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                            let val = std::str::from_utf8(&attr.value).unwrap_or("");
                            match key {
                                "id" => id = val.parse().ok(),
                                "base" => base = Some(val.to_string()),
                                "name" => type_name = val.to_string(),
                                _ => {}
                            }
                        }

                        if let Some(type_id) = id {
                            // Register name → type_id for base type resolution
                            if !type_name.is_empty() {
                                self.type_name_lookup
                                    .insert((kit_index, type_name), type_id);
                            }
                            // Flush previous type if any
                            if let Some(prev_type_id) = current_type_id.take() {
                                self.insert_type(
                                    kit_index,
                                    prev_type_id,
                                    &current_base,
                                    &current_slots,
                                );
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

        let has_base = base.as_ref().is_some_and(|b| !b.is_empty());
        if has_base {
            // Resolve parent type slots by looking up the base type in the manifest DB.
            // e.g., Add4 base="control::Add2" → inherit Add2's slots (meta, out, in1, in2)
            if let Some(base_str) = base.as_ref() {
                if let Some(parent_slots) = self.resolve_base_slots(base_str, kit_index) {
                    slots.extend(parent_slots);
                } else {
                    // Fallback: just prepend meta if parent not found
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
            }
        }

        // Append own slots, skipping any that were already inherited from parent
        for slot in own_slots {
            if !slots.iter().any(|s| s.name == slot.name) {
                slots.push(slot.clone());
            }
        }
        self.types.insert((kit_index, type_id), slots);
    }

    /// Resolve base type slots from a "kit::Type" string like "control::Add2".
    fn resolve_base_slots(&self, base: &str, _current_kit: u8) -> Option<Vec<ManifestSlot>> {
        let parts: Vec<&str> = base.split("::").collect();
        if parts.len() != 2 {
            return None;
        }
        let kit_name = parts[0];
        let type_name = parts[1];

        let kit_idx = DEFAULT_KITS.iter().position(|k| k.name == kit_name)? as u8;
        let tid = self
            .type_name_lookup
            .get(&(kit_idx, type_name.to_string()))?;
        self.types.get(&(kit_idx, *tid)).cloned()
    }

    /// Look up the slot schema for a component type.
    ///
    /// Returns `None` if no manifest was loaded for this (kit_id, type_id).
    /// Make a type known to the manifest, with no slot details (tests).
    #[cfg(test)]
    pub(crate) fn know_type_for_test(&mut self, kit_id: u8, type_id: u8) {
        self.types.entry((kit_id, type_id)).or_default();
    }

    pub fn get_slots(&self, kit_id: u8, type_id: u8) -> Option<&Vec<ManifestSlot>> {
        self.types.get(&(kit_id, type_id))
    }

    /// Get the total number of types in the database.
    pub fn type_count(&self) -> usize {
        self.types.len()
    }

    /// Iterate over all (kit_id, type_id) -> slots entries.
    pub fn all_types(&self) -> impl Iterator<Item = (&(u8, u8), &Vec<ManifestSlot>)> {
        self.types.iter()
    }

    /// Look up a type name by (kit_index, type_id).
    ///
    /// Reverse lookup through `type_name_lookup`.
    pub fn type_name(&self, kit_id: u8, type_id: u8) -> Option<String> {
        for ((k, name), &tid) in &self.type_name_lookup {
            if *k == kit_id && tid == type_id {
                return Some(name.clone());
            }
        }
        None
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
        Some("") => {
            // Empty default: for Buf/Str types means empty string, otherwise zero
            if type_id == SoxValueType::Buf as u8 {
                SlotValue::Str(String::new())
            } else {
                default_for_type(type_id)
            }
        }
        Some(s) => match type_id {
            t if t == SoxValueType::Bool as u8 => SlotValue::Bool(s == "true" || s == "1"),
            t if t == SoxValueType::Int as u8 => SlotValue::Int(s.parse().unwrap_or(0)),
            t if t == SoxValueType::Float as u8 => SlotValue::Float(s.parse().unwrap_or(0.0)),
            t if t == SoxValueType::Double as u8 => SlotValue::Double(s.parse().unwrap_or(0.0)),
            t if t == SoxValueType::Long as u8 => SlotValue::Long(s.parse().unwrap_or(0)),
            t if t == SoxValueType::Byte as u8 || t == SoxValueType::Short as u8 => {
                SlotValue::Int(s.parse().unwrap_or(0))
            }
            t if t == SoxValueType::Buf as u8 => SlotValue::Str(s.to_string()),
            t if t == SoxValueType::Void as u8 => SlotValue::Null,
            _ => default_for_type(type_id),
        },
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

    /// Drop every subscription to a component that no longer exists, so a
    /// later component that gets the same id does not inherit its watchers.
    pub fn forget_component(&mut self, comp_id: u16) {
        if let Some(sessions) = self.subscriptions.remove(&comp_id) {
            for session_id in sessions {
                if let Some(comps) = self.by_session.get_mut(&session_id) {
                    comps.remove(&comp_id);
                }
            }
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
    pub fn build_events(&self, changed_comps: &[u16], tree: &ComponentTree) -> Vec<(u16, Vec<u8>)> {
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
            payload.push(b'e'); // lowercase — unsolicited event
            payload.push(0xFF); // replyNum (unused for events)
            payload.extend_from_slice(&comp_id.to_be_bytes());
            payload.push(b'r'); // what = runtime

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
    pub fn build_config_events(&self, comp_id: u16, tree: &ComponentTree) -> Vec<(u16, Vec<u8>)> {
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
            if slot.flags & SLOT_FLAG_ACTION != 0 {
                continue;
            }
            if slot.flags & SLOT_FLAG_CONFIG == 0 {
                continue;
            }
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
    handle_sox_request_with_dyn(request, tree, subscriptions, session_id, None)
}

/// Handle a SOX request with optional access to the dynamic slot store.
///
/// When `dyn_store` is provided, `readComp(what='d')` returns dynamic tags.
pub fn handle_sox_request_with_dyn(
    request: &SoxRequest,
    tree: &mut ComponentTree,
    subscriptions: &mut SubscriptionManager,
    session_id: u16,
    dyn_store: Option<&super::DynSlotStoreHandle>,
) -> SoxResponse {
    match request.cmd {
        SoxCmd::ReadSchema => handle_read_schema(request),
        SoxCmd::ReadVersion => handle_read_version(request),
        SoxCmd::ReadComp => handle_read_comp(request, tree, dyn_store),
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
        SoxCmd::FileOpen => handle_file_open(request, session_id),
        SoxCmd::FileRead => handle_file_read(request, session_id),
        SoxCmd::FileWrite => handle_file_write(request, session_id),
        SoxCmd::FileClose => handle_file_close(request, session_id),
        SoxCmd::FileRename => handle_file_rename(request),
        _ => error_msg(request.cmd, request.req_id, "unsupported command"),
    }
}

// ---- SOX File Transfer ----

use std::path::{Component, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The one active SOX file transfer (get or put). It belongs to the DASP
/// session that opened it; other sessions get "busy" (R-15, S-45).
static SOX_FILE_XFER: Mutex<Option<SoxFileXfer>> = Mutex::new(None);

/// Session whose open put was dropped by `release_stale_file_xfer`; its next
/// fileClose gets "transfer expired" instead of a success (nothing was written).
static SOX_FILE_XFER_DROPPED: Mutex<Option<u16>> = Mutex::new(None);

/// A transfer whose owner sends nothing for this long is dropped.
pub const SOX_FILE_XFER_IDLE: Duration = Duration::from_secs(60);

/// Largest file a put may announce.
const SOX_PUT_MAX_SIZE: usize = 10 * 1024 * 1024;

/// Transfer mode: reading (get) or writing (put).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SoxFileXferMode {
    Get,
    Put,
}

struct SoxFileXfer {
    /// DASP session that opened the transfer.
    owner: u16,
    /// Last accepted request from the owner; see `SOX_FILE_XFER_IDLE`.
    last_activity: Instant,
    /// For GET: the full file data. For PUT: a pre-allocated buffer filled by chunks.
    data: Vec<u8>,
    chunk_size: usize,
    mode: SoxFileXferMode,
    /// PUT only: the requested path, validated again (and its directories
    /// created) at fileClose, just before the write.
    write_path: Option<String>,
    /// GET: bytes served from `offset`. PUT: total expected file size.
    file_size: usize,
    /// PUT only: total number of expected chunks.
    num_chunks: usize,
    /// PUT only: count of distinct chunks received so far.
    chunks_received: usize,
    /// PUT only: one bit per chunk, set when that chunk arrived.
    received: Vec<u64>,
    /// PUT only: chunks refused (bad size, number or payload). The first is
    /// logged at warn, the rest at debug; the count is logged at the end.
    rejected: usize,
    /// GET only: byte offset in the file (from fileOpen headers).
    offset: usize,
}

impl SoxFileXfer {
    fn get(owner: u16, data: Vec<u8>, chunk_size: usize, file_size: usize, offset: usize) -> Self {
        Self {
            owner,
            last_activity: Instant::now(),
            data,
            chunk_size,
            mode: SoxFileXferMode::Get,
            write_path: None,
            file_size,
            num_chunks: 0,
            chunks_received: 0,
            received: Vec::new(),
            rejected: 0,
            offset,
        }
    }

    /// `chunk_size` must be > 0.
    fn put(owner: u16, file_size: usize, chunk_size: usize, write_path: Option<String>) -> Self {
        // Sedona: even a zero-byte file has one (empty) chunk.
        let num_chunks = file_size.div_ceil(chunk_size).max(1);
        Self {
            owner,
            last_activity: Instant::now(),
            data: vec![0u8; file_size],
            chunk_size,
            mode: SoxFileXferMode::Put,
            write_path,
            file_size,
            num_chunks,
            chunks_received: 0,
            received: vec![0u64; num_chunks.div_ceil(64)],
            rejected: 0,
            offset: 0,
        }
    }

    /// Every chunk arrived (a zero-byte put needs none).
    fn put_complete(&self) -> bool {
        self.file_size == 0 || self.chunks_received == self.num_chunks
    }

    /// Copy one put chunk into the buffer. The chunk must have exactly the
    /// length its position calls for, so a complete set of chunks fills the
    /// whole buffer. A repeated chunk overwrites and is not counted twice.
    fn store_chunk(&mut self, chunk_num: usize, chunk: &[u8]) -> Result<(), &'static str> {
        if self.mode != SoxFileXferMode::Put {
            return Err("not in put mode");
        }
        if chunk_num >= self.num_chunks {
            return Err("chunk out of range");
        }
        let start = chunk_num
            .checked_mul(self.chunk_size)
            .ok_or("chunk out of range")?;
        let expected = self.file_size.saturating_sub(start).min(self.chunk_size);
        if chunk.len() != expected {
            return Err("bad chunk size");
        }
        let end = start.checked_add(expected).ok_or("chunk out of range")?;
        let dst = self.data.get_mut(start..end).ok_or("chunk out of range")?;
        dst.copy_from_slice(chunk);
        // INVARIANT: chunk_num < num_chunks, received has num_chunks bits.
        let (word, bit) = (chunk_num / 64, 1u64 << (chunk_num % 64));
        if let Some(w) = self.received.get_mut(word) {
            if *w & bit == 0 {
                *w |= bit;
                self.chunks_received += 1;
            }
        }
        self.last_activity = Instant::now();
        Ok(())
    }

    /// Count a refused chunk; only the first per transfer is a warning, so
    /// a client sending bad chunks cannot flood the log.
    fn note_rejected(&mut self, chunk_num: Option<usize>, len: usize, msg: &str) {
        self.rejected += 1;
        if self.rejected == 1 {
            tracing::warn!(
                session = self.owner,
                chunk_num,
                len,
                msg,
                "SOX: put chunk rejected (further rejections at debug)"
            );
        } else {
            tracing::debug!(
                session = self.owner,
                chunk_num,
                len,
                msg,
                "SOX: put chunk rejected"
            );
        }
    }

    /// Bytes of GET chunk `chunk_num`, or `None` past the end.
    fn get_chunk(&self, chunk_num: usize) -> Option<&[u8]> {
        let start = chunk_num
            .checked_mul(self.chunk_size)?
            .checked_add(self.offset)?;
        let limit = self
            .offset
            .checked_add(self.file_size)?
            .min(self.data.len());
        let end = start.saturating_add(self.chunk_size).min(limit);
        if start >= end {
            return None;
        }
        self.data.get(start..end)
    }
}

/// The transfer, only when `session` owns it.
fn owned_xfer<'a>(
    guard: &'a mut std::sync::MutexGuard<'static, Option<SoxFileXfer>>,
    session: u16,
) -> Option<&'a mut SoxFileXfer> {
    guard.as_mut().filter(|x| x.owner == session)
}

/// Drop the transfer when its session is no longer in `live_sessions`
/// (expired or closed) or it has been idle for `idle`. Called from the SOX
/// loop's cleanup tick. A dropped put leaves a mark so the owner's later
/// fileClose reports it instead of a success.
pub fn release_stale_file_xfer(live_sessions: &[u16], idle: Duration) {
    let mut xfer = crate::lock::lock_or_recover(&SOX_FILE_XFER, "SOX file transfer");
    let reason = match xfer.as_ref() {
        Some(x) if !live_sessions.contains(&x.owner) => "session ended",
        Some(x) if x.last_activity.elapsed() >= idle => "idle",
        _ => return,
    };
    let Some(x) = xfer.take() else { return };
    drop(xfer);
    tracing::warn!(
        session = x.owner,
        mode = ?x.mode,
        chunks = x.chunks_received,
        expected = x.num_chunks,
        rejected = x.rejected,
        reason,
        "SOX: file transfer dropped without fileClose (nothing written)"
    );
    if x.mode == SoxFileXferMode::Put {
        *crate::lock::lock_or_recover(&SOX_FILE_XFER_DROPPED, "SOX dropped transfer") =
            Some(x.owner);
    }
}

const SOX_CHUNK_SIZE: usize = 256;
const KITS_BASE_DIR: &str = "/home/eacio/sandstar/etc/kits";
const MANIFESTS_DIR: &str = "/home/eacio/sandstar/etc/manifests";
/// Allowed directories for file writes (put operations).
const WRITE_ALLOWED_DIRS: &[&str] = &[MANIFESTS_DIR, "/home/eacio/sandstar/etc/config", "/tmp"];

/// Allowed write directories. Off Linux (Windows dev) the OS temp directory
/// is added; on the device `TMPDIR` must not widen the list.
fn allowed_write_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = WRITE_ALLOWED_DIRS.iter().map(PathBuf::from).collect();
    if cfg!(not(target_os = "linux")) {
        let temp = std::env::temp_dir();
        if !dirs.contains(&temp) {
            dirs.push(temp);
        }
    }
    dirs
}

fn read_allowed_dirs() -> Vec<PathBuf> {
    [KITS_BASE_DIR, MANIFESTS_DIR]
        .iter()
        .map(PathBuf::from)
        .collect()
}

/// Resolve a SOX file URI to a local filesystem path.
///
/// Supports URI schemes:
///   - `m:kitname.xml` — kit manifest
///   - `m:m.zip` — bundled manifests (not supported)
///   - `/kits/...` — kit binary
///   - absolute paths — used directly
fn resolve_sox_uri(uri: &str) -> Result<String, &'static str> {
    if let Some(manifest_name) = uri.strip_prefix("m:") {
        if manifest_name == "m.zip" {
            return Err("m.zip not supported");
        }
        Ok(format!("{}/{}", MANIFESTS_DIR, manifest_name))
    } else if let Some(kit_path) = uri.strip_prefix("/kits/") {
        Ok(format!("{}/{}", KITS_BASE_DIR, kit_path))
    } else if uri.starts_with('/') || (cfg!(windows) && uri.len() >= 2 && uri.as_bytes()[1] == b':')
    {
        // Absolute path — used directly (validated later against allowed dirs)
        Ok(uri.to_string())
    } else {
        Ok(format!("{}/{}", KITS_BASE_DIR, uri))
    }
}

/// What a validated path is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathUse {
    /// Read an existing file under the kits or manifests directory.
    Read,
    /// Check a write target without touching the disk (fileOpen put).
    WriteCheck,
    /// Write target: missing directories are created after the checks pass
    /// (fileClose, rename destination).
    Write,
    /// The source of a rename: must exist, not be a link, and lie in an
    /// allowed write directory; nothing is created.
    RenameSource,
}

/// `path` is one of `dirs` or below it, compared by path components
/// (`/tmp` does not admit `/tmpX`).
fn under_any(path: &Path, dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|d| path.starts_with(d))
}

/// `dirs` with links resolved, as far as they exist: the existing part is
/// canonicalized and the missing rest appended.
fn canonical_dirs(dirs: &[PathBuf]) -> Result<Vec<PathBuf>, &'static str> {
    dirs.iter().map(|d| resolve_existing(d)).collect()
}

/// `path` with its longest existing prefix canonicalized (links resolved)
/// and the remaining, not yet existing components appended. Where a
/// missing directory would really be created. A stat error is an error,
/// never "missing".
fn resolve_existing(path: &Path) -> Result<PathBuf, &'static str> {
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    for anc in path.ancestors() {
        match anc.try_exists() {
            Ok(true) => {
                let mut out = std::fs::canonicalize(anc).map_err(|e| {
                    tracing::warn!(path = ?anc, err = %e, "SOX: cannot resolve path");
                    "invalid path"
                })?;
                out.extend(rest.iter().rev());
                return Ok(out);
            }
            Ok(false) => match anc.file_name() {
                Some(name) => rest.push(name),
                None => return Err("invalid path"),
            },
            Err(e) => {
                tracing::warn!(path = ?anc, err = %e, "SOX: cannot stat path");
                return Err("invalid path");
            }
        }
    }
    Err("invalid path")
}

/// Check a resolved path before any file system change (R-15).
fn validate_path(local_path: &str, use_: PathUse) -> Result<PathBuf, &'static str> {
    validate_path_in(
        local_path,
        use_,
        &allowed_write_dirs(),
        &read_allowed_dirs(),
    )
}

/// Order: syntax (NUL, `..` component, absolute) → allow-list on the path
/// as written → allow-list on the real location (existing part resolved,
/// links followed) → only for `Write`: create missing directories and check
/// the canonical parent again. The final component must not be a link.
/// Every comparison is by path component.
fn validate_path_in(
    local_path: &str,
    use_: PathUse,
    write_dirs: &[PathBuf],
    read_dirs: &[PathBuf],
) -> Result<PathBuf, &'static str> {
    let path = Path::new(local_path);
    if local_path.contains('\0')
        || !path.is_absolute()
        || path.components().any(|c| c == Component::ParentDir)
    {
        return Err("invalid path");
    }

    if use_ == PathUse::Read {
        let canonical = std::fs::canonicalize(path).map_err(|_| "file not found")?;
        if !under_any(&canonical, &canonical_dirs(read_dirs)?) {
            return Err("invalid path");
        }
        return Ok(canonical);
    }

    let parent = path.parent().ok_or("invalid path")?;
    let filename = path.file_name().ok_or("invalid path")?;
    if !under_any(parent, write_dirs) {
        return Err("invalid path");
    }
    let canonical_write_dirs = canonical_dirs(write_dirs)?;

    // Where the file really lands, links in existing ancestors followed.
    let real_parent = resolve_existing(parent)?;
    if !under_any(&real_parent, &canonical_write_dirs) {
        return Err("invalid path");
    }

    let parent_exists = parent.try_exists().map_err(|e| {
        tracing::warn!(dir = ?parent, err = %e, "SOX: cannot stat directory");
        "invalid path"
    })?;
    let final_parent = match (use_, parent_exists) {
        (PathUse::RenameSource, false) => return Err("not found"),
        (PathUse::WriteCheck, false) => real_parent,
        (PathUse::Write, false) => {
            std::fs::create_dir_all(parent).map_err(|e| {
                tracing::warn!(dir = ?parent, err = %e, "SOX: cannot create directory");
                "cannot create directory"
            })?;
            let canonical = std::fs::canonicalize(parent).map_err(|e| {
                tracing::warn!(dir = ?parent, err = %e, "SOX: cannot resolve directory");
                "invalid path"
            })?;
            if !under_any(&canonical, &canonical_write_dirs) {
                return Err("invalid path");
            }
            canonical
        }
        _ => real_parent,
    };

    let full = final_parent.join(filename);
    match std::fs::symlink_metadata(&full) {
        Ok(meta) if meta.file_type().is_symlink() => {
            tracing::warn!(path = ?full, "SOX: refusing a symbolic link as file target");
            Err("invalid path")
        }
        Ok(_) => Ok(full),
        Err(_) if use_ == PathUse::RenameSource => Err("not found"),
        Err(_) => Ok(full),
    }
}

/// Write `data` to `path` atomically: a new temp file in the same
/// directory (never an existing file or link), flushed, then renamed over
/// the target. A failure leaves the old file as it was.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("no file name"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".sox-{}.tmp", std::process::id()));
    let tmp = dir.join(tmp_name);

    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp); // best effort; the error below is what counts
    }
    result
}

/// fileOpen ('f') — open a file for get (reading) or put (writing).
///
/// Request: str method, str uri, i4 fileSize, u2 chunkSize, headers[]
/// Response: u4 fileSize, u2 chunkSize, u1 end-of-headers(0x00)
fn handle_file_open(req: &SoxRequest, session: u16) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let (Some(method), Some(uri), Some(file_size), Some(chunk_size)) = (
        reader.read_str(),
        reader.read_str(),
        reader.read_u32(),
        reader.read_u16(),
    ) else {
        tracing::warn!(session, len = req.payload.len(), "SOX: fileOpen truncated");
        return error_msg(SoxCmd::FileOpen, req.req_id, "bad request");
    };
    let file_size = file_size as usize;
    let mut chunk_size = chunk_size as usize;

    tracing::info!(session, method = %method, uri = %uri, file_size, chunk_size, "SOX: fileOpen");

    // Parse optional headers (key-value pairs terminated by empty string)
    let mut offset: usize = 0;
    let mut _mode = String::from("w");
    loop {
        match reader.read_str() {
            Some(key) if key.is_empty() => break,
            Some(key) => {
                let val = reader.read_str().unwrap_or_default();
                match key.as_str() {
                    "offset" => match val.parse() {
                        Ok(v) => offset = v,
                        Err(_) => return error_msg(SoxCmd::FileOpen, req.req_id, "bad offset"),
                    },
                    "mode" => _mode = val,
                    _ => {} // ignore unknown headers
                }
            }
            None => break,
        }
    }

    // Cap chunk size
    if chunk_size == 0 {
        chunk_size = SOX_CHUNK_SIZE;
    }

    // Another session's transfer: busy, before anything else is done.
    {
        let xfer = crate::lock::lock_or_recover(&SOX_FILE_XFER, "SOX file transfer");
        if matches!(xfer.as_ref(), Some(x) if x.owner != session) {
            return error_msg(SoxCmd::FileOpen, req.req_id, "busy");
        }
    }

    let local_path = match resolve_sox_uri(&uri) {
        Ok(p) => p,
        Err(msg) => return error_msg(SoxCmd::FileOpen, req.req_id, msg),
    };

    let (file, size_u32) = match method.as_str() {
        "g" => {
            // --- GET: read file from disk ---
            let canonical = match validate_path(&local_path, PathUse::Read) {
                Ok(p) => p,
                Err(msg) => {
                    tracing::warn!(path = %local_path, msg, "SOX: fileOpen get rejected");
                    return error_msg(SoxCmd::FileOpen, req.req_id, msg);
                }
            };
            let data = match std::fs::read(&canonical) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(path = ?canonical, err = %e, "SOX: fileOpen read failed");
                    return error_msg(SoxCmd::FileOpen, req.req_id, "file not found");
                }
            };
            let actual_size = data.len();
            // If the client requested a specific size with offset, cap accordingly
            let effective_size = if file_size == 0 {
                actual_size.saturating_sub(offset)
            } else {
                file_size.min(actual_size.saturating_sub(offset))
            };
            let Ok(size_u32) = u32::try_from(effective_size) else {
                return error_msg(SoxCmd::FileOpen, req.req_id, "too big");
            };
            tracing::info!(path = ?canonical, size = effective_size, "SOX: fileOpen GET OK");
            (
                SoxFileXfer::get(session, data, chunk_size, effective_size, offset),
                size_u32,
            )
        }
        "p" => {
            // --- PUT: prepare to receive file data from client ---
            if file_size > SOX_PUT_MAX_SIZE {
                return error_msg(SoxCmd::FileOpen, req.req_id, "too big");
            }
            // The buffer is written whole at fileClose; a put at an offset
            // was never written where asked (S-45).
            if offset != 0 {
                return error_msg(SoxCmd::FileOpen, req.req_id, "offset not supported");
            }
            // Checked now, touched nothing: directories are created and the
            // path checked again at fileClose, right before the write.
            if let Err(msg) = validate_path(&local_path, PathUse::WriteCheck) {
                tracing::warn!(path = %local_path, msg, "SOX: fileOpen put rejected");
                return error_msg(SoxCmd::FileOpen, req.req_id, msg);
            }
            let file = SoxFileXfer::put(session, file_size, chunk_size, Some(local_path));
            tracing::info!(
                path = file.write_path.as_deref().unwrap_or_default(),
                file_size,
                num_chunks = file.num_chunks,
                chunk_size,
                "SOX: fileOpen PUT OK"
            );
            // INVARIANT: file_size <= SOX_PUT_MAX_SIZE fits in u32.
            (file, file_size as u32)
        }
        _ => return error_msg(SoxCmd::FileOpen, req.req_id, "bad method"),
    };

    // Install. The owner's previous transfer is replaced only now, so a
    // refused fileOpen leaves it as it was.
    let mut xfer = crate::lock::lock_or_recover(&SOX_FILE_XFER, "SOX file transfer");
    match xfer.as_ref() {
        Some(x) if x.owner != session => {
            return error_msg(SoxCmd::FileOpen, req.req_id, "busy");
        }
        Some(x) => tracing::warn!(
            session,
            mode = ?x.mode,
            chunks = x.chunks_received,
            expected = x.num_chunks,
            "SOX: fileOpen replaces this session's open transfer (nothing written)"
        ),
        None => {}
    }
    *xfer = Some(file);
    drop(xfer);
    let mut dropped = crate::lock::lock_or_recover(&SOX_FILE_XFER_DROPPED, "SOX dropped transfer");
    if *dropped == Some(session) {
        *dropped = None;
    }

    let mut resp = SoxResponse::success(SoxCmd::FileOpen, req.req_id);
    resp.write_u32(size_u32);
    resp.write_u16(chunk_size as u16);
    resp.write_u8(0x00); // end of headers
    resp
}

/// fileRead ('g') — read a chunk of the open file.
/// Request: u2 chunkNum
/// Response: raw chunk bytes
fn handle_file_read(req: &SoxRequest, session: u16) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let Some(chunk_num) = reader.read_u16() else {
        return error_msg(SoxCmd::FileRead, req.req_id, "bad chunk");
    };

    let mut guard = crate::lock::lock_or_recover(&SOX_FILE_XFER, "SOX file transfer");
    let Some(file) = owned_xfer(&mut guard, session) else {
        return error_msg(SoxCmd::FileRead, req.req_id, "no file open");
    };
    if file.mode != SoxFileXferMode::Get {
        return error_msg(SoxCmd::FileRead, req.req_id, "not in get mode");
    }
    file.last_activity = Instant::now();
    match file.get_chunk(chunk_num as usize) {
        Some(bytes) => {
            let mut resp = SoxResponse::success(SoxCmd::FileRead, req.req_id);
            resp.write_bytes(bytes);
            resp
        }
        None => error_msg(SoxCmd::FileRead, req.req_id, "chunk out of range"),
    }
}

/// fileClose ('z'/'q') — close the current file transfer.
///
/// For GET: simply releases the file data.
/// For PUT: writes the received data to disk (atomically) if every chunk
/// arrived; otherwise nothing is written and "incomplete" is returned
/// (S-45). The transfer is released in every case. A put that was dropped
/// as stale answers "transfer expired".
fn handle_file_close(req: &SoxRequest, session: u16) -> SoxResponse {
    let mut guard = crate::lock::lock_or_recover(&SOX_FILE_XFER, "SOX file transfer");
    if owned_xfer(&mut guard, session).is_none() {
        let others = guard.is_some();
        drop(guard);
        let mut dropped =
            crate::lock::lock_or_recover(&SOX_FILE_XFER_DROPPED, "SOX dropped transfer");
        if *dropped == Some(session) {
            *dropped = None;
            tracing::warn!(
                session,
                "SOX: fileClose after the put was dropped (nothing written)"
            );
            return error_msg(SoxCmd::FileClose, req.req_id, "transfer expired");
        }
        if others {
            // Another session's transfer: leave it alone.
            return error_msg(SoxCmd::FileClose, req.req_id, "no file open");
        }
        tracing::info!(session, "SOX: fileClose (no transfer)");
        return SoxResponse::success(SoxCmd::FileClose, req.req_id);
    }
    let Some(file) = guard.take() else {
        return SoxResponse::success(SoxCmd::FileClose, req.req_id);
    };
    drop(guard);

    if file.mode == SoxFileXferMode::Put {
        let requested = file.write_path.as_deref().unwrap_or_default();
        if !file.put_complete() {
            tracing::warn!(
                session,
                path = requested,
                chunks = file.chunks_received,
                expected = file.num_chunks,
                rejected = file.rejected,
                "SOX: fileClose — put incomplete, nothing written"
            );
            return error_msg(SoxCmd::FileClose, req.req_id, "incomplete");
        }
        if file.write_path.is_some() {
            // Checked again: the tree may have changed since fileOpen.
            let path = match validate_path(requested, PathUse::Write) {
                Ok(p) => p,
                Err(msg) => {
                    tracing::warn!(
                        path = requested,
                        msg,
                        "SOX: fileClose — put target rejected"
                    );
                    return error_msg(SoxCmd::FileClose, req.req_id, msg);
                }
            };
            tracing::info!(
                path = ?path,
                chunks = file.chunks_received,
                size = file.data.len(),
                "SOX: fileClose — flushing PUT to disk"
            );
            if let Err(e) = write_atomic(&path, &file.data) {
                tracing::error!(path = ?path, err = %e, "SOX: fileClose — failed to write file");
                return error_msg(SoxCmd::FileClose, req.req_id, "write failed");
            }
        }
    }
    tracing::info!(session, "SOX: fileClose");
    SoxResponse::success(SoxCmd::FileClose, req.req_id)
}

/// fileWrite ('h') — receive a chunk of file data during a PUT transfer.
///
/// This is the Sandstar extension for receiving file chunks. The original
/// Sedona protocol uses 'k' for both invoke and chunk transfer, but we use
/// 'h' (FileWrite) as a dedicated command for clarity.
///
/// Request: u2 chunkNum, u2 chunkSize, u1[chunkSize] data
/// Response: 'H' + replyNum (success) or '!' + replyNum (error)
fn handle_file_write(req: &SoxRequest, session: u16) -> SoxResponse {
    let mut guard = crate::lock::lock_or_recover(&SOX_FILE_XFER, "SOX file transfer");
    let Some(file) = owned_xfer(&mut guard, session) else {
        return error_msg(SoxCmd::FileWrite, req.req_id, "no file open");
    };
    let result = parse_put_chunk(&req.payload).and_then(|(n, chunk)| {
        file.store_chunk(n, chunk)
            .map(|()| n)
            .map_err(|msg| (Some(n), chunk.len(), msg))
    });
    match result {
        Ok(chunk_num) => {
            tracing::debug!(
                chunk_num,
                received = file.chunks_received,
                expected = file.num_chunks,
                "SOX: fileWrite chunk received"
            );
            SoxResponse::success(SoxCmd::FileWrite, req.req_id)
        }
        Err((chunk_num, len, msg)) => {
            file.note_rejected(chunk_num, len, msg);
            error_msg(SoxCmd::FileWrite, req.req_id, msg)
        }
    }
}

/// A refused put chunk: chunk number (if it could be read), payload length, message.
type ChunkError = (Option<usize>, usize, &'static str);

/// Parse a put chunk: u2(chunkNum) + u2(chunkSize) + u1[chunkSize](data).
/// On error: (chunk number if read, payload length, message).
fn parse_put_chunk(payload: &[u8]) -> Result<(usize, &[u8]), ChunkError> {
    let mut reader = SoxReader::new(payload);
    let bad = |n, msg| (n, payload.len(), msg);
    let chunk_num = reader.read_u16().ok_or(bad(None, "bad chunk"))? as usize;
    let len = reader.read_u16().ok_or(bad(Some(chunk_num), "bad chunk"))? as usize;
    let data = reader
        .read_bytes(len)
        .ok_or(bad(Some(chunk_num), "truncated chunk"))?;
    Ok((chunk_num, data))
}

/// True when `session` has a PUT transfer open.
///
/// The dispatch loop routes that session's 'k' messages to the chunk
/// handler instead of the invoke handler; other sessions' 'k' stay invokes.
pub fn is_put_transfer_active(session: u16) -> bool {
    let xfer = crate::lock::lock_or_recover(&SOX_FILE_XFER, "SOX file transfer");
    matches!(&*xfer, Some(f) if f.mode == SoxFileXferMode::Put && f.owner == session)
}

/// Handle a raw 'k' chunk message during a PUT transfer.
///
/// Called from the dispatch loop when `is_put_transfer_active(session)` is true.
/// The payload is everything after the 2-byte SOX header (cmd + replyNum).
///
/// Returns `None`: chunk receipts are silent, as in Sedona. A rejected
/// chunk is counted (first one logged at warn); fileClose then reports the
/// put as incomplete.
pub fn handle_put_chunk(session: u16, payload: &[u8]) -> Option<SoxResponse> {
    let mut guard = crate::lock::lock_or_recover(&SOX_FILE_XFER, "SOX file transfer");
    let Some(file) = owned_xfer(&mut guard, session) else {
        // Only after the transfer was dropped between routing and here.
        tracing::debug!(session, "SOX: put chunk without an open transfer");
        return None;
    };
    let result = parse_put_chunk(payload).and_then(|(n, chunk)| {
        file.store_chunk(n, chunk)
            .map(|()| n)
            .map_err(|msg| (Some(n), chunk.len(), msg))
    });
    match result {
        Ok(chunk_num) => tracing::debug!(
            chunk_num,
            received = file.chunks_received,
            expected = file.num_chunks,
            "SOX: put chunk received"
        ),
        Err((chunk_num, len, msg)) => file.note_rejected(chunk_num, len, msg),
    }
    None
}

/// fileRename ('b') — rename a file on the device.
///
/// Request: str from, str to
/// Response: 'B' + replyNum (success) or '!' + replyNum (error)
///
/// Both paths must be within allowed write directories.
fn handle_file_rename(req: &SoxRequest) -> SoxResponse {
    let mut reader = SoxReader::new(&req.payload);
    let from_uri = reader.read_str().unwrap_or_default();
    let to_uri = reader.read_str().unwrap_or_default();

    tracing::info!(from = %from_uri, to = %to_uri, "SOX: fileRename");

    if from_uri.is_empty() || to_uri.is_empty() {
        return error_msg(SoxCmd::FileRename, req.req_id, "empty path");
    }

    // Resolve URIs to local paths
    let from_path = match resolve_sox_uri(&from_uri) {
        Ok(p) => p,
        Err(msg) => return error_msg(SoxCmd::FileRename, req.req_id, msg),
    };
    let to_path = match resolve_sox_uri(&to_uri) {
        Ok(p) => p,
        Err(msg) => return error_msg(SoxCmd::FileRename, req.req_id, msg),
    };

    // The source must exist within the allowed write dirs; nothing is created for it.
    let canonical_from = match validate_path(&from_path, PathUse::RenameSource) {
        Ok(p) => p,
        Err(msg) => {
            tracing::warn!(from = %from_path, msg, "SOX: fileRename source rejected");
            return error_msg(SoxCmd::FileRename, req.req_id, msg);
        }
    };

    // Validate destination path is within allowed write dirs
    let canonical_to = match validate_path(&to_path, PathUse::Write) {
        Ok(p) => p,
        Err(msg) => {
            tracing::warn!(to = %to_path, msg, "SOX: fileRename dest rejected");
            return error_msg(SoxCmd::FileRename, req.req_id, msg);
        }
    };

    match std::fs::rename(&canonical_from, &canonical_to) {
        Ok(()) => {
            tracing::info!(from = ?canonical_from, to = ?canonical_to, "SOX: fileRename OK");
            SoxResponse::success(SoxCmd::FileRename, req.req_id)
        }
        Err(e) => {
            tracing::error!(from = ?canonical_from, to = ?canonical_to, err = %e, "SOX: fileRename failed");
            error_msg(SoxCmd::FileRename, req.req_id, "rename failed")
        }
    }
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
    resp.write_str("EacIo"); // platformId
    resp.write_u8(0x00); // scodeFlags
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
/// Request payload: u2 compId, u1 what ('t'=tree, 'c'=config, 'r'=runtime, 'l'=links, 'd'=dynamic tags).
///
/// The `what='d'` mode is a Sandstar extension (not part of standard Sedona SOX).
/// It returns dynamic tags from the side-car DynSlotStore encoded as:
/// `u16 tag_count`, then for each tag: `null-terminated key + u8 type_code + value bytes`.
fn handle_read_comp(
    req: &SoxRequest,
    tree: &ComponentTree,
    dyn_store: Option<&super::DynSlotStoreHandle>,
) -> SoxResponse {
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
                if slot.flags & SLOT_FLAG_ACTION != 0 {
                    continue;
                }
                if slot.flags & SLOT_FLAG_CONFIG == 0 {
                    continue;
                } // skip non-config
                encode_slot_value(&mut resp, &slot.value);
            }
        }
        b'r' => {
            // Runtime: only slots WITHOUT CONFIG flag, in schema order.
            for slot in &comp.slots {
                if slot.flags & SLOT_FLAG_ACTION != 0 {
                    continue;
                }
                if slot.flags & SLOT_FLAG_CONFIG != 0 {
                    continue;
                } // skip config
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
        b'd' => {
            // Dynamic tags (Sandstar extension — not part of standard Sedona SOX).
            // Encodes the component's dynamic tags from the side-car DynSlotStore.
            handle_read_comp_dynamic(comp_id, dyn_store, &mut resp);
        }
        _ => {
            // Unknown what — return empty
        }
    }

    resp
}

/// Encode dynamic tags from the DynSlotStore into a readComp response.
///
/// Wire format: `u16 tag_count`, then for each tag:
///   - Null-terminated key string
///   - `u8` type code (0=Null, 1=Marker, 2=Bool, 3=Int, 4=Float, 5=Str, 6=Ref)
///   - Type-specific value bytes
fn handle_read_comp_dynamic(
    comp_id: u16,
    dyn_store: Option<&super::DynSlotStoreHandle>,
    response: &mut SoxResponse,
) {
    if let Some(store_handle) = dyn_store {
        let store = crate::lock::read_or_recover(store_handle, "dyn slot store");
        let tags = store.get_all_with_computed(comp_id);
        response.write_u16(tags.len() as u16);
        // Sort by key for deterministic wire order (important for tests and debugging).
        let mut sorted: Vec<_> = tags.iter().collect();
        sorted.sort_by_key(|(k, _)| k.as_str());
        for (key, value) in sorted {
            response.write_str(key);
            encode_dyn_value(response, value);
        }
    } else {
        response.write_u16(0); // no dynamic slot store — zero tags
    }
}

/// Encode a single `DynValue` onto a SOX response payload.
///
/// Type codes:
/// - 0 = Null
/// - 1 = Marker (no value bytes)
/// - 2 = Bool (`u8`: 0 or 1)
/// - 3 = Int (`i32` big-endian, truncated from i64)
/// - 4 = Float (`f32` big-endian, truncated from f64)
/// - 5 = Str (null-terminated)
/// - 6 = Ref (null-terminated)
fn encode_dyn_value(response: &mut SoxResponse, value: &super::dyn_slots::DynValue) {
    use super::dyn_slots::DynValue;
    match value {
        DynValue::Null => {
            response.write_u8(0);
        }
        DynValue::Marker => {
            response.write_u8(1);
        }
        DynValue::Bool(b) => {
            response.write_u8(2);
            response.write_u8(if *b { 1 } else { 0 });
        }
        DynValue::Int(i) => {
            response.write_u8(3);
            response.write_i32(*i as i32);
        }
        DynValue::Float(f) => {
            response.write_u8(4);
            response.write_f32(*f as f32);
        }
        DynValue::Str(s) => {
            response.write_u8(5);
            response.write_str(s);
        }
        DynValue::Ref(r) => {
            response.write_u8(6);
            response.write_str(r);
        }
    }
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

        tracing::info!(
            session = session_id,
            comp_id,
            what_byte = what,
            "SOX: doSubscribe (old protocol)"
        );

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
                        if slot.flags & SLOT_FLAG_ACTION != 0 {
                            continue;
                        }
                        if slot.flags & SLOT_FLAG_CONFIG == 0 {
                            continue;
                        }
                        encode_slot_value(&mut resp, &slot.value);
                    }
                }
                b'r' => {
                    for slot in &comp.slots {
                        if slot.flags & SLOT_FLAG_ACTION != 0 {
                            continue;
                        }
                        if slot.flags & SLOT_FLAG_CONFIG != 0 {
                            continue;
                        }
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
        tracing::info!(
            session = session_id,
            mask,
            requested = comp_ids.len(),
            total = all_ids.len(),
            "SOX: batchSubscribe"
        );

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
            VirtualSlot {
                name: "meta".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(1),
            },
            VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(0),
            },
            VirtualSlot {
                name: "set".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_ACTION,
                value: SlotValue::Int(0),
            },
        ],
        // control::Add2 (kit 2, type 3), Sub2 (49), Mul2 (37)
        // Manifest: meta(Int,config), out(Float,runtime), in1(Float,runtime), in2(Float,runtime)
        (2, 3) | (2, 49) | (2, 37) => vec![
            VirtualSlot {
                name: "meta".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(1),
            },
            VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Float as u8,
                flags: SLOT_FLAG_RUNTIME,
                value: SlotValue::Float(0.0),
            },
            VirtualSlot {
                name: "in1".into(),
                type_id: SoxValueType::Float as u8,
                flags: SLOT_FLAG_RUNTIME,
                value: SlotValue::Float(0.0),
            },
            VirtualSlot {
                name: "in2".into(),
                type_id: SoxValueType::Float as u8,
                flags: SLOT_FLAG_RUNTIME,
                value: SlotValue::Float(0.0),
            },
        ],
        // control::Div2 (kit 2, type 18)
        // Manifest: meta(Int,config), out(Float,runtime), in1(Float,runtime), in2(Float,runtime), div0(Bool,runtime)
        (2, 18) => vec![
            VirtualSlot {
                name: "meta".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(1),
            },
            VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Float as u8,
                flags: SLOT_FLAG_RUNTIME,
                value: SlotValue::Float(0.0),
            },
            VirtualSlot {
                name: "in1".into(),
                type_id: SoxValueType::Float as u8,
                flags: SLOT_FLAG_RUNTIME,
                value: SlotValue::Float(0.0),
            },
            VirtualSlot {
                name: "in2".into(),
                type_id: SoxValueType::Float as u8,
                flags: SLOT_FLAG_RUNTIME,
                value: SlotValue::Float(0.0),
            },
            VirtualSlot {
                name: "div0".into(),
                type_id: SoxValueType::Bool as u8,
                flags: SLOT_FLAG_RUNTIME,
                value: SlotValue::Bool(false),
            },
        ],
        // control::ConstBool (kit 2, type 13)
        // Manifest: meta, out(Bool,config), setTrue(Void,action), setFalse(Void,action), setNull(Void,action)
        (2, 13) => vec![
            VirtualSlot {
                name: "meta".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(1),
            },
            VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Bool as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Bool(false),
            },
            VirtualSlot {
                name: "setTrue".into(),
                type_id: SoxValueType::Void as u8,
                flags: SLOT_FLAG_ACTION,
                value: SlotValue::Null,
            },
            VirtualSlot {
                name: "setFalse".into(),
                type_id: SoxValueType::Void as u8,
                flags: SLOT_FLAG_ACTION,
                value: SlotValue::Null,
            },
            VirtualSlot {
                name: "setNull".into(),
                type_id: SoxValueType::Void as u8,
                flags: SLOT_FLAG_ACTION,
                value: SlotValue::Null,
            },
        ],
        // control::WriteBool (kit 2, type 56)
        // Manifest: meta, in(Bool,config), out(Bool,runtime), setTrue/setFalse/setNull(actions)
        (2, 56) => vec![
            VirtualSlot {
                name: "meta".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(1),
            },
            VirtualSlot {
                name: "in".into(),
                type_id: SoxValueType::Bool as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Bool(false),
            },
            VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Bool as u8,
                flags: SLOT_FLAG_RUNTIME,
                value: SlotValue::Bool(false),
            },
            VirtualSlot {
                name: "setTrue".into(),
                type_id: SoxValueType::Void as u8,
                flags: SLOT_FLAG_ACTION,
                value: SlotValue::Null,
            },
            VirtualSlot {
                name: "setFalse".into(),
                type_id: SoxValueType::Void as u8,
                flags: SLOT_FLAG_ACTION,
                value: SlotValue::Null,
            },
            VirtualSlot {
                name: "setNull".into(),
                type_id: SoxValueType::Void as u8,
                flags: SLOT_FLAG_ACTION,
                value: SlotValue::Null,
            },
        ],
        // control::WriteFloat (kit 2, type 57)
        // Manifest: meta, in(Float,config), out(Float,runtime), set(Float,action), setNull(Void,action)
        (2, 57) => vec![
            VirtualSlot {
                name: "meta".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(1),
            },
            VirtualSlot {
                name: "in".into(),
                type_id: SoxValueType::Float as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Float(0.0),
            },
            VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Float as u8,
                flags: SLOT_FLAG_RUNTIME,
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
        // control::WriteInt (kit 2, type 58)
        // Manifest: meta, in(Int,config), out(Int,runtime), set(Int,action)
        (2, 58) => vec![
            VirtualSlot {
                name: "meta".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(1),
            },
            VirtualSlot {
                name: "in".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(0),
            },
            VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_RUNTIME,
                value: SlotValue::Int(0),
            },
            VirtualSlot {
                name: "set".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_ACTION,
                value: SlotValue::Int(0),
            },
        ],
        // EacIo::ChannelRead (kit 1, type 100) — bridge sensor values to control logic
        // User sets channelId (config), out auto-updates with live sensor value every tick
        (1, 100) => vec![
            VirtualSlot {
                name: "meta".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(1),
            },
            VirtualSlot {
                name: "channelId".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(0),
            },
            VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Float as u8,
                flags: SLOT_FLAG_RUNTIME,
                value: SlotValue::Float(0.0),
            },
            VirtualSlot {
                name: "status".into(),
                type_id: SoxValueType::Buf as u8,
                flags: SLOT_FLAG_RUNTIME,
                value: SlotValue::Str(String::new()),
            },
        ],
        // Default: meta slot + auto-extend on write for unknown types
        _ => vec![VirtualSlot {
            name: "meta".into(),
            type_id: SoxValueType::Int as u8,
            flags: SLOT_FLAG_CONFIG,
            value: SlotValue::Int(1),
        }],
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

    if let Some(err) = validate_sox_name(&name) {
        return error_msg(req.cmd, req.req_id, err);
    }

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
    tree.mark_user_added(new_id);

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

    if let Some(removed) = tree.remove_subtree(comp_id) {
        tree.mark_dirty();
        tracing::info!(comp_id, removed = removed.len(), "SOX: component deleted");
        SoxResponse::success(SoxCmd::Delete, req.req_id)
    } else {
        error_msg(req.cmd, req.req_id, "bad compId")
    }
}

/// Max component name length for Sedona compatibility (31 chars).
const MAX_NAME_LEN: usize = 31;

/// Validate a component name: non-empty, max 31 chars, starts with letter,
/// only letters/digits/underscores. Returns error message or None if valid.
fn validate_sox_name(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("name cannot be empty");
    }
    if name.len() > MAX_NAME_LEN {
        return Some("name too long (max 31 chars)");
    }
    if !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return Some("name must start with a letter");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Some("name can only contain letters, numbers, and underscores");
    }
    None
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

    if let Some(err) = validate_sox_name(&new_name) {
        return error_msg(req.cmd, req.req_id, err);
    }

    if tree.rename(comp_id, new_name.clone()) {
        if tree.is_user_added(comp_id) {
            tree.mark_dirty();
        }
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
            // Check for cycles before attempting to add.
            if tree.would_create_cycle(from_comp, to_comp) {
                return error_msg(req.cmd, req.req_id, "link rejected: would create cycle");
            }
            match tree.try_add_link(from_comp, from_slot, to_comp, to_slot) {
                Ok(()) => {
                    if tree.is_user_added(from_comp) || tree.is_user_added(to_comp) {
                        tree.mark_dirty();
                    }
                    tracing::info!(from_comp, from_slot, to_comp, to_slot, "SOX: link added");
                    SoxResponse::success(SoxCmd::Link, req.req_id)
                }
                Err(e) => {
                    tracing::warn!(from_comp, from_slot, to_comp, to_slot, reason = %e, "SOX: link refused");
                    error_msg(req.cmd, req.req_id, "link add failed")
                }
            }
        }
        b'd' => {
            if tree.remove_link(from_comp, from_slot, to_comp, to_slot) {
                if tree.is_user_added(from_comp) || tree.is_user_added(to_comp) {
                    tree.mark_dirty();
                }
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
        let action = comp
            .and_then(|c| c.slots.get(slot_id as usize))
            .map(|s| s.name.clone());
        // Find the writable output: "out" for Const*, "in" for Write*
        let target = comp.and_then(|c| {
            c.slots
                .iter()
                .find(|s| s.name == "out" && s.flags & SLOT_FLAG_ACTION == 0)
                .or_else(|| {
                    c.slots
                        .iter()
                        .find(|s| s.name == "in" && s.flags & SLOT_FLAG_ACTION == 0)
                })
        });
        let tname = target.map(|s| s.name.clone()).unwrap_or_default();
        let ttype = target
            .map(|s| s.type_id)
            .unwrap_or(SoxValueType::Float as u8);
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
        "save" | "hibernate" => {
            // Save/Hibernate: explicitly persist all user components to disk
            tracing::info!(action = %action_name, "SOX: saving user components");
            let _ = tree.save_user_components();
            None // no slot value change
        }
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
        if tree.is_user_added(comp_id) {
            tree.mark_dirty();
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
    let type_id = tree
        .get(comp_id)
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
                type_id,
                flags: SLOT_FLAG_RUNTIME,
                value: SlotValue::Null,
            });
        }
        let slot = &mut comp.slots[slot_id as usize];
        slot.type_id = type_id;
        tracing::info!(comp_id, slot_id, name = %slot.name, ?value, "SOX: slot value updated");
        slot.value = value;
    }

    // Mark dirty if this is a user-added component
    if tree.is_user_added(comp_id) {
        tree.mark_dirty();
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
    let type_id = tree
        .get(comp_id)
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

        // meta encodes canvas position: x=2, y=2 for first channel (index 0)
        assert_eq!(
            ch.slots[0].value,
            SlotValue::Int(encode_meta_with_pos(2, 2))
        ); // meta with position
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
        let resp = handle_read_comp(&req, &tree, None);
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
        let resp = handle_read_comp(&req, &tree, None);
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
        let resp = handle_read_comp(&req, &tree, None);
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
        let resp = handle_read_comp(&req, &tree, None);
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
        let resp = handle_read_comp(&req, &tree, None);
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
        let resp = handle_read_comp(&req, &tree, None);
        assert_eq!(resp.cmd, b'C');
        // Config mode: comp_id + what + config slot values (no tree structure)
        let mut r = SoxReader::new(&resp.payload);
        assert_eq!(r.read_u16(), Some(100)); // comp_id
        assert_eq!(r.read_u8(), Some(b'c')); // what byte echoed back
                                             // Config slots: meta (Int with position encoding) and pointQuery (Str="")
                                             // meta: i4(encode_meta_with_pos(2, 2)) for first channel (index 0)
        assert_eq!(r.read_i32(), Some(encode_meta_with_pos(2, 2)));
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
        assert_eq!(sedona_flags_to_slot_flags(""), SLOT_FLAG_RUNTIME); // no flags = runtime
        assert_eq!(
            sedona_flags_to_slot_flags("o"),
            SLOT_FLAG_OPERATOR | SLOT_FLAG_RUNTIME
        );
    }

    #[test]
    fn manifest_db_default_value_parsing() {
        let int_type = SoxValueType::Int as u8;
        let float_type = SoxValueType::Float as u8;
        let bool_type = SoxValueType::Bool as u8;
        let buf_type = SoxValueType::Buf as u8;

        assert_eq!(
            parse_default_value(int_type, Some("42")),
            SlotValue::Int(42)
        );
        assert_eq!(
            parse_default_value(float_type, Some("3.14")),
            SlotValue::Float(3.14)
        );
        assert_eq!(
            parse_default_value(bool_type, Some("true")),
            SlotValue::Bool(true)
        );
        assert_eq!(
            parse_default_value(bool_type, Some("false")),
            SlotValue::Bool(false)
        );
        assert_eq!(
            parse_default_value(buf_type, Some("hello")),
            SlotValue::Str("hello".into())
        );
        assert_eq!(
            parse_default_value(buf_type, Some("")),
            SlotValue::Str(String::new())
        );
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
        payload.push(2); // kitId=2 (control)
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
        payload.push(2); // kitId=2 (control)
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
        assert!(
            db.type_count() > 20,
            "Expected >20 types, got {}",
            db.type_count()
        );

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
        let req = SoxRequest {
            cmd: SoxCmd::Link,
            req_id: 10,
            payload,
        };
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
        let req = SoxRequest {
            cmd: SoxCmd::Link,
            req_id: 11,
            payload,
        };
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
        let req = SoxRequest {
            cmd: SoxCmd::Link,
            req_id: 12,
            payload,
        };
        let resp = handle_link(&req, &mut tree);
        assert_eq!(resp.cmd, b'!'); // error
    }

    /// Regression: when a target slot already has an incoming link, a new
    /// link from a different source onto the same slot should REPLACE the
    /// old one (DDC single-source semantics). Previously both links
    /// stayed in the tree and the last-inserted source won via insertion
    /// order; deleting it would let the older link's value resurface
    /// in a way that surprised editor users.
    #[test]
    fn add_link_replaces_existing_link_on_same_target_slot() {
        let mut tree = ComponentTree::new();
        // Three components, parents=NO_PARENT for simplicity.
        tree.add(make_source_comp(200, NO_PARENT, "srcA", 70.0));
        tree.add(make_source_comp(201, NO_PARENT, "srcB", 50.0));
        tree.add(make_math_comp(202, NO_PARENT, "tgt", 2, 3));

        // First link: A.out (slot 1) → tgt.in1 (slot 2).
        assert!(tree.add_link(200, 1, 202, 2));
        assert_eq!(tree.get(202).unwrap().links.len(), 1);
        assert_eq!(tree.get(200).unwrap().links.len(), 1);

        // Second link to the SAME target slot from a different source
        // should replace the first link, not coexist.
        assert!(tree.add_link(201, 1, 202, 2));

        let tgt = tree.get(202).unwrap();
        assert_eq!(
            tgt.links.len(),
            1,
            "target should have exactly one incoming link"
        );
        assert_eq!(tgt.links[0].from_comp, 201, "new link should win");

        // The old source must no longer carry a stale outgoing link
        // record either (links live on both endpoints).
        let old_src = tree.get(200).unwrap();
        assert_eq!(
            old_src.links.len(),
            0,
            "old source's outgoing link must be cleaned"
        );
    }

    /// Adding a link from the SAME source twice (truly duplicate) is
    /// still rejected — it's a no-op idempotency guard, not a replace.
    #[test]
    fn add_link_idempotent_when_exact_same_link_exists() {
        let mut tree = ComponentTree::new();
        tree.add(make_source_comp(200, NO_PARENT, "src", 10.0));
        tree.add(make_math_comp(201, NO_PARENT, "tgt", 2, 3));

        assert!(tree.add_link(200, 1, 201, 2));
        // Same exact link → false (no-op).
        assert!(!tree.add_link(200, 1, 201, 2));
        assert_eq!(tree.get(201).unwrap().links.len(), 1);
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
        let req = SoxRequest {
            cmd: SoxCmd::Link,
            req_id: 13,
            payload,
        };
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
        let req = SoxRequest {
            cmd: SoxCmd::Link,
            req_id: 14,
            payload,
        };
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
        let req = SoxRequest {
            cmd: SoxCmd::ReadComp,
            req_id: 15,
            payload,
        };
        let resp = handle_read_comp(&req, &tree, None);
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
        assert!(
            original_children.len() >= 2,
            "need at least 2 children to reorder"
        );
        // Reverse the children order
        let mut reversed = original_children.clone();
        reversed.reverse();
        let mut payload = Vec::new();
        payload.extend_from_slice(&5u16.to_be_bytes()); // parentId
        payload.push(reversed.len() as u8); // count
        for &id in &reversed {
            payload.extend_from_slice(&id.to_be_bytes());
        }
        let req = SoxRequest {
            cmd: SoxCmd::Reorder,
            req_id: 20,
            payload,
        };
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
        let req = SoxRequest {
            cmd: SoxCmd::Reorder,
            req_id: 21,
            payload,
        };
        let resp = handle_reorder(&req, &mut tree);
        assert_eq!(resp.cmd, b'!');
    }

    #[test]
    fn handle_reorder_nonexistent_parent_fails() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        let mut payload = Vec::new();
        payload.extend_from_slice(&999u16.to_be_bytes()); // nonexistent parent
        payload.push(0); // count=0
        let req = SoxRequest {
            cmd: SoxCmd::Reorder,
            req_id: 22,
            payload,
        };
        let resp = handle_reorder(&req, &mut tree);
        assert_eq!(resp.cmd, b'!');
    }

    #[test]
    fn get_links_returns_both_directions() {
        let mut tree = ComponentTree::from_channels(&sample_channels());
        // Use non-cyclic links: 100→101 and 102→100
        // (102 has no path to 100 via outgoing links, so 102→100 is not a cycle)
        tree.add_link(100, 6, 101, 1);
        tree.add_link(102, 6, 100, 2);
        // Links for comp 100: one as source (from=100→101), one as destination (102→to=100)
        let links = tree.get_links(100);
        assert_eq!(links.len(), 2);
        // Links for comp 101: one as destination (to=101)
        let links = tree.get_links(101);
        assert_eq!(links.len(), 1);
        // Links for comp 102: one as source (from=102→100)
        let links = tree.get_links(102);
        assert_eq!(links.len(), 1);
    }

    // ---- execute_links tests ----

    /// Helper: create an Add2 component (kit_id=2, type_id=3) with standard slots:
    /// slot 0 = meta (Int), slot 1 = out (Float), slot 2 = in1 (Float), slot 3 = in2 (Float)
    fn make_math_comp(
        comp_id: u16,
        parent_id: u16,
        name: &str,
        kit_id: u8,
        type_id: u8,
    ) -> VirtualComponent {
        VirtualComponent {
            comp_id,
            parent_id,
            name: name.into(),
            type_name: format!("math::{name}"),
            kit_id,
            type_id,
            children: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(0.0),
                },
                VirtualSlot {
                    name: "in1".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(0.0),
                },
                VirtualSlot {
                    name: "in2".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(0.0),
                },
            ],
            links: Vec::new(),
        }
    }

    /// Helper: create a simple component with a single float output slot at index 1
    fn make_source_comp(
        comp_id: u16,
        parent_id: u16,
        name: &str,
        out_value: f32,
    ) -> VirtualComponent {
        VirtualComponent {
            comp_id,
            parent_id,
            name: name.into(),
            type_name: "func::ConstFloat".into(),
            kit_id: 2,
            type_id: 255, // arbitrary non-executable type
            children: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(out_value),
                },
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
        div.slots.push(VirtualSlot {
            name: "div0".into(),
            type_id: 0,
            flags: 0,
            value: SlotValue::Bool(false),
        });
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
        div.slots.push(VirtualSlot {
            name: "div0".into(),
            type_id: 0,
            flags: 0,
            value: SlotValue::Bool(false),
        });
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

    /// Regression: production tree had WriteFloat slot order
    /// `meta=0, in=1, out=2` (from the editor's manifest), while the
    /// hardcoded handler expected `meta=0, out=1, in=2`. The handler
    /// would read from index 2 (interpreting it as `in`, but it was
    /// actually `out=0`) and write that 0 back to index 1 (`in`).
    /// Combined with `execute_links` setting index 1 from a linked
    /// source on every tick, this collapsed into a per-tick reset
    /// loop where the linked input never landed at the target.
    /// Now slots are looked up by name.
    #[test]
    fn write_float_passthrough_works_with_inverted_slot_order() {
        let mut tree = ComponentTree::new();
        // Slot order matching production manifest: meta=0, in=1, out=2.
        let comp = VirtualComponent {
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "wf".into(),
            type_name: "control::WriteFloat".into(),
            kit_id: 2,
            type_id: 57,
            children: vec![],
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: SoxValueType::Int as u8,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "in".into(),
                    type_id: SoxValueType::Float as u8,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(17.5),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: SoxValueType::Float as u8,
                    flags: 4,
                    value: SlotValue::Float(0.0),
                },
            ],
            links: vec![],
        };
        tree.add(comp);

        let changed = tree.execute_components();
        assert!(
            changed.contains(&200),
            "WriteFloat passthrough must mark out as changed"
        );

        let comp = tree.get(200).unwrap();
        // out (slot 2 in this layout) should now equal in (17.5).
        match &comp.slots[2].value {
            SlotValue::Float(v) => assert_eq!(*v, 17.5, "out should pick up in"),
            other => panic!("expected Float, got {:?}", other),
        }
        // in (slot 1) must NOT be clobbered by the handler reading the
        // wrong index and writing 0 back. This was the production bug.
        match &comp.slots[1].value {
            SlotValue::Float(v) => assert_eq!(*v, 17.5, "in must not be reset"),
            other => panic!("expected Float, got {:?}", other),
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
    fn make_math_comp_n(
        comp_id: u16,
        parent_id: u16,
        name: &str,
        kit_id: u8,
        type_id: u8,
        num_inputs: usize,
    ) -> VirtualComponent {
        let mut slots = vec![
            VirtualSlot {
                name: "meta".into(),
                type_id: 1,
                flags: 0,
                value: SlotValue::Int(0),
            },
            VirtualSlot {
                name: "out".into(),
                type_id: 4,
                flags: 0,
                value: SlotValue::Float(0.0),
            },
        ];
        for i in 0..num_inputs {
            slots.push(VirtualSlot {
                name: format!("in{}", i + 1),
                type_id: 4,
                flags: 0,
                value: SlotValue::Float(0.0),
            });
        }
        VirtualComponent {
            comp_id,
            parent_id,
            name: name.into(),
            type_name: format!("math::{name}"),
            kit_id,
            type_id,
            children: Vec::new(),
            slots,
            links: Vec::new(),
        }
    }

    /// Helper: create a bool-logic component with N bool input slots
    fn make_bool_comp(
        comp_id: u16,
        name: &str,
        kit_id: u8,
        type_id: u8,
        num_inputs: usize,
    ) -> VirtualComponent {
        let mut slots = vec![
            VirtualSlot {
                name: "meta".into(),
                type_id: 1,
                flags: 0,
                value: SlotValue::Int(0),
            },
            VirtualSlot {
                name: "out".into(),
                type_id: 0,
                flags: 0,
                value: SlotValue::Bool(false),
            },
        ];
        for i in 0..num_inputs {
            slots.push(VirtualSlot {
                name: format!("in{}", i + 1),
                type_id: 0,
                flags: 0,
                value: SlotValue::Bool(false),
            });
        }
        VirtualComponent {
            comp_id,
            parent_id: NO_PARENT,
            name: name.into(),
            type_name: format!("logic::{name}"),
            kit_id,
            type_id,
            children: Vec::new(),
            slots,
            links: Vec::new(),
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
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Float(10.0)
        );
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
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Float(40.0)
        );
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
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Float(120.0)
        );
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
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Float(-42.5)
        );
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
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Float(15.5)
        );
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
        c.slots[3].value = SlotValue::Float(0.0); // lowLmt
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
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Float(100.0)
        );
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
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Float(50.0)
        );
    }

    #[test]
    fn execute_round() {
        let mut tree = ComponentTree::new();
        let mut c = make_math_comp_n(200, NO_PARENT, "rnd", 2, 47, 1);
        // Add decimalPlaces config slot at index 3
        c.slots.push(VirtualSlot {
            name: "decimalPlaces".into(),
            type_id: 1,
            flags: 0,
            value: SlotValue::Int(2),
        });
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
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Bool(false)
        );
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
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Bool(false)
        );
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
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Bool(false)
        );
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
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Bool(false)
        );
    }

    // ---- Comparator test ----

    #[test]
    fn execute_cmpr() {
        let mut tree = ComponentTree::new();
        // Cmpr: meta=0, xgy=1, xey=2, xly=3, x=4, y=5
        let c = VirtualComponent {
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "cmpr".into(),
            type_name: "math::Cmpr".into(),
            kit_id: 2,
            type_id: 12,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "xgy".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "xey".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "xly".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "x".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(10.0),
                },
                VirtualSlot {
                    name: "y".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(5.0),
                },
            ],
        };
        tree.add(c);
        tree.execute_components();
        let comp = tree.get(200).unwrap();
        assert_eq!(comp.slots[1].value, SlotValue::Bool(true), "x > y");
        assert_eq!(comp.slots[2].value, SlotValue::Bool(false), "x == y");
        assert_eq!(comp.slots[3].value, SlotValue::Bool(false), "x < y");
    }

    #[test]
    fn execute_cmpr_equal() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "cmpr".into(),
            type_name: "math::Cmpr".into(),
            kit_id: 2,
            type_id: 12,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "xgy".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "xey".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "xly".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "x".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(7.0),
                },
                VirtualSlot {
                    name: "y".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(7.0),
                },
            ],
        };
        tree.add(c);
        tree.execute_components();
        let comp = tree.get(200).unwrap();
        assert_eq!(comp.slots[1].value, SlotValue::Bool(false), "x > y");
        assert_eq!(comp.slots[2].value, SlotValue::Bool(true), "x == y");
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
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "f2i".into(),
            type_name: "math::F2I".into(),
            kit_id: 2,
            type_id: 22,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "in".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(42.7),
                },
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
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "i2f".into(),
            type_name: "math::I2F".into(),
            kit_id: 2,
            type_id: 26,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(0.0),
                },
                VirtualSlot {
                    name: "in".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(99),
                },
            ],
        };
        tree.add(c);
        tree.execute_components();
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Float(99.0)
        );
    }

    // ---- Multiplexer tests ----

    #[test]
    fn execute_asw_sel_false() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "asw".into(),
            type_name: "math::ASW".into(),
            kit_id: 2,
            type_id: 1,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(0.0),
                },
                VirtualSlot {
                    name: "sel".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "in1".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(10.0),
                },
                VirtualSlot {
                    name: "in2".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(20.0),
                },
            ],
        };
        tree.add(c);
        tree.execute_components();
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Float(10.0)
        );
    }

    #[test]
    fn execute_asw_sel_true() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "asw".into(),
            type_name: "math::ASW".into(),
            kit_id: 2,
            type_id: 1,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(0.0),
                },
                VirtualSlot {
                    name: "sel".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(true),
                },
                VirtualSlot {
                    name: "in1".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(10.0),
                },
                VirtualSlot {
                    name: "in2".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(20.0),
                },
            ],
        };
        tree.add(c);
        tree.execute_components();
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Float(20.0)
        );
    }

    #[test]
    fn execute_bsw() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "bsw".into(),
            type_name: "logic::BSW".into(),
            kit_id: 2,
            type_id: 11,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "sel".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(true),
                },
                VirtualSlot {
                    name: "in1".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "in2".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(true),
                },
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
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "isw".into(),
            type_name: "math::ISW".into(),
            kit_id: 2,
            type_id: 28,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "sel".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "in1".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(42),
                },
                VirtualSlot {
                    name: "in2".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(99),
                },
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
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "hyst".into(),
            type_name: "control::Hysteresis".into(),
            kit_id: 2,
            type_id: 25,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "in".into(),
                    type_id: 2,
                    flags: 0,
                    value: SlotValue::Float(75.0),
                },
                VirtualSlot {
                    name: "rising".into(),
                    type_id: 2,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(72.0),
                },
                VirtualSlot {
                    name: "falling".into(),
                    type_id: 2,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(68.0),
                },
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
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "hyst".into(),
            type_name: "control::Hysteresis".into(),
            kit_id: 2,
            type_id: 25,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(true),
                },
                VirtualSlot {
                    name: "in".into(),
                    type_id: 2,
                    flags: 0,
                    value: SlotValue::Float(70.0),
                },
                VirtualSlot {
                    name: "rising".into(),
                    type_id: 2,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(72.0),
                },
                VirtualSlot {
                    name: "falling".into(),
                    type_id: 2,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(68.0),
                },
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
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "hyst".into(),
            type_name: "control::Hysteresis".into(),
            kit_id: 2,
            type_id: 25,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(true),
                },
                VirtualSlot {
                    name: "in".into(),
                    type_id: 2,
                    flags: 0,
                    value: SlotValue::Float(65.0),
                },
                VirtualSlot {
                    name: "rising".into(),
                    type_id: 2,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(72.0),
                },
                VirtualSlot {
                    name: "falling".into(),
                    type_id: 2,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(68.0),
                },
            ],
        };
        tree.add(c);
        tree.execute_components();
        // in=65, out=true, 65 < falling(68) → switches to false
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Bool(false)
        );
    }

    #[test]
    fn execute_sr_latch_set() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "sr".into(),
            type_name: "control::SRLatch".into(),
            kit_id: 2,
            type_id: 48,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "set".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(true),
                },
                VirtualSlot {
                    name: "reset".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
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
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "sr".into(),
            type_name: "control::SRLatch".into(),
            kit_id: 2,
            type_id: 48,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(true),
                },
                VirtualSlot {
                    name: "set".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(true),
                },
                VirtualSlot {
                    name: "reset".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(true),
                },
            ],
        };
        tree.add(c);
        tree.execute_components();
        // reset wins over set
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Bool(false)
        );
    }

    #[test]
    fn execute_reset_remap() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "rst".into(),
            type_name: "control::Reset".into(),
            kit_id: 2,
            type_id: 46,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 2,
                    flags: 0,
                    value: SlotValue::Float(0.0),
                },
                VirtualSlot {
                    name: "in".into(),
                    type_id: 2,
                    flags: 0,
                    value: SlotValue::Float(50.0),
                },
                VirtualSlot {
                    name: "inLow".into(),
                    type_id: 2,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(0.0),
                },
                VirtualSlot {
                    name: "inHigh".into(),
                    type_id: 2,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(100.0),
                },
                VirtualSlot {
                    name: "outLow".into(),
                    type_id: 2,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(55.0),
                },
                VirtualSlot {
                    name: "outHigh".into(),
                    type_id: 2,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(85.0),
                },
            ],
        };
        tree.add(c);
        tree.execute_components();
        // 50% of [0,100] → 50% of [55,85] = 70.0
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Float(70.0)
        );
    }

    #[test]
    fn execute_write_float_passthrough() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "wf".into(),
            type_name: "control::WriteFloat".into(),
            kit_id: 2,
            type_id: 57,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 2,
                    flags: 0,
                    value: SlotValue::Float(0.0),
                },
                VirtualSlot {
                    name: "in".into(),
                    type_id: 2,
                    flags: 0,
                    value: SlotValue::Float(42.5),
                },
            ],
        };
        tree.add(c);
        tree.execute_components();
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Float(42.5)
        );
    }

    #[test]
    fn execute_write_bool_passthrough() {
        let mut tree = ComponentTree::new();
        let c = VirtualComponent {
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "wb".into(),
            type_name: "control::WriteBool".into(),
            kit_id: 2,
            type_id: 56,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "in".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(true),
                },
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
            comp_id: 200,
            parent_id: NO_PARENT,
            name: "seq".into(),
            type_name: "control::LSeq".into(),
            kit_id: 2,
            type_id: 31,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "in".into(),
                    type_id: 2,
                    flags: 0,
                    value: SlotValue::Float(0.6),
                },
                VirtualSlot {
                    name: "numStages".into(),
                    type_id: 1,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Int(4),
                },
            ],
        };
        tree.add(c);
        tree.execute_components();
        // 0.6 * 4 = 2.4, floor = 2
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Int(2));
    }

    // ---- Stateful component tests ----

    /// Helper: create a DlyOn component (kit_id=2, type_id=20)
    fn make_dlyon(comp_id: u16, input: bool, delay: f32) -> VirtualComponent {
        VirtualComponent {
            comp_id,
            parent_id: NO_PARENT,
            name: "dlyon".into(),
            type_name: "control::DlyOn".into(),
            kit_id: 2,
            type_id: 20,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "in".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(input),
                },
                VirtualSlot {
                    name: "delay".into(),
                    type_id: 4,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(delay),
                },
            ],
        }
    }

    /// Helper: create a DlyOff component (kit_id=2, type_id=19)
    fn make_dlyoff(comp_id: u16, input: bool, delay: f32) -> VirtualComponent {
        VirtualComponent {
            comp_id,
            parent_id: NO_PARENT,
            name: "dlyoff".into(),
            type_name: "control::DlyOff".into(),
            kit_id: 2,
            type_id: 19,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "in".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(input),
                },
                VirtualSlot {
                    name: "delay".into(),
                    type_id: 4,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(delay),
                },
            ],
        }
    }

    /// Helper: create a Count component (kit_id=2, type_id=16)
    fn make_count(comp_id: u16, input: bool, preset: i32) -> VirtualComponent {
        VirtualComponent {
            comp_id,
            parent_id: NO_PARENT,
            name: "count".into(),
            type_name: "control::Count".into(),
            kit_id: 2,
            type_id: 16,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "in".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(input),
                },
                VirtualSlot {
                    name: "preset".into(),
                    type_id: 1,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Int(preset),
                },
            ],
        }
    }

    /// Helper: create a Ramp component (kit_id=2, type_id=44)
    fn make_ramp(comp_id: u16, min: f32, max: f32, step: f32) -> VirtualComponent {
        VirtualComponent {
            comp_id,
            parent_id: NO_PARENT,
            name: "ramp".into(),
            type_name: "control::Ramp".into(),
            kit_id: 2,
            type_id: 44,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(min),
                },
                VirtualSlot {
                    name: "min".into(),
                    type_id: 4,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(min),
                },
                VirtualSlot {
                    name: "max".into(),
                    type_id: 4,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(max),
                },
                VirtualSlot {
                    name: "step".into(),
                    type_id: 4,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(step),
                },
            ],
        }
    }

    /// Helper: create a Tstat component (kit_id=2, type_id=54)
    /// Real manifest: meta=0, diff=1(deadband), isHeating=2, sp=3, cv=4, out=5, raise=6, lower=7
    fn make_tstat(comp_id: u16, sp: f32, cv: f32, diff: f32) -> VirtualComponent {
        VirtualComponent {
            comp_id,
            parent_id: NO_PARENT,
            name: "tstat".into(),
            type_name: "control::Tstat".into(),
            kit_id: 2,
            type_id: 54,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "diff".into(),
                    type_id: 4,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(diff),
                },
                VirtualSlot {
                    name: "isHeating".into(),
                    type_id: 0,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Bool(true),
                },
                VirtualSlot {
                    name: "sp".into(),
                    type_id: 4,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(sp),
                },
                VirtualSlot {
                    name: "cv".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(cv),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "raise".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "lower".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(false),
                },
            ],
        }
    }

    /// Helper: create an UpDn component (kit_id=2, type_id=55)
    fn make_updn(
        comp_id: u16,
        out: f32,
        up: bool,
        dn: bool,
        step: f32,
        min: f32,
        max: f32,
    ) -> VirtualComponent {
        VirtualComponent {
            comp_id,
            parent_id: NO_PARENT,
            name: "updn".into(),
            type_name: "control::UpDn".into(),
            kit_id: 2,
            type_id: 55,
            children: Vec::new(),
            links: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(out),
                },
                VirtualSlot {
                    name: "up".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(up),
                },
                VirtualSlot {
                    name: "dn".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Bool(dn),
                },
                VirtualSlot {
                    name: "step".into(),
                    type_id: 4,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(step),
                },
                VirtualSlot {
                    name: "min".into(),
                    type_id: 4,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(min),
                },
                VirtualSlot {
                    name: "max".into(),
                    type_id: 4,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Float(max),
                },
            ],
        }
    }

    // --- DlyOn tests ---

    #[test]
    fn dlyon_stays_false_before_delay() {
        let mut tree = ComponentTree::new();
        tree.add(make_dlyon(200, true, 3.0)); // delay=3 seconds
                                              // Tick 1: counter=1, delay=3 -> out=false
        tree.execute_components();
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Bool(false)
        );
        // Tick 2: counter=2 -> still false
        tree.execute_components();
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Bool(false)
        );
    }

    #[test]
    fn dlyon_goes_true_after_delay() {
        let mut tree = ComponentTree::new();
        tree.add(make_dlyon(200, true, 3.0));
        tree.execute_components(); // tick 1
        tree.execute_components(); // tick 2
        tree.execute_components(); // tick 3: counter=3 >= delay=3 -> out=true
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(true));
    }

    #[test]
    fn dlyon_resets_when_input_goes_false() {
        let mut tree = ComponentTree::new();
        tree.add(make_dlyon(200, true, 3.0));
        tree.execute_components(); // tick 1
        tree.execute_components(); // tick 2
                                   // Now set input to false
        tree.get_mut(200).unwrap().slots[2].value = SlotValue::Bool(false);
        tree.execute_components();
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Bool(false)
        );
        // Set back to true — counter should have reset, so need 3 more ticks
        tree.get_mut(200).unwrap().slots[2].value = SlotValue::Bool(true);
        tree.execute_components(); // tick 1 after reset
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Bool(false)
        );
    }

    #[test]
    fn dlyon_input_false_means_out_false() {
        let mut tree = ComponentTree::new();
        tree.add(make_dlyon(200, false, 1.0));
        tree.execute_components();
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Bool(false)
        );
    }

    // --- DlyOff tests ---

    #[test]
    fn dlyoff_stays_true_during_delay() {
        let mut tree = ComponentTree::new();
        // Start with input=true, then switch to false
        tree.add(make_dlyoff(200, true, 3.0));
        tree.execute_components(); // in=true -> out=true, counter reset
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(true));
        // Switch input to false
        tree.get_mut(200).unwrap().slots[2].value = SlotValue::Bool(false);
        tree.execute_components(); // tick 1: counter=1 < 3 -> out=true still
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(true));
        tree.execute_components(); // tick 2: counter=2 < 3 -> out=true still
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(true));
    }

    #[test]
    fn dlyoff_goes_false_after_delay() {
        let mut tree = ComponentTree::new();
        tree.add(make_dlyoff(200, false, 3.0));
        tree.execute_components(); // tick 1
        tree.execute_components(); // tick 2
        tree.execute_components(); // tick 3: counter=3 >= 3 -> out=false
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Bool(false)
        );
    }

    #[test]
    fn dlyoff_resets_when_input_goes_true() {
        let mut tree = ComponentTree::new();
        tree.add(make_dlyoff(200, false, 3.0));
        tree.execute_components(); // tick 1 with in=false
        tree.execute_components(); // tick 2
                                   // Set input back to true
        tree.get_mut(200).unwrap().slots[2].value = SlotValue::Bool(true);
        tree.execute_components(); // in=true -> out=true, counter reset
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Bool(true));
    }

    // --- Count tests ---

    #[test]
    fn count_increments_on_rising_edge() {
        let mut tree = ComponentTree::new();
        tree.add(make_count(200, false, 0)); // start with in=false, no preset
        tree.execute_components(); // no edge yet
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Int(0));
        // Rising edge: false -> true
        tree.get_mut(200).unwrap().slots[2].value = SlotValue::Bool(true);
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Int(1));
        // Staying true: no edge, no increment
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Int(1));
    }

    #[test]
    fn count_multiple_edges() {
        let mut tree = ComponentTree::new();
        tree.add(make_count(200, false, 0));
        for _ in 0..5 {
            tree.get_mut(200).unwrap().slots[2].value = SlotValue::Bool(true);
            tree.execute_components(); // rising edge
            tree.get_mut(200).unwrap().slots[2].value = SlotValue::Bool(false);
            tree.execute_components(); // falling edge (no count)
        }
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Int(5));
    }

    #[test]
    fn count_resets_at_preset() {
        let mut tree = ComponentTree::new();
        tree.add(make_count(200, false, 3)); // preset=3
        for _ in 0..3 {
            tree.get_mut(200).unwrap().slots[2].value = SlotValue::Bool(true);
            tree.execute_components();
            tree.get_mut(200).unwrap().slots[2].value = SlotValue::Bool(false);
            tree.execute_components();
        }
        // After 3 rising edges with preset=3, should reset to 0
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Int(0));
    }

    // --- Ramp tests ---

    #[test]
    fn ramp_goes_up_then_reverses() {
        let mut tree = ComponentTree::new();
        tree.add(make_ramp(200, 0.0, 10.0, 3.0)); // min=0, max=10, step=3
                                                  // Start at 0.0, direction=up
        tree.execute_components(); // 0 + 3 = 3.0
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(3.0));
        tree.execute_components(); // 3 + 3 = 6.0
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(6.0));
        tree.execute_components(); // 6 + 3 = 9.0
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(9.0));
        tree.execute_components(); // 9 + 3 = 12.0 >= 10 -> clamped to 10.0, direction reverses
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Float(10.0)
        );
        tree.execute_components(); // 10 - 3 = 7.0 (now going down)
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(7.0));
    }

    #[test]
    fn ramp_reverses_at_min() {
        let mut tree = ComponentTree::new();
        // Start at min, going down initially won't happen since direction defaults to up
        // Let's set it up already going down by first reaching the top
        tree.add(make_ramp(200, 0.0, 5.0, 6.0)); // step=6, overshoots max immediately
        tree.execute_components(); // 0 + 6 = 6 >= 5 -> clamp to 5, reverse to down
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(5.0));
        tree.execute_components(); // 5 - 6 = -1 <= 0 -> clamp to 0, reverse to up
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(0.0));
        tree.execute_components(); // 0 + 6 = 6 >= 5 -> clamp to 5, reverse
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(5.0));
    }

    // --- Tstat tests ---
    // Real manifest: meta=0, diff=1(deadband), isHeating=2, sp=3, cv=4, out=5, raise=6, lower=7

    #[test]
    fn tstat_heating_when_cold() {
        let mut tree = ComponentTree::new();
        // isHeating=true, sp=72, cv=65, diff=4 -> half=2 -> cv < 72-2=70 -> out=true (need heat)
        tree.add(make_tstat(200, 72.0, 65.0, 4.0));
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[5].value, SlotValue::Bool(true)); // out=on
        assert_eq!(tree.get(200).unwrap().slots[6].value, SlotValue::Bool(true)); // raise
        assert_eq!(
            tree.get(200).unwrap().slots[7].value,
            SlotValue::Bool(false)
        ); // not lower
    }

    #[test]
    fn tstat_cooling_when_hot() {
        let mut tree = ComponentTree::new();
        // isHeating=true, sp=72, cv=80, diff=4 -> half=2 -> cv > 72+2=74 -> out=false (warm enough)
        tree.add(make_tstat(200, 72.0, 80.0, 4.0));
        tree.execute_components();
        assert_eq!(
            tree.get(200).unwrap().slots[5].value,
            SlotValue::Bool(false)
        ); // out=off
        assert_eq!(tree.get(200).unwrap().slots[7].value, SlotValue::Bool(true));
        // lower
    }

    #[test]
    fn tstat_deadband_holds_state() {
        let mut tree = ComponentTree::new();
        // isHeating=true, sp=72, cv=71, diff=4 -> half=2 -> 70 <= cv <= 74 -> deadband
        tree.add(make_tstat(200, 72.0, 71.0, 4.0));
        tree.execute_components();
        // Within deadband, keeps initial state (out=false)
        assert_eq!(
            tree.get(200).unwrap().slots[5].value,
            SlotValue::Bool(false)
        );
    }

    #[test]
    fn tstat_heating_stays_in_deadband() {
        let mut tree = ComponentTree::new();
        // Start cold to trigger heating
        tree.add(make_tstat(200, 72.0, 65.0, 4.0));
        tree.execute_components(); // out=true
        assert_eq!(tree.get(200).unwrap().slots[5].value, SlotValue::Bool(true));
        // Now warm up into deadband zone: pv=71 (within 70-74)
        tree.get_mut(200).unwrap().slots[4].value = SlotValue::Float(71.0);
        tree.execute_components();
        // Should keep out=true (deadband holds)
        assert_eq!(tree.get(200).unwrap().slots[5].value, SlotValue::Bool(true));
    }

    // --- UpDn tests ---

    #[test]
    fn updn_increments_on_up() {
        let mut tree = ComponentTree::new();
        tree.add(make_updn(200, 5.0, true, false, 1.0, 0.0, 10.0));
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(6.0));
    }

    #[test]
    fn updn_decrements_on_dn() {
        let mut tree = ComponentTree::new();
        tree.add(make_updn(200, 5.0, false, true, 1.0, 0.0, 10.0));
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(4.0));
    }

    #[test]
    fn updn_clamps_to_max() {
        let mut tree = ComponentTree::new();
        tree.add(make_updn(200, 9.5, true, false, 1.0, 0.0, 10.0));
        tree.execute_components();
        assert_eq!(
            tree.get(200).unwrap().slots[1].value,
            SlotValue::Float(10.0)
        );
    }

    #[test]
    fn updn_clamps_to_min() {
        let mut tree = ComponentTree::new();
        tree.add(make_updn(200, 0.5, false, true, 1.0, 0.0, 10.0));
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(0.0));
    }

    #[test]
    fn updn_both_up_and_dn_cancel_out() {
        let mut tree = ComponentTree::new();
        // up and dn both true with same step -> net zero change
        tree.add(make_updn(200, 5.0, true, true, 1.0, 0.0, 10.0));
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(5.0));
    }

    #[test]
    fn updn_no_change_when_neither() {
        let mut tree = ComponentTree::new();
        tree.add(make_updn(200, 5.0, false, false, 1.0, 0.0, 10.0));
        tree.execute_components();
        assert_eq!(tree.get(200).unwrap().slots[1].value, SlotValue::Float(5.0));
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
        tree.add(make_math_comp(204, NO_PARENT, "add", 2, 3)); // Add2

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

    // ---- File transfer tests ----

    /// Helper: build a fileOpen request payload with given method, uri, fileSize, chunkSize
    fn build_file_open_payload(
        method: &str,
        uri: &str,
        file_size: u32,
        chunk_size: u16,
    ) -> Vec<u8> {
        let mut payload = Vec::new();
        // str method (null-terminated)
        payload.extend_from_slice(method.as_bytes());
        payload.push(0x00);
        // str uri (null-terminated)
        payload.extend_from_slice(uri.as_bytes());
        payload.push(0x00);
        // i4 fileSize
        payload.extend_from_slice(&file_size.to_be_bytes());
        // u2 chunkSize
        payload.extend_from_slice(&chunk_size.to_be_bytes());
        // end of headers
        payload.push(0x00);
        payload
    }

    /// Helper: build a fileWrite chunk payload: u2(chunkNum) + u2(chunkSize) + data
    fn build_file_write_chunk_payload(chunk_num: u16, data: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&chunk_num.to_be_bytes());
        payload.extend_from_slice(&(data.len() as u16).to_be_bytes());
        payload.extend_from_slice(data);
        payload
    }

    /// Serialization lock for file transfer tests that share global SOX_FILE_XFER state.
    static FILE_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Session that owns the transfers in these tests; `OTHER` is a second client.
    const OWNER: u16 = 1;
    const OTHER: u16 = 2;

    /// Helper: acquire the serialization lock and clear file transfer state.
    /// Hold the returned guard for the duration of the test.
    fn lock_file_xfer() -> std::sync::MutexGuard<'static, ()> {
        let guard = FILE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_file_xfer();
        guard
    }

    /// Helper: clear the global file transfer state (use within a locked test).
    fn clear_file_xfer() {
        let mut xfer = SOX_FILE_XFER.lock().unwrap_or_else(|e| e.into_inner());
        *xfer = None;
    }

    fn set_xfer(x: SoxFileXfer) {
        *SOX_FILE_XFER.lock().unwrap_or_else(|e| e.into_inner()) = Some(x);
    }

    fn with_xfer<R>(f: impl FnOnce(Option<&SoxFileXfer>) -> R) -> R {
        let xfer = SOX_FILE_XFER.lock().unwrap_or_else(|e| e.into_inner());
        f(xfer.as_ref())
    }

    fn req(cmd: SoxCmd, payload: Vec<u8>) -> SoxRequest {
        SoxRequest {
            cmd,
            req_id: 7,
            payload,
        }
    }

    /// Error text of a `!` response.
    fn err_text(resp: &SoxResponse) -> String {
        assert_eq!(resp.cmd, b'!', "expected an error response");
        let end = resp
            .payload
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(resp.payload.len());
        String::from_utf8_lossy(&resp.payload[..end]).into_owned()
    }

    /// A unique path in the OS temp dir (allowed for writes on every platform).
    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("sandstar_s45_{}_{name}", std::process::id()))
    }

    fn close(session: u16) -> SoxResponse {
        handle_file_close(&req(SoxCmd::FileClose, vec![]), session)
    }

    fn write(session: u16, chunk_num: u16, data: &[u8]) -> SoxResponse {
        let payload = build_file_write_chunk_payload(chunk_num, data);
        handle_file_write(&req(SoxCmd::FileWrite, payload), session)
    }

    #[test]
    fn file_open_put_bad_method() {
        let _guard = lock_file_xfer();
        let payload = build_file_open_payload("x", "/tmp/test.txt", 10, 256);
        let resp = handle_file_open(&req(SoxCmd::FileOpen, payload), OWNER);
        assert_eq!(resp.cmd, b'!', "expected error for bad method");
    }

    #[test]
    fn file_open_put_path_traversal_rejected() {
        let _guard = lock_file_xfer();
        let payload = build_file_open_payload("p", "/tmp/../etc/passwd", 10, 256);
        let resp = handle_file_open(&req(SoxCmd::FileOpen, payload), OWNER);
        assert_eq!(resp.cmd, b'!', "expected error for path traversal");
    }

    #[test]
    fn file_open_put_null_byte_is_a_string_end() {
        let _guard = lock_file_xfer();
        // read_str stops at the NUL: "evil" becomes the next field and the
        // size bytes no longer line up, so the request is refused.
        let payload = build_file_open_payload("p", "/tmp/test\0evil", 10, 256);
        let resp = handle_file_open(&req(SoxCmd::FileOpen, payload), OWNER);
        assert_eq!(resp.cmd, b'!');
        with_xfer(|x| assert!(x.is_none()));
        assert!(validate_path("/tmp/a\0b", PathUse::Write).is_err());
    }

    #[test]
    fn file_open_put_too_big_rejected() {
        let _guard = lock_file_xfer();
        // 11MB exceeds the 10MB limit
        let payload = build_file_open_payload("p", "/tmp/bigfile", 11 * 1024 * 1024, 256);
        let resp = handle_file_open(&req(SoxCmd::FileOpen, payload), OWNER);
        assert_eq!(err_text(&resp), "too big");
    }

    #[test]
    fn file_open_put_with_offset_rejected() {
        let _guard = lock_file_xfer();
        let path = temp_path("offset.bin");
        let mut payload = build_file_open_payload("p", &path.to_string_lossy(), 4, 4);
        payload.pop(); // drop end of headers
        payload.extend_from_slice(b"offset\0");
        payload.extend_from_slice(format!("{}\0", usize::MAX).as_bytes());
        payload.push(0x00);
        let resp = handle_file_open(&req(SoxCmd::FileOpen, payload), OWNER);
        assert_eq!(err_text(&resp), "offset not supported");
        with_xfer(|x| assert!(x.is_none()));
    }

    #[test]
    fn file_open_bad_offset_rejected() {
        let _guard = lock_file_xfer();
        let mut payload = build_file_open_payload("g", "m:sys.xml", 0, 64);
        payload.pop();
        payload.extend_from_slice(b"offset\0-1\0\0");
        let resp = handle_file_open(&req(SoxCmd::FileOpen, payload), OWNER);
        assert_eq!(err_text(&resp), "bad offset");
    }

    #[test]
    fn file_write_chunk_no_transfer() {
        let _guard = lock_file_xfer();
        let resp = write(OWNER, 0, &[1, 2, 3]);
        assert_eq!(err_text(&resp), "no file open");
    }

    #[test]
    fn file_write_chunk_accepts_data() {
        let _guard = lock_file_xfer();
        set_xfer(SoxFileXfer::put(OWNER, 10, 10, None));
        let resp = write(OWNER, 0, &[0xAA; 10]);
        assert_eq!(resp.cmd, b'H', "expected success response 'H'");
        with_xfer(|x| {
            let file = x.expect("transfer");
            assert_eq!(file.data, vec![0xAA; 10]);
            assert_eq!(file.chunks_received, 1);
        });
    }

    #[test]
    fn file_write_chunk_out_of_range() {
        let _guard = lock_file_xfer();
        set_xfer(SoxFileXfer::put(OWNER, 10, 10, None));
        // Chunk 1 does not exist in a one-chunk file.
        assert_eq!(err_text(&write(OWNER, 1, &[0xBB; 5])), "chunk out of range");
        // The largest chunk number and size do not overflow or panic.
        set_xfer(SoxFileXfer::put(OWNER, 10, usize::from(u16::MAX), None));
        assert_eq!(
            err_text(&write(OWNER, u16::MAX, &[1])),
            "chunk out of range"
        );
    }

    #[test]
    fn file_write_chunk_of_the_wrong_size_rejected() {
        let _guard = lock_file_xfer();
        set_xfer(SoxFileXfer::put(OWNER, 12, 8, None));
        // Chunk 0 must be 8 bytes, the last chunk the 4 that remain.
        assert_eq!(err_text(&write(OWNER, 0, &[1; 5])), "bad chunk size");
        assert_eq!(err_text(&write(OWNER, 1, &[1; 8])), "bad chunk size");
        assert_eq!(write(OWNER, 1, &[1; 4]).cmd, b'H');
        with_xfer(|x| assert_eq!(x.expect("transfer").chunks_received, 1));
    }

    #[test]
    fn file_write_short_and_corrupt_payloads() {
        let _guard = lock_file_xfer();
        set_xfer(SoxFileXfer::put(OWNER, 8, 8, None));
        for payload in [vec![], vec![0], vec![0, 0, 0], vec![0, 0, 0, 9, 1, 2]] {
            let resp = handle_file_write(&req(SoxCmd::FileWrite, payload), OWNER);
            assert_eq!(resp.cmd, b'!');
            assert!(handle_put_chunk(OWNER, &[0, 0, 0, 9, 1]).is_none());
        }
        with_xfer(|x| assert_eq!(x.expect("transfer").chunks_received, 0));
    }

    #[test]
    fn file_write_multiple_chunks() {
        let _guard = lock_file_xfer();
        set_xfer(SoxFileXfer::put(OWNER, 20, 10, None));
        assert_eq!(write(OWNER, 0, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]).cmd, b'H');
        assert_eq!(
            write(OWNER, 1, &[11, 12, 13, 14, 15, 16, 17, 18, 19, 20]).cmd,
            b'H'
        );
        with_xfer(|x| {
            let file = x.expect("transfer");
            let expected: Vec<u8> = (1..=20).collect();
            assert_eq!(file.data, expected);
            assert_eq!(file.chunks_received, 2);
            assert!(file.put_complete());
        });
    }

    #[test]
    fn a_repeated_chunk_is_counted_once() {
        let _guard = lock_file_xfer();
        set_xfer(SoxFileXfer::put(OWNER, 8, 4, None));
        assert_eq!(write(OWNER, 0, &[1; 4]).cmd, b'H');
        assert_eq!(write(OWNER, 0, &[2; 4]).cmd, b'H');
        with_xfer(|x| {
            let file = x.expect("transfer");
            assert_eq!(file.chunks_received, 1);
            assert!(!file.put_complete());
        });
    }

    #[test]
    fn file_close_clears_state() {
        let _guard = lock_file_xfer();
        set_xfer(SoxFileXfer::get(OWNER, vec![0u8; 10], 10, 10, 0));
        assert_eq!(close(OWNER).cmd, b'Z');
        assert!(!is_put_transfer_active(OWNER));
        with_xfer(|x| assert!(x.is_none()));
    }

    #[test]
    fn file_close_put_writes_to_disk() {
        let _guard = lock_file_xfer();
        let test_path = temp_path("close_write.bin");
        let _ = std::fs::remove_file(&test_path);
        set_xfer(SoxFileXfer::put(
            OWNER,
            4,
            4,
            Some(test_path.to_string_lossy().into_owned()),
        ));
        assert_eq!(write(OWNER, 0, &[0xDE, 0xAD, 0xBE, 0xEF]).cmd, b'H');
        assert_eq!(close(OWNER).cmd, b'Z');
        let contents = std::fs::read(&test_path).expect("file should exist");
        assert_eq!(contents, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        let _ = std::fs::remove_file(&test_path);
    }

    #[test]
    fn file_close_with_missing_chunks_writes_nothing() {
        let _guard = lock_file_xfer();
        let test_path = temp_path("incomplete.bin");
        let _ = std::fs::remove_file(&test_path);
        set_xfer(SoxFileXfer::put(
            OWNER,
            8,
            4,
            Some(test_path.to_string_lossy().into_owned()),
        ));
        assert_eq!(write(OWNER, 1, &[9; 4]).cmd, b'H');
        assert_eq!(err_text(&close(OWNER)), "incomplete");
        assert!(
            !test_path.exists(),
            "an incomplete put must not reach the disk"
        );
        // The transfer is released either way.
        with_xfer(|x| assert!(x.is_none()));
    }

    #[test]
    fn file_close_zero_byte_put_writes_an_empty_file() {
        let _guard = lock_file_xfer();
        let test_path = temp_path("empty.bin");
        let _ = std::fs::remove_file(&test_path);
        set_xfer(SoxFileXfer::put(
            OWNER,
            0,
            256,
            Some(test_path.to_string_lossy().into_owned()),
        ));
        assert_eq!(close(OWNER).cmd, b'Z');
        assert_eq!(std::fs::read(&test_path).expect("file").len(), 0);
        let _ = std::fs::remove_file(&test_path);
    }

    #[test]
    fn is_put_transfer_active_only_for_the_owner() {
        let _guard = lock_file_xfer();
        assert!(!is_put_transfer_active(OWNER));
        set_xfer(SoxFileXfer::put(OWNER, 0, 256, None));
        assert!(is_put_transfer_active(OWNER));
        assert!(!is_put_transfer_active(OTHER));
        clear_file_xfer();
        assert!(!is_put_transfer_active(OWNER));
    }

    #[test]
    fn handle_put_chunk_silent() {
        let _guard = lock_file_xfer();
        set_xfer(SoxFileXfer::put(OWNER, 8, 4, None));
        let chunk = build_file_write_chunk_payload(0, &[0xCA, 0xFE, 0xBA, 0xBE]);
        assert!(
            handle_put_chunk(OWNER, &chunk).is_none(),
            "put chunk should be silent"
        );
        with_xfer(|x| {
            let file = x.expect("transfer");
            assert_eq!(&file.data[0..4], &[0xCA, 0xFE, 0xBA, 0xBE]);
            assert_eq!(file.chunks_received, 1);
        });
    }

    #[test]
    fn another_session_cannot_touch_the_transfer() {
        let _guard = lock_file_xfer();
        set_xfer(SoxFileXfer::put(OWNER, 8, 4, None));
        let chunk = build_file_write_chunk_payload(0, &[1; 4]);
        assert!(handle_put_chunk(OTHER, &chunk).is_none());
        assert_eq!(err_text(&write(OTHER, 0, &[1; 4])), "no file open");
        let read = handle_file_read(&req(SoxCmd::FileRead, vec![0, 0]), OTHER);
        assert_eq!(err_text(&read), "no file open");
        let open = build_file_open_payload("p", "/tmp/other.bin", 4, 4);
        assert_eq!(
            err_text(&handle_file_open(&req(SoxCmd::FileOpen, open), OTHER)),
            "busy"
        );
        assert_eq!(err_text(&close(OTHER)), "no file open");
        with_xfer(|x| {
            let file = x.expect("the owner's transfer is still open");
            assert_eq!(file.owner, OWNER);
            assert_eq!(file.chunks_received, 0);
        });
    }

    #[test]
    fn the_owner_reopening_replaces_its_transfer() {
        let _guard = lock_file_xfer();
        set_xfer(SoxFileXfer::put(OWNER, 8, 4, None));
        let path = temp_path("reopen.bin");
        let open = build_file_open_payload("p", &path.to_string_lossy(), 4, 4);
        assert_eq!(
            handle_file_open(&req(SoxCmd::FileOpen, open), OWNER).cmd,
            b'F'
        );
        with_xfer(|x| assert_eq!(x.expect("transfer").file_size, 4));
        clear_file_xfer();
    }

    #[test]
    fn a_transfer_of_a_gone_or_idle_session_is_released() {
        let _guard = lock_file_xfer();
        set_xfer(SoxFileXfer::put(OWNER, 8, 4, None));
        release_stale_file_xfer(&[OWNER, OTHER], SOX_FILE_XFER_IDLE);
        with_xfer(|x| assert!(x.is_some(), "live and active: kept"));
        release_stale_file_xfer(&[OTHER], SOX_FILE_XFER_IDLE);
        with_xfer(|x| assert!(x.is_none(), "owner session gone: released"));

        set_xfer(SoxFileXfer::get(OWNER, vec![1; 4], 4, 4, 0));
        release_stale_file_xfer(&[OWNER], Duration::ZERO);
        with_xfer(|x| assert!(x.is_none(), "idle: released"));
    }

    #[test]
    fn file_read_never_panics_on_large_offsets_or_chunks() {
        let _guard = lock_file_xfer();
        for offset in [0, 9, 10, usize::MAX - 5, usize::MAX] {
            set_xfer(SoxFileXfer::get(
                OWNER,
                vec![7u8; 10],
                4,
                10usize.saturating_sub(offset),
                offset,
            ));
            for chunk in [0u16, 1, 2, 3, u16::MAX] {
                let payload = chunk.to_be_bytes().to_vec();
                let resp = handle_file_read(&req(SoxCmd::FileRead, payload), OWNER);
                assert!(resp.cmd == b'G' || resp.cmd == b'!');
            }
        }
        set_xfer(SoxFileXfer::get(OWNER, (0u8..10).collect(), 4, 10, 0));
        let last = handle_file_read(&req(SoxCmd::FileRead, vec![0, 2]), OWNER);
        assert_eq!(last.payload, vec![8, 9]);
        let short = handle_file_read(&req(SoxCmd::FileRead, vec![0]), OWNER);
        assert_eq!(err_text(&short), "bad chunk");
    }

    // ---- validate_path (R-15) ----

    /// The temp dir as a string with no trailing separator.
    fn temp_str() -> String {
        std::env::temp_dir()
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_string()
    }

    #[test]
    fn validate_path_rejects_a_sibling_with_the_same_prefix() {
        // "/tmp" must not admit "/tmpX"; no directory may appear there.
        let sibling = format!("{}S45x{}", temp_str(), std::process::id());
        let _ = std::fs::remove_dir_all(&sibling);
        let file = Path::new(&sibling).join("sub").join("f.bin");
        let res = validate_path(&file.to_string_lossy(), PathUse::Write);
        assert_eq!(res.err(), Some("invalid path"));
        assert!(
            !Path::new(&sibling).exists(),
            "nothing is created before the check"
        );
        assert!(validate_path("/home/eacio/sandstar/etc/config2/f", PathUse::Write).is_err());
    }

    #[test]
    fn validate_path_creates_missing_dirs_only_inside_the_allowed_tree() {
        let dir = temp_path("newdir");
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("a").join("f.bin");
        let ok = validate_path(&file.to_string_lossy(), PathUse::Write).expect("allowed");
        assert!(dir.join("a").is_dir());
        assert_eq!(ok.file_name(), file.file_name());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_path_rename_source_creates_nothing() {
        let dir = temp_path("missing_src");
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("f.bin");
        let res = validate_path(&file.to_string_lossy(), PathUse::RenameSource);
        assert_eq!(res.err(), Some("not found"));
        assert!(!dir.exists());
    }

    #[test]
    fn validate_path_rejects_relative_and_dotdot() {
        assert!(validate_path("tmp/f", PathUse::Write).is_err());
        assert!(validate_path("/tmp/../etc/f", PathUse::Write).is_err());
        assert!(validate_path("/tmp/../etc/f", PathUse::Read).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn validate_path_rejects_a_link_out_of_the_allowed_tree() {
        let outside = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("s45_outside_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).expect("outside dir");
        let link = temp_path("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");

        // Existing parent behind the link.
        let direct = link.join("f.bin");
        assert!(validate_path(&direct.to_string_lossy(), PathUse::Write).is_err());
        // New directories behind the link must not be created.
        let deep = link.join("made").join("f.bin");
        assert!(validate_path(&deep.to_string_lossy(), PathUse::Write).is_err());
        assert!(
            !outside.join("made").exists(),
            "no directory outside the allowed tree"
        );

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn file_rename_empty_paths_rejected() {
        let payload = vec![0x00, 0x00]; // empty "from" and "to"
        let resp = handle_file_rename(&req(SoxCmd::FileRename, payload));
        assert_eq!(resp.cmd, b'!', "expected error for empty paths");
    }

    #[test]
    fn file_rename_path_traversal_rejected() {
        let mut payload = Vec::new();
        payload.extend_from_slice(b"/tmp/../etc/shadow\0");
        payload.extend_from_slice(b"/tmp/renamed\0");
        let resp = handle_file_rename(&req(SoxCmd::FileRename, payload));
        assert_eq!(resp.cmd, b'!', "expected error for path traversal");
    }

    fn rename_payload(from: &Path, to: &Path) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(from.to_string_lossy().as_bytes());
        payload.push(0x00);
        payload.extend_from_slice(to.to_string_lossy().as_bytes());
        payload.push(0x00);
        payload
    }

    #[test]
    fn file_rename_success() {
        let from = temp_path("rename_from.txt");
        let to = temp_path("rename_to.txt");
        let _ = std::fs::remove_file(&from);
        let _ = std::fs::remove_file(&to);
        std::fs::write(&from, b"test content").expect("write source file");

        let resp = handle_file_rename(&req(SoxCmd::FileRename, rename_payload(&from, &to)));
        assert_eq!(resp.cmd, b'B', "expected success response 'B'");
        assert!(!from.exists(), "source should not exist after rename");
        assert_eq!(std::fs::read(&to).expect("read dest file"), b"test content");
        let _ = std::fs::remove_file(&to);
    }

    #[test]
    fn file_rename_source_not_found() {
        let from = temp_path("nonexistent.txt");
        let to = temp_path("rename_dest.txt");
        let _ = std::fs::remove_file(&from);
        let _ = std::fs::remove_file(&to);
        let resp = handle_file_rename(&req(SoxCmd::FileRename, rename_payload(&from, &to)));
        assert_eq!(err_text(&resp), "not found");
    }

    #[test]
    fn file_write_not_in_put_mode() {
        let _guard = lock_file_xfer();
        set_xfer(SoxFileXfer::get(OWNER, vec![0u8; 10], 10, 10, 0));
        assert_eq!(err_text(&write(OWNER, 0, &[1, 2, 3])), "not in put mode");
    }

    #[test]
    fn full_put_flow_open_write_close() {
        let _guard = lock_file_xfer();
        let test_path = temp_path("full_put.bin");
        let _ = std::fs::remove_file(&test_path);
        let file_data = b"Hello, Sandstar!";

        let open = build_file_open_payload("p", &test_path.to_string_lossy(), 16, 8);
        let resp = handle_file_open(&req(SoxCmd::FileOpen, open), OWNER);
        assert_eq!(resp.cmd, b'F', "fileOpen put: {:?}", resp.payload);
        assert!(is_put_transfer_active(OWNER));

        assert_eq!(write(OWNER, 0, &file_data[0..8]).cmd, b'H');
        // The second chunk arrives as a raw 'k' message, as Sedona sends it.
        let chunk = build_file_write_chunk_payload(1, &file_data[8..16]);
        assert!(handle_put_chunk(OWNER, &chunk).is_none());
        assert_eq!(close(OWNER).cmd, b'Z');

        assert_eq!(
            std::fs::read(&test_path).expect("file should exist"),
            file_data
        );
        let _ = std::fs::remove_file(&test_path);
    }

    #[test]
    fn file_open_short_or_odd_payloads_are_refused_without_panic() {
        let _guard = lock_file_xfer();
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![b'p'],
            b"p\0".to_vec(),
            b"p\0/tmp/x\0".to_vec(),
            b"p\0/tmp/x\0\0\0\0\x04".to_vec(), // size but no chunk size
        ];
        for payload in cases {
            let resp = handle_file_open(&req(SoxCmd::FileOpen, payload), OWNER);
            assert_eq!(err_text(&resp), "bad request");
        }
        // Multi-byte UTF-8 in the uri: answered either way, no panic.
        for uri in ["/tmp/çğ\u{1F600}/ü.bin", "m:ü\u{1F600}.xml", "/kits/ş"] {
            let open = build_file_open_payload("p", uri, 4, 4);
            let resp = handle_file_open(&req(SoxCmd::FileOpen, open), OWNER);
            assert!(resp.cmd == b'F' || resp.cmd == b'!');
            clear_file_xfer();
        }
    }

    #[test]
    fn file_open_put_touches_nothing_until_close() {
        let _guard = lock_file_xfer();
        let dir = temp_path("lazy_dir");
        let _ = std::fs::remove_dir_all(&dir);
        let target = dir.join("sub").join("f.bin");
        let open = build_file_open_payload("p", &target.to_string_lossy(), 4, 4);
        assert_eq!(
            handle_file_open(&req(SoxCmd::FileOpen, open), OWNER).cmd,
            b'F'
        );
        assert!(!dir.exists(), "fileOpen creates no directory");
        // Abandoned: still nothing on disk, and the close says so.
        release_stale_file_xfer(&[], SOX_FILE_XFER_IDLE);
        assert!(!dir.exists());
        assert_eq!(err_text(&close(OWNER)), "transfer expired");
        // A second close of that session is a plain no-op again.
        assert_eq!(close(OWNER).cmd, b'Z');

        let open = build_file_open_payload("p", &target.to_string_lossy(), 4, 4);
        assert_eq!(
            handle_file_open(&req(SoxCmd::FileOpen, open), OWNER).cmd,
            b'F'
        );
        assert_eq!(write(OWNER, 0, b"abcd").cmd, b'H');
        assert_eq!(close(OWNER).cmd, b'Z');
        assert_eq!(std::fs::read(&target).expect("written"), b"abcd");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_file_open_keeps_the_owners_transfer() {
        let _guard = lock_file_xfer();
        set_xfer(SoxFileXfer::put(OWNER, 8, 4, None));
        assert_eq!(write(OWNER, 0, &[1; 4]).cmd, b'H');
        let bad = build_file_open_payload("p", "/tmp/../etc/x", 4, 4);
        assert_eq!(
            handle_file_open(&req(SoxCmd::FileOpen, bad), OWNER).cmd,
            b'!'
        );
        let big = build_file_open_payload("p", "/tmp/x", 11 * 1024 * 1024, 4);
        assert_eq!(
            handle_file_open(&req(SoxCmd::FileOpen, big), OWNER).cmd,
            b'!'
        );
        with_xfer(|x| assert_eq!(x.expect("kept").chunks_received, 1));
    }

    #[test]
    fn rejected_chunks_are_counted() {
        let _guard = lock_file_xfer();
        set_xfer(SoxFileXfer::put(OWNER, 8, 4, None));
        for _ in 0..5 {
            assert!(handle_put_chunk(OWNER, &[0, 9, 0, 1, 7]).is_none());
        }
        assert_eq!(err_text(&write(OWNER, 0, &[1; 3])), "bad chunk size");
        with_xfer(|x| {
            let file = x.expect("transfer");
            assert_eq!(file.rejected, 6);
            assert_eq!(file.chunks_received, 0);
        });
    }

    #[test]
    fn validate_path_allows_dots_inside_a_name() {
        let file = temp_path("kit..xml");
        assert!(validate_path(&file.to_string_lossy(), PathUse::WriteCheck).is_ok());
    }

    #[test]
    fn validate_path_creates_a_missing_allowed_dir() {
        // The allowed dir itself does not exist yet, its parent does (as with
        // etc/manifests on a fresh device): a put must still be possible.
        let root = temp_path("allow_root");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let allowed = root.join("manifests");
        let dirs = vec![allowed.clone()];
        let f = allowed.join("kit.xml").to_string_lossy().into_owned();
        assert!(validate_path_in(&f, PathUse::WriteCheck, &dirs, &[]).is_ok());
        assert!(!allowed.exists(), "a check creates nothing");
        assert!(validate_path_in(&f, PathUse::Write, &dirs, &[]).is_ok());
        assert!(allowed.is_dir());
        // Beside the allowed dir: refused, nothing created.
        let b = root
            .join("other")
            .join("x.xml")
            .to_string_lossy()
            .into_owned();
        assert!(validate_path_in(&b, PathUse::Write, &dirs, &[]).is_err());
        assert!(!root.join("other").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_as_the_file_itself_is_refused() {
        let _guard = lock_file_xfer();
        let outside = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("s45_victim_{}", std::process::id()));
        std::fs::create_dir_all(outside.parent().expect("parent")).expect("target dir");
        std::fs::write(&outside, b"keep").expect("victim");
        let link = temp_path("file_link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");
        let l = link.to_string_lossy().into_owned();

        assert!(validate_path(&l, PathUse::WriteCheck).is_err());
        assert!(validate_path(&l, PathUse::RenameSource).is_err());
        let open = build_file_open_payload("p", &l, 4, 4);
        assert_eq!(
            handle_file_open(&req(SoxCmd::FileOpen, open), OWNER).cmd,
            b'!'
        );

        // A link put in place after fileOpen is refused at fileClose.
        let target = temp_path("late_link");
        let _ = std::fs::remove_file(&target);
        let t = target.to_string_lossy().into_owned();
        let open = build_file_open_payload("p", &t, 4, 4);
        assert_eq!(
            handle_file_open(&req(SoxCmd::FileOpen, open), OWNER).cmd,
            b'F'
        );
        assert_eq!(write(OWNER, 0, b"new!").cmd, b'H');
        std::os::unix::fs::symlink(&outside, &target).expect("late symlink");
        assert_eq!(close(OWNER).cmd, b'!', "the target is a link at fileClose");
        assert_eq!(std::fs::read(&outside).expect("victim"), b"keep");

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn write_atomic_replaces_the_file_and_leaves_no_temp() {
        let dir = temp_path("atomic");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let file = dir.join("cfg.xml");
        std::fs::write(&file, b"old").expect("old");
        write_atomic(&file, b"new content").expect("write");
        assert_eq!(std::fs::read(&file).expect("read"), b"new content");
        assert_eq!(
            std::fs::read_dir(&dir).expect("ls").count(),
            1,
            "no temp file left"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- Persistence tests ----

    #[test]
    fn persist_save_load_round_trip() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let persist_path = dir.path().join("sox_components.json");
        let persist_str = persist_path.to_str().unwrap().to_string();

        // Build a tree with channel components and user-added components
        let channels = sample_channels();
        let mut tree = ComponentTree::from_channels(&channels);
        tree.set_persist_path(persist_str.clone());

        // Add two user-added components under control (comp_id=6)
        let id1 = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: id1,
            parent_id: 6,
            name: "myConst".into(),
            type_name: "control::ConstFloat".into(),
            kit_id: 5,
            type_id: 1,
            children: Vec::new(),
            slots: vec![VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Float as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Float(72.5),
            }],
            links: Vec::new(),
        });
        tree.mark_user_added(id1);

        let id2 = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: id2,
            parent_id: 6,
            name: "myAdd".into(),
            type_name: "control::Add2".into(),
            kit_id: 5,
            type_id: 2,
            children: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "in1".into(),
                    type_id: SoxValueType::Float as u8,
                    flags: SLOT_FLAG_RUNTIME,
                    value: SlotValue::Float(0.0),
                },
                VirtualSlot {
                    name: "in2".into(),
                    type_id: SoxValueType::Float as u8,
                    flags: SLOT_FLAG_RUNTIME,
                    value: SlotValue::Float(0.0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: SoxValueType::Float as u8,
                    flags: SLOT_FLAG_RUNTIME,
                    value: SlotValue::Float(0.0),
                },
            ],
            links: Vec::new(),
        });
        tree.mark_user_added(id2);

        // Add a link between them
        tree.add_link(id1, 0, id2, 0);
        tree.mark_dirty();

        // Save
        tree.save_user_components().expect("save should succeed");
        assert!(persist_path.exists(), "persist file should be created");

        // Load into a new tree built from the same channels
        let mut tree2 = ComponentTree::from_channels(&channels);
        tree2.set_persist_path(persist_str);
        let loaded = tree2.load_user_components().expect("load should succeed");
        assert_eq!(loaded, 2);

        // Verify components restored
        let comp1 = tree2.get(id1).expect("user comp 1 should exist");
        assert_eq!(comp1.name, "myConst");
        assert_eq!(comp1.parent_id, 6);
        assert_eq!(comp1.slots.len(), 1);
        assert_eq!(comp1.slots[0].value, SlotValue::Float(72.5));

        let comp2 = tree2.get(id2).expect("user comp 2 should exist");
        assert_eq!(comp2.name, "myAdd");
        assert_eq!(comp2.slots.len(), 3);

        // Verify link restored
        assert!(!comp2.links.is_empty(), "links should be restored");
        assert_eq!(comp2.links[0].from_comp, id1);
        assert_eq!(comp2.links[0].to_comp, id2);

        // Verify user_added_ids restored
        assert!(tree2.is_user_added(id1));
        assert!(tree2.is_user_added(id2));

        // Verify next_comp_id is correct (no collisions)
        let id3 = tree2.next_comp_id();
        assert!(id3 > id2, "next_comp_id should be beyond loaded components");

        // Verify parent registered children
        let control = tree2.get(6).expect("control folder should exist");
        assert!(
            control.children.contains(&id1),
            "control should contain user comp 1"
        );
        assert!(
            control.children.contains(&id2),
            "control should contain user comp 2"
        );
    }

    #[test]
    fn persist_channel_comps_not_saved() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let persist_path = dir.path().join("sox_components.json");
        let persist_str = persist_path.to_str().unwrap().to_string();

        let channels = sample_channels();
        let mut tree = ComponentTree::from_channels(&channels);
        tree.set_persist_path(persist_str);

        // No user-added components — save should produce empty list
        tree.mark_dirty();
        tree.save_user_components().expect("save should succeed");

        let json = std::fs::read_to_string(&persist_path).expect("read");
        let data: PersistData = serde_json::from_str(&json).expect("parse");
        assert_eq!(
            data.components.len(),
            0,
            "channel components should NOT be saved"
        );
        assert_eq!(data.user_added_ids.len(), 0);
    }

    #[test]
    fn persist_user_comps_are_saved() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let persist_path = dir.path().join("sox_components.json");
        let persist_str = persist_path.to_str().unwrap().to_string();

        let mut tree = ComponentTree::from_channels(&[]);
        tree.set_persist_path(persist_str);

        let id = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: id,
            parent_id: 6,
            name: "userComp".into(),
            type_name: "test::Thing".into(),
            kit_id: 99,
            type_id: 1,
            children: Vec::new(),
            slots: vec![VirtualSlot {
                name: "val".into(),
                type_id: SoxValueType::Int as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Int(42),
            }],
            links: Vec::new(),
        });
        tree.mark_user_added(id);

        tree.save_user_components().expect("save should succeed");

        let json = std::fs::read_to_string(&persist_path).expect("read");
        let data: PersistData = serde_json::from_str(&json).expect("parse");
        assert_eq!(data.components.len(), 1);
        assert_eq!(data.components[0].name, "userComp");
        assert_eq!(data.user_added_ids, vec![id]);
    }

    #[test]
    fn persist_dirty_flag_behavior() {
        let mut tree = ComponentTree::new();

        // Initially not dirty
        assert!(!tree.take_dirty());

        // Mark dirty
        tree.mark_dirty();
        assert!(tree.take_dirty());

        // take_dirty clears the flag
        assert!(!tree.take_dirty());

        // mark_user_added sets dirty
        tree.mark_user_added(999);
        assert!(tree.take_dirty());
    }

    #[test]
    fn persist_load_nonexistent_file_is_noop() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let persist_path = dir.path().join("does_not_exist.json");
        let persist_str = persist_path.to_str().unwrap().to_string();

        let mut tree = ComponentTree::new();
        tree.set_persist_path(persist_str);

        let loaded = tree
            .load_user_components()
            .expect("load should succeed even with no file");
        assert_eq!(loaded, 0);
    }

    #[test]
    fn persist_delete_removes_from_user_added_ids() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let persist_path = dir.path().join("sox_components.json");
        let persist_str = persist_path.to_str().unwrap().to_string();

        let mut tree = ComponentTree::from_channels(&[]);
        tree.set_persist_path(persist_str.clone());

        let id = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: id,
            parent_id: 6,
            name: "toDelete".into(),
            type_name: "test::Thing".into(),
            kit_id: 99,
            type_id: 1,
            children: Vec::new(),
            slots: Vec::new(),
            links: Vec::new(),
        });
        tree.mark_user_added(id);
        assert!(tree.is_user_added(id));

        // Remove it (simulating handle_delete behavior)
        tree.remove(id);
        tree.user_added_ids.remove(&id);
        tree.mark_dirty();

        assert!(!tree.is_user_added(id));

        // Save and verify it's gone
        tree.save_user_components().expect("save");
        let json = std::fs::read_to_string(&persist_path).expect("read");
        let data: PersistData = serde_json::from_str(&json).expect("parse");
        assert_eq!(data.components.len(), 0);
    }

    #[test]
    fn persist_slot_value_types_round_trip() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let persist_path = dir.path().join("sox_components.json");
        let persist_str = persist_path.to_str().unwrap().to_string();

        let mut tree = ComponentTree::from_channels(&[]);
        tree.set_persist_path(persist_str.clone());

        let id = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: id,
            parent_id: 6,
            name: "allTypes".into(),
            type_name: "test::AllTypes".into(),
            kit_id: 99,
            type_id: 1,
            children: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "b".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Bool(true),
                },
                VirtualSlot {
                    name: "i".into(),
                    type_id: 2,
                    flags: 0,
                    value: SlotValue::Int(-42),
                },
                VirtualSlot {
                    name: "l".into(),
                    type_id: 3,
                    flags: 0,
                    value: SlotValue::Long(123456789),
                },
                VirtualSlot {
                    name: "f".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(3.14),
                },
                VirtualSlot {
                    name: "d".into(),
                    type_id: 5,
                    flags: 0,
                    value: SlotValue::Double(2.71828),
                },
                VirtualSlot {
                    name: "s".into(),
                    type_id: 7,
                    flags: 0,
                    value: SlotValue::Str("hello".into()),
                },
                VirtualSlot {
                    name: "buf".into(),
                    type_id: 8,
                    flags: 0,
                    value: SlotValue::Buf(vec![1, 2, 3]),
                },
                VirtualSlot {
                    name: "n".into(),
                    type_id: 0,
                    flags: 0,
                    value: SlotValue::Null,
                },
            ],
            links: Vec::new(),
        });
        tree.mark_user_added(id);

        tree.save_user_components().expect("save");

        let mut tree2 = ComponentTree::from_channels(&[]);
        tree2.set_persist_path(persist_str);
        tree2.load_user_components().expect("load");

        let comp = tree2.get(id).expect("comp should exist");
        assert_eq!(comp.slots[0].value, SlotValue::Bool(true));
        assert_eq!(comp.slots[1].value, SlotValue::Int(-42));
        assert_eq!(comp.slots[2].value, SlotValue::Long(123456789));
        assert_eq!(comp.slots[3].value, SlotValue::Float(3.14));
        assert_eq!(comp.slots[4].value, SlotValue::Double(2.71828));
        assert_eq!(comp.slots[5].value, SlotValue::Str("hello".into()));
        assert_eq!(comp.slots[6].value, SlotValue::Buf(vec![1, 2, 3]));
        assert_eq!(comp.slots[7].value, SlotValue::Null);
    }

    #[test]
    fn persist_next_comp_id_restored_correctly() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let persist_path = dir.path().join("sox_components.json");
        let persist_str = persist_path.to_str().unwrap().to_string();

        let mut tree = ComponentTree::from_channels(&sample_channels());
        tree.set_persist_path(persist_str.clone());

        // Add a component with a high ID
        let id = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: id,
            parent_id: 6,
            name: "highId".into(),
            type_name: "test::Thing".into(),
            kit_id: 99,
            type_id: 1,
            children: Vec::new(),
            slots: Vec::new(),
            links: Vec::new(),
        });
        tree.mark_user_added(id);
        tree.save_user_components().expect("save");

        // Load into a fresh tree — next_comp_id should not collide
        let mut tree2 = ComponentTree::from_channels(&sample_channels());
        tree2.set_persist_path(persist_str);
        tree2.load_user_components().expect("load");

        let new_id = tree2.next_comp_id();
        assert!(
            new_id > id,
            "next_comp_id ({new_id}) should be > loaded id ({id})"
        );
    }

    // ---- Cycle detection tests ----

    #[test]
    fn cycle_detection_simple_no_cycle() {
        // A→B should be allowed (no cycle)
        let mut tree = ComponentTree::from_channels(&sample_channels());
        assert!(tree.add_link(100, 0, 101, 0));
    }

    #[test]
    fn cycle_detection_direct_cycle_rejected() {
        // A→B then B→A should be rejected (direct cycle)
        let mut tree = ComponentTree::from_channels(&sample_channels());
        assert!(tree.add_link(100, 0, 101, 0));
        assert!(!tree.add_link(101, 0, 100, 0));
    }

    #[test]
    fn cycle_detection_longer_chain_rejected() {
        // A→B→C then C→A should be rejected (3-node cycle)
        let mut tree = ComponentTree::from_channels(&sample_channels());
        assert!(tree.add_link(100, 0, 101, 0));
        assert!(tree.add_link(101, 0, 102, 0));
        assert!(!tree.add_link(102, 0, 100, 0));
    }

    #[test]
    fn cycle_detection_self_loop_allowed() {
        // A→A self-loop should be allowed (editor uses these)
        let mut tree = ComponentTree::from_channels(&sample_channels());
        assert!(tree.add_link(100, 0, 100, 1));
    }

    #[test]
    fn cycle_detection_chain_then_cycle() {
        // A→B→C, then C→A via different slots — still a cycle
        let mut tree = ComponentTree::from_channels(&sample_channels());
        assert!(tree.add_link(100, 1, 101, 2));
        assert!(tree.add_link(101, 3, 102, 4));
        // C→A would close the cycle
        assert!(!tree.add_link(102, 5, 100, 6));
    }

    #[test]
    fn cycle_detection_large_chain_no_cycle() {
        // Build a long chain of components without a cycle — all should be allowed.
        // Use dynamically-added components beyond the channel range.
        let mut tree = ComponentTree::from_channels(&sample_channels());
        // Add extra components (IDs 200..209)
        for id in 200..210u16 {
            tree.add(VirtualComponent {
                comp_id: id,
                parent_id: 1,
                name: format!("Comp{id}"),
                type_name: "sys::Component".into(),
                kit_id: 0,
                type_id: 5,
                children: vec![],
                slots: vec![VirtualSlot {
                    name: "meta".into(),
                    type_id: SoxValueType::Int as u8,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Int(0),
                }],
                links: vec![],
            });
        }
        // Chain: 200→201→202→…→209 (no cycle)
        for id in 200..209u16 {
            assert!(
                tree.add_link(id, 0, id + 1, 0),
                "link {id}->{} should succeed",
                id + 1
            );
        }
        // Adding 209→200 would create a cycle — rejected
        assert!(!tree.add_link(209, 0, 200, 0));
        // Adding 209→100 (not in chain) should be fine
        assert!(tree.add_link(209, 0, 100, 0));
    }

    #[test]
    fn handle_link_cycle_rejected_error() {
        // Verify handle_link returns error for cyclic links
        let mut tree = ComponentTree::from_channels(&sample_channels());
        tree.add_link(100, 0, 101, 0);
        // Try adding 101→100 via handler — should fail with cycle error
        let mut payload = Vec::new();
        payload.push(b'a'); // subcmd = add
        payload.extend_from_slice(&101u16.to_be_bytes()); // fromCompId
        payload.push(0); // fromSlotId
        payload.extend_from_slice(&100u16.to_be_bytes()); // toCompId
        payload.push(0); // toSlotId
        let req = SoxRequest {
            cmd: SoxCmd::Link,
            req_id: 50,
            payload,
        };
        let resp = handle_link(&req, &mut tree);
        assert_eq!(resp.cmd, b'!'); // error response
    }

    // ---- collect_channel_writes tests ----

    /// Helper: create a channel component with the standard slot layout.
    /// Slot 2 = channel ID (Int), Slot 6 = out (Float).
    fn make_channel_comp(
        comp_id: u16,
        parent_id: u16,
        name: &str,
        channel_id: i32,
        out_value: f32,
    ) -> VirtualComponent {
        VirtualComponent {
            comp_id,
            parent_id,
            name: name.into(),
            type_name: "EacIo::AnalogInput".into(),
            kit_id: 15, // EacIo kit
            type_id: 0,
            children: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Int(1),
                },
                VirtualSlot {
                    name: "channelName".into(),
                    type_id: 8,
                    flags: SLOT_FLAG_RUNTIME,
                    value: SlotValue::Str(name.into()),
                },
                VirtualSlot {
                    name: "channel".into(),
                    type_id: 1,
                    flags: SLOT_FLAG_RUNTIME,
                    value: SlotValue::Int(channel_id),
                },
                VirtualSlot {
                    name: "pointQuery".into(),
                    type_id: 8,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Str(String::new()),
                },
                VirtualSlot {
                    name: "pointQuerySize".into(),
                    type_id: 1,
                    flags: SLOT_FLAG_RUNTIME,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "pointQueryStatus".into(),
                    type_id: 0,
                    flags: SLOT_FLAG_RUNTIME,
                    value: SlotValue::Bool(false),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 4,
                    flags: SLOT_FLAG_RUNTIME,
                    value: SlotValue::Float(out_value),
                },
                VirtualSlot {
                    name: "curStatus".into(),
                    type_id: 8,
                    flags: SLOT_FLAG_RUNTIME,
                    value: SlotValue::Str("ok".into()),
                },
                VirtualSlot {
                    name: "enabled".into(),
                    type_id: 0,
                    flags: SLOT_FLAG_RUNTIME,
                    value: SlotValue::Bool(true),
                },
            ],
            links: Vec::new(),
        }
    }

    #[test]
    fn collect_channel_writes_logic_to_channel() {
        // Setup: Add2 component (comp 200) wired to channel comp (comp 100)
        let mut tree = ComponentTree::new();
        let ch = make_channel_comp(CHANNEL_COMP_BASE, 5, "ch_1113", 1113, 0.0);
        tree.add(ch);
        tree.channel_comp_end = CHANNEL_COMP_BASE + 1;

        let mut add = make_math_comp(200, 6, "add", 2, 3);
        add.slots[1].value = SlotValue::Float(42.5); // out = 42.5
        tree.add(add);

        // Link: add.out(slot 1) -> channel.out(slot 6)
        tree.add_link(200, 1, CHANNEL_COMP_BASE, 6);

        // Simulate execute_links: propagate add.out to channel.out
        let link_changed = tree.execute_links();
        assert!(link_changed.contains(&CHANNEL_COMP_BASE));

        // Now collect channel writes
        let writes = tree.collect_channel_writes(&link_changed);
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, 1113); // channel_id
        assert!((writes[0].1 - 42.5).abs() < 0.001); // value
    }

    #[test]
    fn collect_channel_writes_no_link_no_write() {
        // Channel comp changed but has no incoming link — should NOT produce a write.
        let mut tree = ComponentTree::new();
        let ch = make_channel_comp(CHANNEL_COMP_BASE, 5, "ch_1113", 1113, 72.5);
        tree.add(ch);
        tree.channel_comp_end = CHANNEL_COMP_BASE + 1;

        let writes = tree.collect_channel_writes(&[CHANNEL_COMP_BASE]);
        assert!(writes.is_empty(), "no incoming link means no write");
    }

    #[test]
    fn collect_channel_writes_link_to_non_out_slot_ignored() {
        // Link targets slot 0 (meta) on a channel comp, not slot 6 (out).
        let mut tree = ComponentTree::new();
        let ch = make_channel_comp(CHANNEL_COMP_BASE, 5, "ch_1113", 1113, 0.0);
        tree.add(ch);
        tree.channel_comp_end = CHANNEL_COMP_BASE + 1;

        let src = make_source_comp(200, 6, "src", 99.0);
        tree.add(src);

        // Link to slot 0 (meta), not slot 6 (out)
        tree.add_link(200, 1, CHANNEL_COMP_BASE, 0);

        let writes = tree.collect_channel_writes(&[CHANNEL_COMP_BASE]);
        assert!(
            writes.is_empty(),
            "link to non-out slot should not produce channel write"
        );
    }

    #[test]
    fn collect_channel_writes_non_channel_comp_ignored() {
        // Changed comp_id is NOT a channel comp — should be ignored.
        let mut tree = ComponentTree::new();
        tree.channel_comp_end = CHANNEL_COMP_BASE; // no channel comps

        let src = make_source_comp(200, 6, "src", 50.0);
        tree.add(src);

        let writes = tree.collect_channel_writes(&[200]);
        assert!(
            writes.is_empty(),
            "non-channel comp should not produce channel write"
        );
    }

    #[test]
    fn collect_channel_writes_full_dataflow() {
        // End-to-end: ConstFloat(78.0) → Add2 → channel comp
        // Simulates sensor value flowing through logic and back to a channel.
        let mut tree = ComponentTree::new();

        // Channel comp 100: channel 2001, initial out=0.0
        let ch = make_channel_comp(CHANNEL_COMP_BASE, 5, "ch_2001", 2001, 0.0);
        tree.add(ch);
        tree.channel_comp_end = CHANNEL_COMP_BASE + 1;

        // ConstFloat source: out=78.0
        let src = make_source_comp(200, 6, "const", 78.0);
        tree.add(src);

        // Add2: in1 wired from const, in2=2.0 (manual offset)
        let mut add = make_math_comp(201, 6, "add", 2, 3);
        add.slots[3].value = SlotValue::Float(2.0); // in2 = 2.0
        tree.add(add);

        // Wire: const.out(1) -> add.in1(2)
        tree.add_link(200, 1, 201, 2);
        // Wire: add.out(1) -> channel.out(6)
        tree.add_link(201, 1, CHANNEL_COMP_BASE, 6);

        // Execute dataflow
        let link_changed = tree.execute_links();
        let comp_changed = tree.execute_components();

        // After execute_links: add.in1 = 78.0
        // After execute_components: add.out = 78.0 + 2.0 = 80.0

        // Second round of link propagation to push add.out to channel.out
        let link_changed2 = tree.execute_links();

        let all_changed: Vec<u16> = link_changed
            .iter()
            .chain(comp_changed.iter())
            .chain(link_changed2.iter())
            .copied()
            .collect();

        let writes = tree.collect_channel_writes(&all_changed);
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, 2001);
        assert!(
            (writes[0].1 - 80.0).abs() < 0.001,
            "expected 80.0, got {}",
            writes[0].1
        );
    }

    // ---- Sensor bridge (chXXXX naming) tests ----

    #[test]
    fn sensor_bridge_ch1713_matching_channel_updates_out() {
        // A user-added ConstFloat named "ch1713" should auto-bridge from channel 1713.
        let mut tree = ComponentTree::new();

        // Channel comp for channel 1713 with sensor value 78.0
        let ch = make_channel_comp(CHANNEL_COMP_BASE, 5, "ch_1713", 1713, 78.0);
        tree.add(ch);
        tree.channel_comp_end = CHANNEL_COMP_BASE + 1;

        // User-added ConstFloat named "ch1713" — out is slot 1, initially 0.0
        let src = make_source_comp(200, 6, "ch1713", 0.0);
        tree.add(src);
        tree.user_added_ids.insert(200);

        let changed = tree.execute_components();

        // The bridge should have updated slot 1 (out) to 78.0
        let comp = tree.get(200).unwrap();
        match &comp.slots[1].value {
            SlotValue::Float(v) => assert!((*v - 78.0).abs() < 0.001, "expected 78.0, got {v}"),
            other => panic!("expected Float, got {other:?}"),
        }
        assert!(changed.contains(&200), "comp 200 should be in changed list");
    }

    #[test]
    fn sensor_bridge_ch9999_no_matching_channel_stays_zero() {
        // "ch9999" has no matching channel comp — out should stay 0.0.
        let mut tree = ComponentTree::new();

        // Only channel 1113 exists
        let ch = make_channel_comp(CHANNEL_COMP_BASE, 5, "ch_1113", 1113, 72.5);
        tree.add(ch);
        tree.channel_comp_end = CHANNEL_COMP_BASE + 1;

        let src = make_source_comp(200, 6, "ch9999", 0.0);
        tree.add(src);
        tree.user_added_ids.insert(200);

        let changed = tree.execute_components();

        let comp = tree.get(200).unwrap();
        match &comp.slots[1].value {
            SlotValue::Float(v) => {
                assert!((*v - 0.0).abs() < 0.001, "out should stay 0.0, got {v}")
            }
            other => panic!("expected Float, got {other:?}"),
        }
        assert!(
            !changed.contains(&200),
            "no change expected for unmatched channel"
        );
    }

    #[test]
    fn sensor_bridge_cheese_not_a_channel_number() {
        // "cheese" starts with "ch" but the remainder is not a number — no bridge.
        let mut tree = ComponentTree::new();

        let ch = make_channel_comp(CHANNEL_COMP_BASE, 5, "ch_1113", 1113, 72.5);
        tree.add(ch);
        tree.channel_comp_end = CHANNEL_COMP_BASE + 1;

        let src = make_source_comp(200, 6, "cheese", 0.0);
        tree.add(src);
        tree.user_added_ids.insert(200);

        let changed = tree.execute_components();

        let comp = tree.get(200).unwrap();
        match &comp.slots[1].value {
            SlotValue::Float(v) => {
                assert!((*v - 0.0).abs() < 0.001, "out should stay 0.0, got {v}")
            }
            other => panic!("expected Float, got {other:?}"),
        }
        assert!(
            !changed.contains(&200),
            "cheese should not trigger sensor bridge"
        );
    }

    #[test]
    fn sensor_bridge_ch0_invalid_channel_no_bridge() {
        // "ch0" parses to channel 0 which is invalid (ch_id > 0 required) — no bridge.
        let mut tree = ComponentTree::new();

        let ch = make_channel_comp(CHANNEL_COMP_BASE, 5, "ch_1113", 1113, 72.5);
        tree.add(ch);
        tree.channel_comp_end = CHANNEL_COMP_BASE + 1;

        let src = make_source_comp(200, 6, "ch0", 0.0);
        tree.add(src);
        tree.user_added_ids.insert(200);

        let changed = tree.execute_components();

        let comp = tree.get(200).unwrap();
        match &comp.slots[1].value {
            SlotValue::Float(v) => assert!(
                (*v - 0.0).abs() < 0.001,
                "out should stay 0.0 for ch0, got {v}"
            ),
            other => panic!("expected Float, got {other:?}"),
        }
        assert!(
            !changed.contains(&200),
            "ch0 should not trigger sensor bridge"
        );
    }

    #[test]
    fn output_channel_bridge_does_not_overwrite_user_value() {
        // An output channel (direction="out") should NOT have its ConstFloat
        // overwritten by the sensor bridge. Instead, the ConstFloat value should
        // flow TO the channel component.
        let channels = vec![
            ChannelInfo {
                id: 360,
                label: "Digital Output 6".into(),
                channel_type: "digital".into(),
                direction: "out".into(),
                enabled: true,
                status: "ok".into(),
                cur: 0.0,
                raw: 0.0,
            },
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
        ];
        let mut tree = ComponentTree::from_channels(&channels);

        // Verify output_channel_ids was populated
        assert!(
            tree.output_channel_ids.contains(&360),
            "ch360 should be in output_channel_ids"
        );
        assert!(
            !tree.output_channel_ids.contains(&1113),
            "ch1113 should NOT be in output_channel_ids"
        );

        // Add a user ConstFloat named "ch360" with value 1.0 (user wants output ON)
        let cf_id = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: cf_id,
            parent_id: 6,
            name: "ch360".into(),
            type_name: "control::ConstFloat".into(),
            kit_id: 1,
            type_id: 100,
            children: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 4,
                    flags: SLOT_FLAG_CONFIG,
                    value: SlotValue::Int(1),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 5,
                    flags: SLOT_FLAG_RUNTIME,
                    value: SlotValue::Float(1.0),
                },
            ],
            links: Vec::new(),
        });
        tree.user_added_ids.insert(cf_id);

        // Execute — the output bridge should write ConstFloat value (1.0) TO the channel,
        // NOT overwrite the ConstFloat with the channel's current value (0.0).
        tree.execute_components();

        // ConstFloat "ch360" should still be 1.0
        let comp = tree.get(cf_id).unwrap();
        match &comp.slots[1].value {
            SlotValue::Float(v) => assert!(
                (*v - 1.0).abs() < 0.001,
                "output channel ConstFloat should keep user value 1.0, got {v}"
            ),
            other => panic!("expected Float, got {other:?}"),
        }

        // Run again to confirm stability across multiple ticks
        tree.execute_components();
        let comp = tree.get(cf_id).unwrap();
        match &comp.slots[1].value {
            SlotValue::Float(v) => assert!(
                (*v - 1.0).abs() < 0.001,
                "output channel ConstFloat should still be 1.0 after second tick, got {v}"
            ),
            other => panic!("expected Float, got {other:?}"),
        }
    }

    // ---- Save/Hibernate invoke tests ----

    #[test]
    fn invoke_save_action_on_app_calls_persist() {
        // Invoke "save" on comp 0 (App) should trigger save_user_components.
        let dir = tempfile::tempdir().expect("tmpdir");
        let persist_path = dir.path().join("sox_components.json");
        let persist_str = persist_path.to_str().unwrap().to_string();

        let channels = sample_channels();
        let mut tree = ComponentTree::from_channels(&channels);
        tree.set_persist_path(persist_str.clone());

        // Add a user component so save has something to write
        let id1 = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: id1,
            parent_id: 6,
            name: "saveTest".into(),
            type_name: "control::ConstFloat".into(),
            kit_id: 5,
            type_id: 1,
            children: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(42.0),
                },
                VirtualSlot {
                    name: "save".into(),
                    type_id: 4,
                    flags: SLOT_FLAG_ACTION,
                    value: SlotValue::Null,
                },
            ],
            links: Vec::new(),
        });
        tree.mark_user_added(id1);

        // Build invoke request: comp_id=0, slot_id for "save" action
        // App comp (0) has slots; we need to find a slot named "save".
        // For simplicity, invoke on the user comp that has a "save" slot at index 2.
        let mut payload = Vec::new();
        payload.extend_from_slice(&id1.to_be_bytes()); // comp_id
        payload.push(2); // slot_id = index 2 ("save")
        let req = SoxRequest {
            cmd: SoxCmd::Invoke,
            req_id: 99,
            payload,
        };
        let resp = handle_invoke(&req, &mut tree);

        // Should succeed
        assert_ne!(resp.cmd, b'!', "invoke save should not return error");
        // Persistence file should exist
        assert!(
            persist_path.exists(),
            "save action should create persistence file"
        );
    }

    #[test]
    fn invoke_hibernate_action_persists() {
        // Invoke "hibernate" should also trigger save_user_components.
        let dir = tempfile::tempdir().expect("tmpdir");
        let persist_path = dir.path().join("sox_components.json");
        let persist_str = persist_path.to_str().unwrap().to_string();

        let channels = sample_channels();
        let mut tree = ComponentTree::from_channels(&channels);
        tree.set_persist_path(persist_str.clone());

        let id1 = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: id1,
            parent_id: 6,
            name: "hibTest".into(),
            type_name: "control::ConstFloat".into(),
            kit_id: 5,
            type_id: 1,
            children: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(10.0),
                },
                VirtualSlot {
                    name: "hibernate".into(),
                    type_id: 4,
                    flags: SLOT_FLAG_ACTION,
                    value: SlotValue::Null,
                },
            ],
            links: Vec::new(),
        });
        tree.mark_user_added(id1);

        let mut payload = Vec::new();
        payload.extend_from_slice(&id1.to_be_bytes()); // comp_id
        payload.push(2); // slot_id = index 2 ("hibernate")
        let req = SoxRequest {
            cmd: SoxCmd::Invoke,
            req_id: 98,
            payload,
        };
        let resp = handle_invoke(&req, &mut tree);

        assert_ne!(resp.cmd, b'!', "invoke hibernate should not return error");
        assert!(
            persist_path.exists(),
            "hibernate action should create persistence file"
        );
    }

    // ---- Manifest slot inheritance tests ----

    #[test]
    fn manifest_inherit_add4_from_add2_gets_6_slots() {
        // Add4 (base=control::Add2) should inherit Add2's slots (meta, out, in1, in2)
        // plus its own (in3, in4) = 6 total.
        let xml_control = r#"<?xml version='1.0'?>
<kitManifest name="control" checksum="808b7db3" version="1.2.28">
<type id="3" name="Add2" sizeof="64" base="sys::Component">
  <slot id="0" name="out" type="float" flags="r"/>
  <slot id="1" name="in1" type="float" flags="c"/>
  <slot id="2" name="in2" type="float" flags="c"/>
</type>
<type id="49" name="Add4" sizeof="80" base="control::Add2">
  <slot id="0" name="in3" type="float" flags="c"/>
  <slot id="1" name="in4" type="float" flags="c"/>
</type>
</kitManifest>"#;

        let mut db = ManifestDb::new();
        db.parse_kit_manifest(xml_control, 2); // kit_index=2 for control

        let slots = db.get_slots(2, 49).expect("Add4 should be in manifest");
        assert_eq!(
            slots.len(),
            6,
            "Add4 should have 6 slots: meta, out, in1, in2, in3, in4"
        );
        assert_eq!(slots[0].name, "meta");
        assert_eq!(slots[1].name, "out");
        assert_eq!(slots[2].name, "in1");
        assert_eq!(slots[3].name, "in2");
        assert_eq!(slots[4].name, "in3");
        assert_eq!(slots[5].name, "in4");
    }

    #[test]
    fn manifest_inherit_sub4_from_sub2() {
        // Sub4 (base=control::Sub2) inherits Sub2's slots.
        let xml_control = r#"<?xml version='1.0'?>
<kitManifest name="control" checksum="808b7db3" version="1.2.28">
<type id="37" name="Sub2" sizeof="64" base="sys::Component">
  <slot id="0" name="out" type="float" flags="r"/>
  <slot id="1" name="in1" type="float" flags="c"/>
  <slot id="2" name="in2" type="float" flags="c"/>
</type>
<type id="50" name="Sub4" sizeof="80" base="control::Sub2">
  <slot id="0" name="in3" type="float" flags="c"/>
  <slot id="1" name="in4" type="float" flags="c"/>
</type>
</kitManifest>"#;

        let mut db = ManifestDb::new();
        db.parse_kit_manifest(xml_control, 2);

        let slots = db.get_slots(2, 50).expect("Sub4 should be in manifest");
        assert_eq!(
            slots.len(),
            6,
            "Sub4 should have 6 slots: meta, out, in1, in2, in3, in4"
        );
        assert_eq!(slots[0].name, "meta");
        assert_eq!(slots[1].name, "out");
        assert_eq!(slots[2].name, "in1");
        assert_eq!(slots[3].name, "in2");
        assert_eq!(slots[4].name, "in3");
        assert_eq!(slots[5].name, "in4");
    }

    #[test]
    fn manifest_type_with_sys_component_base_gets_meta_prepended() {
        // A type with base="sys::Component" and no meta in its own slots
        // should get meta prepended.
        let xml = r#"<?xml version='1.0'?>
<kitManifest name="control" checksum="808b7db3" version="1.2.28">
<type id="99" name="CustomComp" sizeof="32" base="sys::Component">
  <slot id="0" name="value" type="float" flags="c"/>
</type>
</kitManifest>"#;

        let mut db = ManifestDb::new();
        db.parse_kit_manifest(xml, 2);

        let slots = db
            .get_slots(2, 99)
            .expect("CustomComp should be in manifest");
        assert!(slots.len() >= 2, "should have at least meta + value");
        assert_eq!(slots[0].name, "meta", "meta should be prepended");
        assert_eq!(slots[1].name, "value");
    }

    // ---- collect_channel_writes additional edge-case tests ----

    #[test]
    fn collect_channel_writes_double_value_coerced() {
        // Channel comp with Double value in slot 6 should still produce a write.
        let mut tree = ComponentTree::new();
        let mut ch = make_channel_comp(CHANNEL_COMP_BASE, 5, "ch_1113", 1113, 0.0);
        ch.slots[6].value = SlotValue::Double(99.9);
        ch.slots[6].type_id = SoxValueType::Double as u8;
        tree.add(ch);
        tree.channel_comp_end = CHANNEL_COMP_BASE + 1;

        let src = make_source_comp(200, 6, "src", 99.9);
        tree.add(src);
        tree.add_link(200, 1, CHANNEL_COMP_BASE, 6);

        let writes = tree.collect_channel_writes(&[CHANNEL_COMP_BASE]);
        assert_eq!(writes.len(), 1);
        assert!(
            (writes[0].1 - 99.9).abs() < 0.001,
            "Double should coerce to f64"
        );
    }

    #[test]
    fn collect_channel_writes_int_value_coerced() {
        // Channel comp with Int value in slot 6 should coerce to f64.
        let mut tree = ComponentTree::new();
        let mut ch = make_channel_comp(CHANNEL_COMP_BASE, 5, "ch_1113", 1113, 0.0);
        ch.slots[6].value = SlotValue::Int(42);
        ch.slots[6].type_id = SoxValueType::Int as u8;
        tree.add(ch);
        tree.channel_comp_end = CHANNEL_COMP_BASE + 1;

        let src = make_source_comp(200, 6, "src", 42.0);
        tree.add(src);
        tree.add_link(200, 1, CHANNEL_COMP_BASE, 6);

        let writes = tree.collect_channel_writes(&[CHANNEL_COMP_BASE]);
        assert_eq!(writes.len(), 1);
        assert!(
            (writes[0].1 - 42.0).abs() < 0.001,
            "Int should coerce to f64"
        );
    }

    // ---- Persistence round-trip with links ----

    #[test]
    fn persist_round_trip_with_links_preserved() {
        // Create comps with links, save, load into new tree — verify links survive.
        let dir = tempfile::tempdir().expect("tmpdir");
        let persist_path = dir.path().join("sox_links.json");
        let persist_str = persist_path.to_str().unwrap().to_string();

        let channels = sample_channels();
        let mut tree = ComponentTree::from_channels(&channels);
        tree.set_persist_path(persist_str.clone());

        // Add a ConstFloat source
        let src_id = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: src_id,
            parent_id: 6,
            name: "linkSrc".into(),
            type_name: "control::ConstFloat".into(),
            kit_id: 2,
            type_id: 14,
            children: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(55.5),
                },
            ],
            links: Vec::new(),
        });
        tree.mark_user_added(src_id);

        // Add an Add2 target
        let tgt_id = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: tgt_id,
            parent_id: 6,
            name: "linkTgt".into(),
            type_name: "control::Add2".into(),
            kit_id: 2,
            type_id: 3,
            children: Vec::new(),
            slots: vec![
                VirtualSlot {
                    name: "meta".into(),
                    type_id: 1,
                    flags: 0,
                    value: SlotValue::Int(0),
                },
                VirtualSlot {
                    name: "out".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(0.0),
                },
                VirtualSlot {
                    name: "in1".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(0.0),
                },
                VirtualSlot {
                    name: "in2".into(),
                    type_id: 4,
                    flags: 0,
                    value: SlotValue::Float(0.0),
                },
            ],
            links: Vec::new(),
        });
        tree.mark_user_added(tgt_id);

        // Wire: src.out(1) -> tgt.in1(2)
        tree.add_link(src_id, 1, tgt_id, 2);
        // Wire: src.out(1) -> tgt.in2(3) (second link)
        tree.add_link(src_id, 1, tgt_id, 3);
        tree.mark_dirty();

        // Save
        tree.save_user_components().expect("save should succeed");

        // Load into a fresh tree
        let mut tree2 = ComponentTree::from_channels(&channels);
        tree2.set_persist_path(persist_str);
        let loaded = tree2.load_user_components().expect("load should succeed");
        assert_eq!(loaded, 2);

        // Verify source component
        let src_loaded = tree2.get(src_id).expect("source should exist");
        assert_eq!(src_loaded.name, "linkSrc");
        assert_eq!(src_loaded.slots[1].value, SlotValue::Float(55.5));

        // Verify target component has links restored
        let tgt_loaded = tree2.get(tgt_id).expect("target should exist");
        assert_eq!(tgt_loaded.name, "linkTgt");
        assert!(
            tgt_loaded.links.len() >= 2,
            "target should have 2 links, got {}",
            tgt_loaded.links.len()
        );

        // Check specific link details
        let link1 = tgt_loaded.links.iter().find(|l| l.to_slot == 2);
        assert!(link1.is_some(), "link to in1 (slot 2) should exist");
        let link1 = link1.unwrap();
        assert_eq!(link1.from_comp, src_id);
        assert_eq!(link1.from_slot, 1);

        let link2 = tgt_loaded.links.iter().find(|l| l.to_slot == 3);
        assert!(link2.is_some(), "link to in2 (slot 3) should exist");
        let link2 = link2.unwrap();
        assert_eq!(link2.from_comp, src_id);
        assert_eq!(link2.from_slot, 1);

        // Verify user_added_ids restored
        assert!(tree2.is_user_added(src_id));
        assert!(tree2.is_user_added(tgt_id));

        // Verify execute_links works after load (links are functional, not just serialized)
        let changed = tree2.execute_links();
        // src.out(55.5) should propagate to tgt.in1 and tgt.in2
        let tgt_after = tree2.get(tgt_id).unwrap();
        match &tgt_after.slots[2].value {
            SlotValue::Float(v) => assert!(
                (*v - 55.5).abs() < 0.001,
                "in1 should be 55.5 after link exec"
            ),
            other => panic!("expected Float for in1, got {other:?}"),
        }
        match &tgt_after.slots[3].value {
            SlotValue::Float(v) => assert!(
                (*v - 55.5).abs() < 0.001,
                "in2 should be 55.5 after link exec"
            ),
            other => panic!("expected Float for in2, got {other:?}"),
        }
        assert!(
            changed.contains(&tgt_id),
            "target should be in changed list after link exec"
        );
    }

    /// Deterministic pseudo-random bytes (no extra dependency, same run every time).
    fn sweep_rng(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *seed >> 33
    }

    /// A SOX request for `cmd` with a random payload. The first payload
    /// bytes lean toward small component ids so handlers get past lookup.
    fn sweep_request(cmd: u8, seed: &mut u64) -> Vec<u8> {
        let len = (sweep_rng(seed) % 48) as usize;
        let mut data = vec![cmd, (sweep_rng(seed) % 250) as u8];
        for i in 0..len {
            let r = sweep_rng(seed);
            data.push(match (i, r % 6) {
                (0, _) => 0,
                (1, _) => (r % 24) as u8,
                (_, 0) => 0,
                (_, 1) => 0xFF,
                _ => (r >> 8) as u8,
            });
        }
        data
    }

    /// Commands swept for panics. File transfer (f/g/h/…) is left out: it
    /// writes to disk and keeps global state (R-15).
    const SWEPT_CMDS: &[u8] = b"vycrlsuwiadno";

    #[test]
    fn sox_handlers_never_panic_on_random_payloads() {
        // R-4: a panic in a SOX handler aborts the process in release.
        let dir = tempfile::tempdir().expect("tmpdir");
        let persist = dir.path().join("sweep.json").to_string_lossy().into_owned();
        let channels = sample_channels();
        let mut seed = 0x5EED_u64;
        let mut failures = Vec::new();

        // Fresh tree per request: every handler sees the same starting state.
        for &cmd in SWEPT_CMDS {
            for _ in 0..600 {
                let data = sweep_request(cmd, &mut seed);
                let Some(req) = SoxRequest::parse(&data) else {
                    continue;
                };
                let (chans, path) = (channels.clone(), persist.clone());
                let res = std::panic::catch_unwind(move || {
                    let mut tree = ComponentTree::from_channels(&chans);
                    tree.set_persist_path(path);
                    let mut subs = SubscriptionManager::new();
                    let _ = handle_sox_request(&req, &mut tree, &mut subs, 1);
                });
                if res.is_err() {
                    failures.push(format!("fresh {data:02x?}"));
                }
            }
        }

        // One tree across all requests: adds, links and renames build up
        // state that later requests run into.
        let mut tree = ComponentTree::from_channels(&channels);
        tree.set_persist_path(persist);
        let mut subs = SubscriptionManager::new();
        for _ in 0..12_000 {
            let cmd = SWEPT_CMDS[(sweep_rng(&mut seed) % SWEPT_CMDS.len() as u64) as usize];
            let data = sweep_request(cmd, &mut seed);
            let Some(req) = SoxRequest::parse(&data) else {
                continue;
            };
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = handle_sox_request(&req, &mut tree, &mut subs, 1);
            }));
            if res.is_err() {
                failures.push(format!("stateful {data:02x?}"));
                break; // the tree may be half-updated after a panic
            }
        }

        assert!(
            failures.is_empty(),
            "{} panics, first: {:?}",
            failures.len(),
            failures.first()
        );
    }

    #[test]
    fn persist_writes_only_names_still_in_use() {
        // Renames used to leave every old name in the table and the
        // persistence file, so a full table survived restarts (R-47).
        let dir = tempfile::tempdir().expect("tmpdir");
        let persist_path = dir.path().join("sox_names.json");
        let persist_str = persist_path.to_str().unwrap().to_string();

        let channels = sample_channels();
        let mut tree = ComponentTree::from_channels(&channels);
        tree.set_persist_path(persist_str.clone());
        let id = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: id,
            parent_id: 6,
            name: "oldName".into(),
            type_name: "Test".into(),
            kit_id: 2,
            type_id: 14,
            children: Vec::new(),
            slots: Vec::new(),
            links: Vec::new(),
        });
        tree.mark_user_added(id);
        for n in ["tmpA", "tmpB", "newName"] {
            assert!(tree.rename(id, n.into()));
        }
        assert!(tree.name_table.contains("oldName"));
        tree.save_user_components().expect("save");

        let json = std::fs::read_to_string(&persist_path).expect("read persist file");
        let data: PersistData = serde_json::from_str(&json).expect("parse");
        assert!(data.name_table.contains(&"newName".to_string()));
        for stale in ["oldName", "tmpA", "tmpB"] {
            assert!(!data.name_table.contains(&stale.to_string()), "{stale}");
        }

        let mut tree2 = ComponentTree::from_channels(&channels);
        tree2.set_persist_path(persist_str);
        tree2.load_user_components().expect("load");
        assert!(tree2.name_table.contains("newName"));
        assert!(!tree2.name_table.contains("oldName"));
    }

    #[test]
    fn full_name_table_does_not_block_rename_or_add() {
        let channels = sample_channels();
        let mut tree = ComponentTree::from_channels(&channels);
        // A one-name table: intern one name to fill it, then every new
        // name is refused.
        tree.name_table = super::super::name_intern::NameInternTable::with_limit(1);
        assert!(tree.name_table.intern("filler").is_valid());

        let id = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: id,
            parent_id: 6,
            name: "addedWhenFull".into(),
            type_name: "Test".into(),
            kit_id: 2,
            type_id: 14,
            children: Vec::new(),
            slots: Vec::new(),
            links: Vec::new(),
        });
        assert!(tree.rename(id, "renamedWhenFull".into()));
        assert_eq!(
            tree.get(id).map(|c| c.name.as_str()),
            Some("renamedWhenFull")
        );
        // The filler belongs to no component, so it was dropped for the new name.
        assert!(tree.name_table.contains("renamedWhenFull"));
        assert!(!tree.name_table.contains("filler"));
        assert_eq!(tree.name_table.len(), 1);
    }

    #[test]
    fn rename_flood_keeps_the_table_to_names_in_use() {
        let channels = sample_channels();
        let mut tree = ComponentTree::from_channels(&channels);
        let id = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: id,
            parent_id: 6,
            name: "flood".into(),
            type_name: "Test".into(),
            kit_id: 2,
            type_id: 14,
            children: Vec::new(),
            slots: Vec::new(),
            links: Vec::new(),
        });
        let live = tree.live_names().len();
        tree.name_table = super::super::name_intern::NameInternTable::with_limit(live + 5);
        for comp in tree.components.values() {
            tree.name_table.intern(&comp.name);
        }
        for i in 0..100 {
            assert!(tree.rename(id, format!("flood{i}")));
        }
        assert!(tree.name_table.contains("flood99"));
        assert!(tree.name_table.len() <= live + 5);
    }

    #[test]
    fn loading_an_old_full_name_table_keeps_component_names() {
        // Files saved before S-26 list every name ever used. Loading one
        // into a full table must not crowd out the live components.
        let dir = tempfile::tempdir().expect("tmpdir");
        let persist_path = dir.path().join("sox_names.json");
        let persist_str = persist_path.to_str().unwrap().to_string();
        let channels = sample_channels();

        let mut tree = ComponentTree::from_channels(&channels);
        tree.set_persist_path(persist_str.clone());
        let id = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: id,
            parent_id: 6,
            name: "liveComp".into(),
            type_name: "Test".into(),
            kit_id: 2,
            type_id: 14,
            children: Vec::new(),
            slots: Vec::new(),
            links: Vec::new(),
        });
        tree.mark_user_added(id);
        tree.save_user_components().expect("save");
        let json = std::fs::read_to_string(&persist_path).expect("read");
        let mut data: PersistData = serde_json::from_str(&json).expect("parse");
        data.name_table = (0..200).map(|i| format!("stale{i}")).collect();
        std::fs::write(&persist_path, serde_json::to_string(&data).unwrap()).unwrap();

        let mut tree2 = ComponentTree::from_channels(&channels);
        let builtin = tree2.live_names().len();
        tree2.name_table = super::super::name_intern::NameInternTable::with_limit(builtin + 50);
        for comp in tree2.components.values() {
            tree2.name_table.intern(&comp.name);
        }
        tree2.set_persist_path(persist_str);
        tree2.load_user_components().expect("load");
        assert!(tree2.name_table.contains("liveComp"));
        assert_eq!(tree2.get(id).map(|c| c.name.as_str()), Some("liveComp"));
    }

    #[test]
    fn persist_name_table_round_trip() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let persist_path = dir.path().join("sox_names.json");
        let persist_str = persist_path.to_str().unwrap().to_string();

        let channels = sample_channels();
        let mut tree = ComponentTree::from_channels(&channels);
        tree.set_persist_path(persist_str.clone());

        // Add a user component (its name gets interned)
        let id1 = tree.next_comp_id();
        tree.add(VirtualComponent {
            comp_id: id1,
            parent_id: 6,
            name: "nameTest".into(),
            type_name: "Test".into(),
            kit_id: 2,
            type_id: 14,
            children: Vec::new(),
            slots: vec![VirtualSlot {
                name: "out".into(),
                type_id: SoxValueType::Float as u8,
                flags: SLOT_FLAG_CONFIG,
                value: SlotValue::Float(1.0),
            }],
            links: Vec::new(),
        });
        tree.mark_user_added(id1);
        tree.mark_dirty();

        // Verify the name is interned
        assert!(tree.name_table.contains("nameTest"));
        let original_count = tree.name_table.len();

        // Save (includes name_table)
        tree.save_user_components().expect("save should succeed");

        // Verify the persisted JSON contains name_table
        let json = std::fs::read_to_string(&persist_path).expect("read persist file");
        let data: PersistData = serde_json::from_str(&json).expect("parse");
        assert!(
            !data.name_table.is_empty(),
            "name_table should be non-empty in persistence"
        );
        assert!(data.name_table.contains(&"nameTest".to_string()));

        // Load into a fresh tree
        let mut tree2 = ComponentTree::from_channels(&channels);
        tree2.set_persist_path(persist_str);
        tree2.load_user_components().expect("load should succeed");

        // Verify name_table restored
        assert!(
            tree2.name_table.contains("nameTest"),
            "name should be re-interned from persistence"
        );
        assert!(
            tree2.name_table.len() >= original_count,
            "restored name table should have at least as many entries"
        );
    }

    #[test]
    fn persist_backward_compat_no_name_table() {
        // Verify that old persistence files without name_table still load correctly
        let dir = tempfile::tempdir().expect("tmpdir");
        let persist_path = dir.path().join("sox_old.json");
        let persist_str = persist_path.to_str().unwrap().to_string();

        // Write old-format JSON (no name_table field)
        let old_json = r#"{
            "components": [],
            "user_added_ids": [],
            "next_comp_id": 100
        }"#;
        std::fs::write(&persist_path, old_json).expect("write old json");

        let channels = sample_channels();
        let mut tree = ComponentTree::from_channels(&channels);
        tree.set_persist_path(persist_str);

        // Should load without error — name_table defaults to empty
        let loaded = tree
            .load_user_components()
            .expect("load should succeed with old format");
        assert_eq!(loaded, 0);
    }

    // ── Name validation tests ──────────────────────────────

    #[test]
    fn validate_sox_name_valid() {
        assert!(validate_sox_name("myComp").is_none());
        assert!(validate_sox_name("a").is_none());
        assert!(validate_sox_name("comp_1").is_none());
        assert!(validate_sox_name("Add2").is_none());
        assert!(validate_sox_name("A").is_none());
    }

    #[test]
    fn validate_sox_name_empty() {
        assert_eq!(validate_sox_name(""), Some("name cannot be empty"));
    }

    #[test]
    fn validate_sox_name_too_long() {
        let long = "a".repeat(32);
        assert_eq!(
            validate_sox_name(&long),
            Some("name too long (max 31 chars)")
        );
        // Exactly 31 should be fine
        let exact = "a".repeat(31);
        assert!(validate_sox_name(&exact).is_none());
    }

    #[test]
    fn validate_sox_name_starts_digit() {
        assert_eq!(
            validate_sox_name("1comp"),
            Some("name must start with a letter")
        );
    }

    #[test]
    fn validate_sox_name_starts_underscore() {
        assert_eq!(
            validate_sox_name("_comp"),
            Some("name must start with a letter")
        );
    }

    #[test]
    fn validate_sox_name_special_chars() {
        assert_eq!(
            validate_sox_name("my-comp"),
            Some("name can only contain letters, numbers, and underscores")
        );
        assert_eq!(
            validate_sox_name("my comp"),
            Some("name can only contain letters, numbers, and underscores")
        );
    }

    #[test]
    fn sox_rename_rejects_invalid_name() {
        let mut tree = ComponentTree::new();
        // Add a user component to rename
        let comp = VirtualComponent {
            comp_id: 100,
            parent_id: 0,
            name: "validName".to_string(),
            type_name: "test::Comp".to_string(),
            kit_id: 0,
            type_id: 0,
            children: Vec::new(),
            slots: Vec::new(),
            links: Vec::new(),
        };
        tree.add(comp);

        // Try renaming with invalid name (starts with digit)
        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes()); // comp_id
        payload.extend_from_slice(b"1bad\0"); // invalid name
        let req = SoxRequest {
            cmd: SoxCmd::Rename,
            req_id: 1,
            payload,
        };
        let resp = handle_rename(&req, &mut tree);
        // Should fail — response cmd should be error ('!')
        assert_ne!(
            resp.payload.first().copied(),
            Some(b'R'),
            "rename with digit-first name should be rejected"
        );
    }

    #[test]
    fn sox_add_rejects_invalid_name() {
        let mut tree = ComponentTree::new();

        // Try adding with name that has special chars
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u16.to_be_bytes()); // parent_id = root
        payload.push(0); // kit_id
        payload.push(0); // type_id
        payload.extend_from_slice(b"bad-name\0"); // invalid name (hyphen)
        let req = SoxRequest {
            cmd: SoxCmd::Add,
            req_id: 1,
            payload,
        };
        let resp = handle_add(&req, &mut tree);
        // Should fail — response should not contain a new comp ID
        assert_ne!(
            resp.payload.first().copied(),
            Some(b'A'),
            "add with hyphenated name should be rejected"
        );
    }

    // ═══════════════════════════════════════════════════════════
    // readComp what='d' (dynamic tags extension) tests
    // ═══════════════════════════════════════════════════════════

    use crate::sox::dyn_slots::{DynSlotStore, DynValue};
    use crate::sox::DynSlotStoreHandle;

    fn make_dyn_store() -> DynSlotStoreHandle {
        Arc::new(std::sync::RwLock::new(DynSlotStore::with_defaults()))
    }

    #[test]
    fn handle_read_comp_dynamic_no_tags() {
        let tree = ComponentTree::from_channels(&sample_channels());
        let store = make_dyn_store();

        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes()); // comp_id = first channel
        payload.push(b'd');
        let req = SoxRequest {
            cmd: SoxCmd::ReadComp,
            req_id: 1,
            payload,
        };
        let resp = handle_read_comp(&req, &tree, Some(&store));

        assert_eq!(resp.cmd, b'C');
        let mut r = SoxReader::new(&resp.payload);
        assert_eq!(r.read_u16(), Some(100)); // comp_id
        assert_eq!(r.read_u8(), Some(b'd')); // what echoed
        assert_eq!(r.read_u16(), Some(0)); // zero tags
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn handle_read_comp_dynamic_no_store() {
        let tree = ComponentTree::from_channels(&sample_channels());

        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(b'd');
        let req = SoxRequest {
            cmd: SoxCmd::ReadComp,
            req_id: 2,
            payload,
        };
        let resp = handle_read_comp(&req, &tree, None);

        assert_eq!(resp.cmd, b'C');
        let mut r = SoxReader::new(&resp.payload);
        assert_eq!(r.read_u16(), Some(100));
        assert_eq!(r.read_u8(), Some(b'd'));
        assert_eq!(r.read_u16(), Some(0)); // zero tags when no store
    }

    #[test]
    fn handle_read_comp_dynamic_with_tags() {
        let tree = ComponentTree::from_channels(&sample_channels());
        let store = make_dyn_store();

        // Set some tags on comp 100
        {
            let mut s = store.write().unwrap();
            s.set(100, "modbusAddr".into(), DynValue::Int(40001))
                .unwrap();
            s.set(100, "point".into(), DynValue::Marker).unwrap();
            s.set(100, "enabled".into(), DynValue::Bool(true)).unwrap();
        }

        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(b'd');
        let req = SoxRequest {
            cmd: SoxCmd::ReadComp,
            req_id: 3,
            payload,
        };
        let resp = handle_read_comp(&req, &tree, Some(&store));

        assert_eq!(resp.cmd, b'C');
        let mut r = SoxReader::new(&resp.payload);
        assert_eq!(r.read_u16(), Some(100)); // comp_id
        assert_eq!(r.read_u8(), Some(b'd')); // what
        let count = r.read_u16().unwrap();
        assert_eq!(count, 3); // three tags

        // Tags are sorted by key: enabled, modbusAddr, point
        // Tag 1: "enabled" Bool(true)
        assert_eq!(r.read_str(), Some("enabled".into()));
        assert_eq!(r.read_u8(), Some(2)); // type=Bool
        assert_eq!(r.read_u8(), Some(1)); // true

        // Tag 2: "modbusAddr" Int(40001)
        assert_eq!(r.read_str(), Some("modbusAddr".into()));
        assert_eq!(r.read_u8(), Some(3)); // type=Int
        assert_eq!(r.read_i32(), Some(40001));

        // Tag 3: "point" Marker
        assert_eq!(r.read_str(), Some("point".into()));
        assert_eq!(r.read_u8(), Some(1)); // type=Marker

        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn handle_read_comp_dynamic_all_value_types() {
        let tree = ComponentTree::from_channels(&sample_channels());
        let store = make_dyn_store();

        {
            let mut s = store.write().unwrap();
            s.set(100, "a_null".into(), DynValue::Null).unwrap();
            s.set(100, "b_marker".into(), DynValue::Marker).unwrap();
            s.set(100, "c_bool".into(), DynValue::Bool(false)).unwrap();
            s.set(100, "d_int".into(), DynValue::Int(-42)).unwrap();
            s.set(100, "e_float".into(), DynValue::Float(3.14)).unwrap();
            s.set(100, "f_str".into(), DynValue::Str("hello".into()))
                .unwrap();
            s.set(100, "g_ref".into(), DynValue::Ref("@p:r:abc".into()))
                .unwrap();
        }

        let mut payload = Vec::new();
        payload.extend_from_slice(&100u16.to_be_bytes());
        payload.push(b'd');
        let req = SoxRequest {
            cmd: SoxCmd::ReadComp,
            req_id: 4,
            payload,
        };
        let resp = handle_read_comp(&req, &tree, Some(&store));

        let mut r = SoxReader::new(&resp.payload);
        r.read_u16(); // comp_id
        r.read_u8(); // what

        let count = r.read_u16().unwrap();
        assert_eq!(count, 7);

        // Sorted by key: a_null, b_marker, c_bool, d_int, e_float, f_str, g_ref

        // a_null: type=0 (Null)
        assert_eq!(r.read_str(), Some("a_null".into()));
        assert_eq!(r.read_u8(), Some(0));

        // b_marker: type=1 (Marker)
        assert_eq!(r.read_str(), Some("b_marker".into()));
        assert_eq!(r.read_u8(), Some(1));

        // c_bool: type=2, val=0
        assert_eq!(r.read_str(), Some("c_bool".into()));
        assert_eq!(r.read_u8(), Some(2));
        assert_eq!(r.read_u8(), Some(0));

        // d_int: type=3, val=-42
        assert_eq!(r.read_str(), Some("d_int".into()));
        assert_eq!(r.read_u8(), Some(3));
        assert_eq!(r.read_i32(), Some(-42));

        // e_float: type=4, val=3.14
        assert_eq!(r.read_str(), Some("e_float".into()));
        assert_eq!(r.read_u8(), Some(4));
        let f = r.read_f32().unwrap();
        assert!((f - 3.14).abs() < 0.01);

        // f_str: type=5, val="hello"
        assert_eq!(r.read_str(), Some("f_str".into()));
        assert_eq!(r.read_u8(), Some(5));
        assert_eq!(r.read_str(), Some("hello".into()));

        // g_ref: type=6, val="@p:r:abc"
        assert_eq!(r.read_str(), Some("g_ref".into()));
        assert_eq!(r.read_u8(), Some(6));
        assert_eq!(r.read_str(), Some("@p:r:abc".into()));

        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn handle_read_comp_dynamic_unknown_comp() {
        let tree = ComponentTree::from_channels(&sample_channels());
        let store = make_dyn_store();

        let mut payload = Vec::new();
        payload.extend_from_slice(&999u16.to_be_bytes()); // nonexistent comp
        payload.push(b'd');
        let req = SoxRequest {
            cmd: SoxCmd::ReadComp,
            req_id: 5,
            payload,
        };
        let resp = handle_read_comp(&req, &tree, Some(&store));

        // Should return error (unknown comp)
        assert_eq!(resp.cmd, b'!');
    }

    // ── S-55: links to nothing, orphans, saved-file report ──

    fn s55_comp(comp_id: u16, parent_id: u16, slots: usize) -> VirtualComponent {
        VirtualComponent {
            comp_id,
            parent_id,
            name: format!("s55c{comp_id}"),
            type_name: "Test".into(),
            kit_id: 200,
            type_id: 1,
            children: vec![],
            slots: (0..slots)
                .map(|i| VirtualSlot {
                    name: format!("s{i}"),
                    type_id: SoxValueType::Float as u8,
                    flags: 0,
                    value: SlotValue::Float(0.0),
                })
                .collect(),
            links: vec![],
        }
    }

    fn s55_tree() -> ComponentTree {
        let mut manifest = ManifestDb::new();
        manifest.know_type_for_test(200, 1);
        let mut tree = ComponentTree::new_with_manifest(Arc::new(manifest));
        tree.add(s55_comp(0, NO_PARENT, 1));
        tree.add(s55_comp(300, 0, 3));
        tree.add(s55_comp(301, 0, 3));
        tree
    }

    #[test]
    fn try_add_link_names_each_reason() {
        let mut tree = s55_tree();
        assert_eq!(tree.try_add_link(300, 1, 999, 1), Err(LinkError::NotFound));
        assert_eq!(tree.try_add_link(999, 1, 301, 1), Err(LinkError::NotFound));
        assert_eq!(tree.try_add_link(300, 3, 301, 1), Err(LinkError::BadSlot));
        assert_eq!(tree.try_add_link(300, 1, 301, 3), Err(LinkError::BadSlot));
        assert_eq!(tree.try_add_link(300, 1, 301, 1), Ok(()));
        assert_eq!(tree.try_add_link(300, 1, 301, 1), Err(LinkError::Duplicate));
        assert_eq!(tree.try_add_link(301, 2, 300, 2), Err(LinkError::Cycle));
        // A refused link leaves nothing behind.
        assert_eq!(tree.get(300).unwrap().links.len(), 1);
        assert_eq!(tree.get(301).unwrap().links.len(), 1);
        assert_eq!(tree.integrity_report(), IntegrityReport::default());
    }

    #[test]
    fn subtree_ids_of_a_missing_component_is_empty() {
        let tree = s55_tree();
        assert!(tree.subtree_ids(999).is_empty());
        assert_eq!(tree.subtree_ids(300), vec![300]);
        let mut all = tree.subtree_ids(0);
        all.sort_unstable();
        assert_eq!(all, vec![0, 300, 301]);
    }

    #[test]
    fn remove_subtree_of_a_missing_component_changes_nothing() {
        let mut tree = s55_tree();
        assert!(tree.remove_subtree(999).is_none());
        assert_eq!(tree.len(), 3);
    }

    #[test]
    fn integrity_report_counts_orphans_and_bad_links_once() {
        let mut tree = s55_tree();
        assert!(tree.add_link(300, 1, 301, 1));
        assert_eq!(tree.integrity_report(), IntegrityReport::default());

        // An orphan: its parent was never in the tree.
        tree.add(s55_comp(302, 777, 3));
        // A link to a component that is not there, and one to a slot past
        // the end; the second is stored on both of its ends.
        let gone = Link {
            from_comp: 300,
            from_slot: 1,
            to_comp: 888,
            to_slot: 1,
        };
        let bad_slot = Link {
            from_comp: 300,
            from_slot: 2,
            to_comp: 301,
            to_slot: 9,
        };
        tree.get_mut(300).unwrap().links.push(gone);
        tree.get_mut(300).unwrap().links.push(bad_slot.clone());
        tree.get_mut(301).unwrap().links.push(bad_slot);

        assert_eq!(
            tree.integrity_report(),
            IntegrityReport {
                orphans: 1,
                bad_links: 2
            }
        );
    }

    /// The UDP SOX delete takes the subtree and its links with it too.
    #[test]
    fn sox_delete_removes_the_subtree_and_its_links() {
        let mut tree = s55_tree();
        tree.add(s55_comp(310, 300, 3));
        tree.mark_user_added(300);
        tree.mark_user_added(310);
        assert!(tree.add_link(310, 1, 301, 1));

        let req = SoxRequest {
            cmd: SoxCmd::Delete,
            req_id: 1,
            payload: 300u16.to_be_bytes().to_vec(),
        };
        let _ = handle_delete(&req, &mut tree);

        assert!(tree.get(300).is_none());
        assert!(tree.get(310).is_none());
        assert!(!tree.is_user_added(310));
        assert!(tree.get(301).unwrap().links.is_empty());
        assert!(tree.dirty);
        assert_eq!(tree.integrity_report(), IntegrityReport::default());
    }

    #[test]
    fn try_add_link_does_not_bound_slots_of_an_unknown_type() {
        let mut tree = s55_tree();
        // Kit 201 is not in the manifest.
        let mut unknown = s55_comp(320, 0, 1);
        unknown.kit_id = 201;
        tree.add(unknown);
        assert_eq!(tree.try_add_link(300, 1, 320, 9), Ok(()));
        assert_eq!(tree.try_add_link(320, 200, 301, 1), Ok(()));
        assert_eq!(tree.integrity_report(), IntegrityReport::default());
    }

    #[test]
    fn remove_subtree_handles_a_wide_parent_and_a_wide_fan_out() {
        let mut tree = s55_tree();
        // 2000 children under 300, each fed by 301.
        for i in 0..2000u16 {
            let id = 1000 + i;
            tree.add(s55_comp(id, 300, 3));
            assert!(tree.add_link(301, 1, id, 1));
        }
        assert_eq!(tree.get(301).unwrap().links.len(), 2000);

        let removed = tree.remove_subtree(300).unwrap();

        assert_eq!(removed.len(), 2001);
        assert_eq!(tree.len(), 2);
        assert!(tree.get(301).unwrap().links.is_empty());
        assert_eq!(tree.get(0).unwrap().children, vec![301]);
    }

    #[test]
    fn forget_component_drops_its_subscriptions_only() {
        let mut subs = SubscriptionManager::new();
        subs.subscribe(1, 300);
        subs.subscribe(2, 300);
        subs.subscribe(1, 301);

        subs.forget_component(300);

        assert!(subs.get_watchers(300).is_none());
        assert_eq!(subs.get_watchers(301).map(|s| s.len()), Some(1));
        assert_eq!(subs.total_subscriptions(), 1);
    }
}
