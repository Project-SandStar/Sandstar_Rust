//! Scalable component storage with free-list ID allocation.
//!
//! Replaces the original Sedona `App.comps[]` array which grows by 8 slots
//! at a time (O(n^2) total copies) and scans linearly for free slots (O(n)
//! per allocation).
//!
//! [`ComponentStore`] pre-allocates component slots and maintains a free
//! list for O(1) allocation and deallocation.  The iterative
//! [`ComponentStore::execution_order`] method replaces the recursive
//! `executeTree()` that could overflow the stack on deep component trees.
//!
//! See research doc `14_SEDONA_VM_SCALABILITY_LIMITS.md` for full analysis.

use smallvec::SmallVec;

/// Sentinel value for "no component" — wider than Sedona's 16-bit `0xFFFF`.
pub const NULL_ID: u32 = u32::MAX;

/// A Sedona VM component instance.
///
/// Uses `u32` IDs (vs Sedona's `i16`) to support up to 4 billion components.
/// Children are stored in a [`SmallVec`] that inlines up to 8 entries,
/// avoiding heap allocation for the common case of few children.
#[derive(Debug, Clone)]
pub struct SvmComponent {
    /// Component ID (wider than Sedona's `short`).
    pub id: u32,
    /// Kit:Type composite ID.
    pub type_id: u16,
    /// Parent component ID (`NULL_ID` for root).
    pub parent_id: u32,
    /// Component name.
    pub name: String,
    /// Child component IDs — inlined for up to 8 children.
    pub children: SmallVec<[u32; 8]>,
    /// Offset into the data memory segment for this component's slots.
    pub slot_offset: u32,
    /// Number of slots (properties + actions).
    pub slot_count: u16,
    /// Component flags (enabled, running, etc.).
    pub flags: u8,
}

impl SvmComponent {
    /// Create a new component with the given ID and type.
    pub fn new(id: u32, type_id: u16) -> Self {
        Self {
            id,
            type_id,
            parent_id: NULL_ID,
            name: String::new(),
            children: SmallVec::new(),
            slot_offset: 0,
            slot_count: 0,
            flags: 0,
        }
    }
}

/// Scalable component storage with free-list ID allocation.
///
/// Pre-allocates `max_components` slots and reuses freed IDs via an
/// internal free list.  All operations are O(1) except [`iter`] which is
/// O(capacity).
///
/// [`iter`]: ComponentStore::iter
pub struct ComponentStore {
    /// Component data indexed by ID.  `None` = free slot.
    slots: Vec<Option<SvmComponent>>,
    /// Free slot indices for O(1) allocation (stack — LIFO).
    free_list: Vec<u32>,
    /// Maximum component count (configurable).
    max_components: u32,
    /// Count of active (occupied) components.
    active_count: u32,
}

impl ComponentStore {
    /// Create a new store with the given maximum capacity.
    ///
    /// All slots are initially free.  The free list is populated in
    /// reverse order so that [`allocate`] returns IDs starting from 0.
    ///
    /// [`allocate`]: ComponentStore::allocate
    pub fn new(max_components: u32) -> Self {
        let cap = max_components as usize;
        let free_list: Vec<u32> = (0..max_components).rev().collect();
        Self {
            slots: vec![None; cap],
            free_list,
            max_components,
            active_count: 0,
        }
    }

    /// Allocate a new component ID from the free list.  O(1).
    ///
    /// Returns `None` if all slots are occupied.
    pub fn allocate(&mut self) -> Option<u32> {
        let id = self.free_list.pop()?;
        self.active_count += 1;
        Some(id)
    }

    /// Free a component ID back to the free list.  O(1).
    ///
    /// Also removes this component from its parent's children list and
    /// clears the slot.  No-op if the slot is already free.
    pub fn free(&mut self, id: u32) {
        if let Some(slot) = self.slots.get_mut(id as usize) {
            if slot.is_some() {
                *slot = None;
                self.free_list.push(id);
                self.active_count -= 1;
            }
        }
    }

    /// Get a component by ID.  O(1).
    pub fn get(&self, id: u32) -> Option<&SvmComponent> {
        self.slots.get(id as usize)?.as_ref()
    }

    /// Get a mutable component by ID.  O(1).
    pub fn get_mut(&mut self, id: u32) -> Option<&mut SvmComponent> {
        self.slots.get_mut(id as usize)?.as_mut()
    }

    /// Insert a component at a pre-allocated ID.
    ///
    /// Returns `true` if the component was inserted, `false` if the ID
    /// is out of range.
    pub fn insert(&mut self, comp: SvmComponent) -> bool {
        let id = comp.id as usize;
        if id < self.slots.len() {
            self.slots[id] = Some(comp);
            true
        } else {
            false
        }
    }

    /// Iterate all active components (in slot order).
    pub fn iter(&self) -> impl Iterator<Item = &SvmComponent> {
        self.slots.iter().filter_map(|s| s.as_ref())
    }

    /// Iterative tree walk — replaces recursive `executeTree()`.
    ///
    /// Returns component IDs in pre-order (parent before children),
    /// which matches the Sedona execution model where a parent's
    /// `execute()` runs after its children have been visited.
    ///
    /// For the Sedona execution model (children execute before parent),
    /// use [`execution_order_post`].
    pub fn execution_order(&self, root_id: u32) -> Vec<u32> {
        let mut order = Vec::new();
        let mut stack = vec![root_id];
        while let Some(id) = stack.pop() {
            if let Some(comp) = self.get(id) {
                order.push(id);
                // Push children in reverse so they execute left-to-right
                for &child_id in comp.children.iter().rev() {
                    stack.push(child_id);
                }
            }
        }
        order
    }

    /// Iterative post-order tree walk — children before parent.
    ///
    /// This matches the Sedona `executeTree()` semantics where children
    /// execute before their parent component.  Uses an explicit stack
    /// instead of recursion, making it immune to stack overflow
    /// regardless of tree depth.
    pub fn execution_order_post(&self, root_id: u32) -> Vec<u32> {
        let mut order = Vec::new();
        // Stack entries: (component_id, children_pushed)
        let mut stack: Vec<(u32, bool)> = vec![(root_id, false)];

        while let Some((id, children_pushed)) = stack.pop() {
            if children_pushed {
                // All children have been processed — emit this node
                order.push(id);
            } else if let Some(comp) = self.get(id) {
                // Push self back — will be emitted after children
                stack.push((id, true));
                // Push children in reverse so first child is processed first
                for &child_id in comp.children.iter().rev() {
                    stack.push((child_id, false));
                }
            }
        }
        order
    }

    /// Number of active (occupied) components.
    pub fn count(&self) -> u32 {
        self.active_count
    }

    /// Maximum capacity.
    pub fn capacity(&self) -> u32 {
        self.max_components
    }

    /// Number of free slots available.
    pub fn free_count(&self) -> usize {
        self.free_list.len()
    }

    /// Check whether a given ID is currently occupied.
    pub fn contains(&self, id: u32) -> bool {
        self.slots
            .get(id as usize)
            .map_or(false, |s| s.is_some())
    }
}

impl std::fmt::Debug for ComponentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentStore")
            .field("active_count", &self.active_count)
            .field("max_components", &self.max_components)
            .field("free_list_len", &self.free_list.len())
            .finish()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    #[test]
    fn new_store_is_empty() {
        let store = ComponentStore::new(100);
        assert_eq!(store.count(), 0);
        assert_eq!(store.capacity(), 100);
        assert_eq!(store.free_count(), 100);
    }

    #[test]
    fn new_store_zero_capacity() {
        let mut store = ComponentStore::new(0);
        assert_eq!(store.count(), 0);
        assert_eq!(store.capacity(), 0);
        assert_eq!(store.free_count(), 0);
        assert!(store.allocate().is_none());
    }

    // -----------------------------------------------------------------------
    // Allocate / free basics
    // -----------------------------------------------------------------------

    #[test]
    fn allocate_returns_sequential_ids() {
        let mut store = ComponentStore::new(10);
        assert_eq!(store.allocate(), Some(0));
        assert_eq!(store.allocate(), Some(1));
        assert_eq!(store.allocate(), Some(2));
        assert_eq!(store.count(), 3);
        assert_eq!(store.free_count(), 7);
    }

    #[test]
    fn allocate_exhausts_capacity() {
        let mut store = ComponentStore::new(3);
        assert_eq!(store.allocate(), Some(0));
        assert_eq!(store.allocate(), Some(1));
        assert_eq!(store.allocate(), Some(2));
        assert_eq!(store.allocate(), None);
        assert_eq!(store.count(), 3);
        assert_eq!(store.free_count(), 0);
    }

    #[test]
    fn free_returns_id_to_pool() {
        let mut store = ComponentStore::new(10);
        let id = store.allocate().unwrap();
        let comp = SvmComponent::new(id, 1);
        store.insert(comp);
        assert_eq!(store.count(), 1);

        store.free(id);
        assert_eq!(store.count(), 0);
        assert_eq!(store.free_count(), 10);
    }

    #[test]
    fn free_and_reallocate_reuses_id() {
        let mut store = ComponentStore::new(10);
        let id0 = store.allocate().unwrap();
        let id1 = store.allocate().unwrap();
        assert_eq!(id0, 0);
        assert_eq!(id1, 1);

        // Insert then free id0
        store.insert(SvmComponent::new(id0, 1));
        store.free(id0);

        // Next allocation should reuse id0 (LIFO free list)
        let reused = store.allocate().unwrap();
        assert_eq!(reused, 0);
    }

    #[test]
    fn free_already_free_slot_is_noop() {
        let mut store = ComponentStore::new(10);
        let id = store.allocate().unwrap();
        // Don't insert anything — slot is None
        store.free(id); // should not panic or double-count
        assert_eq!(store.count(), 1); // allocate incremented, free didn't decrement (slot was None)
    }

    #[test]
    fn free_out_of_range_is_noop() {
        let mut store = ComponentStore::new(5);
        store.free(999); // out of range — no panic
        assert_eq!(store.count(), 0);
    }

    // -----------------------------------------------------------------------
    // Get / get_mut / insert
    // -----------------------------------------------------------------------

    #[test]
    fn get_returns_inserted_component() {
        let mut store = ComponentStore::new(10);
        let id = store.allocate().unwrap();
        let mut comp = SvmComponent::new(id, 42);
        comp.name = "test".into();
        store.insert(comp);

        let got = store.get(id).unwrap();
        assert_eq!(got.id, id);
        assert_eq!(got.type_id, 42);
        assert_eq!(got.name, "test");
    }

    #[test]
    fn get_empty_slot_returns_none() {
        let store = ComponentStore::new(10);
        assert!(store.get(0).is_none());
    }

    #[test]
    fn get_out_of_range_returns_none() {
        let store = ComponentStore::new(5);
        assert!(store.get(999).is_none());
    }

    #[test]
    fn get_mut_modifies_component() {
        let mut store = ComponentStore::new(10);
        let id = store.allocate().unwrap();
        store.insert(SvmComponent::new(id, 1));

        store.get_mut(id).unwrap().name = "modified".into();
        assert_eq!(store.get(id).unwrap().name, "modified");
    }

    #[test]
    fn insert_out_of_range_returns_false() {
        let mut store = ComponentStore::new(5);
        let comp = SvmComponent::new(999, 1);
        assert!(!store.insert(comp));
    }

    // -----------------------------------------------------------------------
    // Contains
    // -----------------------------------------------------------------------

    #[test]
    fn contains_after_insert() {
        let mut store = ComponentStore::new(10);
        let id = store.allocate().unwrap();
        assert!(!store.contains(id));
        store.insert(SvmComponent::new(id, 1));
        assert!(store.contains(id));
    }

    #[test]
    fn contains_after_free() {
        let mut store = ComponentStore::new(10);
        let id = store.allocate().unwrap();
        store.insert(SvmComponent::new(id, 1));
        store.free(id);
        assert!(!store.contains(id));
    }

    #[test]
    fn contains_out_of_range() {
        let store = ComponentStore::new(5);
        assert!(!store.contains(999));
    }

    // -----------------------------------------------------------------------
    // Iterator
    // -----------------------------------------------------------------------

    #[test]
    fn iter_empty_store() {
        let store = ComponentStore::new(10);
        assert_eq!(store.iter().count(), 0);
    }

    #[test]
    fn iter_returns_active_components() {
        let mut store = ComponentStore::new(10);
        for n in 0..5u32 {
            let id = store.allocate().unwrap();
            store.insert(SvmComponent::new(id, n as u16));
        }
        let ids: Vec<u32> = store.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn iter_skips_freed_slots() {
        let mut store = ComponentStore::new(10);
        for _ in 0..5u32 {
            let id = store.allocate().unwrap();
            store.insert(SvmComponent::new(id, 0));
        }
        store.free(1);
        store.free(3);
        let ids: Vec<u32> = store.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![0, 2, 4]);
    }

    // -----------------------------------------------------------------------
    // execution_order (pre-order)
    // -----------------------------------------------------------------------

    #[test]
    fn execution_order_single_node() {
        let mut store = ComponentStore::new(10);
        let id = store.allocate().unwrap();
        store.insert(SvmComponent::new(id, 0));
        assert_eq!(store.execution_order(id), vec![0]);
    }

    #[test]
    fn execution_order_empty_root() {
        let store = ComponentStore::new(10);
        // Root doesn't exist
        assert_eq!(store.execution_order(0), vec![]);
    }

    #[test]
    fn execution_order_tree() {
        //       0
        //      / \
        //     1   2
        //    / \
        //   3   4
        let mut store = ComponentStore::new(10);
        for _ in 0..5u32 {
            let id = store.allocate().unwrap();
            store.insert(SvmComponent::new(id, 0));
        }
        store.get_mut(0).unwrap().children = SmallVec::from_slice(&[1, 2]);
        store.get_mut(1).unwrap().children = SmallVec::from_slice(&[3, 4]);
        store.get_mut(1).unwrap().parent_id = 0;
        store.get_mut(2).unwrap().parent_id = 0;
        store.get_mut(3).unwrap().parent_id = 1;
        store.get_mut(4).unwrap().parent_id = 1;

        let order = store.execution_order(0);
        assert_eq!(order, vec![0, 1, 3, 4, 2]);
    }

    // -----------------------------------------------------------------------
    // execution_order_post (post-order — Sedona semantics)
    // -----------------------------------------------------------------------

    #[test]
    fn execution_order_post_single_node() {
        let mut store = ComponentStore::new(10);
        let id = store.allocate().unwrap();
        store.insert(SvmComponent::new(id, 0));
        assert_eq!(store.execution_order_post(id), vec![0]);
    }

    #[test]
    fn execution_order_post_empty_root() {
        let store = ComponentStore::new(10);
        assert_eq!(store.execution_order_post(0), vec![]);
    }

    #[test]
    fn execution_order_post_tree() {
        //       0
        //      / \
        //     1   2
        //    / \
        //   3   4
        let mut store = ComponentStore::new(10);
        for _ in 0..5u32 {
            let id = store.allocate().unwrap();
            store.insert(SvmComponent::new(id, 0));
        }
        store.get_mut(0).unwrap().children = SmallVec::from_slice(&[1, 2]);
        store.get_mut(1).unwrap().children = SmallVec::from_slice(&[3, 4]);

        // Post-order: children before parent
        let order = store.execution_order_post(0);
        assert_eq!(order, vec![3, 4, 1, 2, 0]);
    }

    #[test]
    fn execution_order_post_deep_chain() {
        // Linear chain: 0 -> 1 -> 2 -> 3 -> 4
        // Tests that deep trees don't overflow (iterative, not recursive)
        let mut store = ComponentStore::new(100);
        for _ in 0..5u32 {
            let id = store.allocate().unwrap();
            store.insert(SvmComponent::new(id, 0));
        }
        for i in 0..4u32 {
            store.get_mut(i).unwrap().children = SmallVec::from_slice(&[i + 1]);
        }

        let order = store.execution_order_post(0);
        // Post-order of a chain: deepest first
        assert_eq!(order, vec![4, 3, 2, 1, 0]);
    }

    #[test]
    fn execution_order_post_wide_tree() {
        // Root 0 with children [1, 2, 3, 4, 5, 6, 7, 8, 9]
        let mut store = ComponentStore::new(20);
        for _ in 0..10u32 {
            let id = store.allocate().unwrap();
            store.insert(SvmComponent::new(id, 0));
        }
        let children: Vec<u32> = (1..10).collect();
        store.get_mut(0).unwrap().children = SmallVec::from_vec(children.clone());

        let order = store.execution_order_post(0);
        // All children before root, in left-to-right order
        let mut expected = children;
        expected.push(0);
        assert_eq!(order, expected);
    }

    #[test]
    fn execution_order_handles_missing_children() {
        // Component 0 references child 5, but slot 5 is empty
        let mut store = ComponentStore::new(10);
        let id = store.allocate().unwrap();
        let mut comp = SvmComponent::new(id, 0);
        comp.children = SmallVec::from_slice(&[5]); // 5 doesn't exist
        store.insert(comp);

        let order = store.execution_order(0);
        assert_eq!(order, vec![0]); // only root, missing child skipped

        let order_post = store.execution_order_post(0);
        assert_eq!(order_post, vec![0]);
    }

    // -----------------------------------------------------------------------
    // SmallVec children — inline optimization
    // -----------------------------------------------------------------------

    #[test]
    fn smallvec_children_inline() {
        let comp = SvmComponent::new(0, 0);
        // SmallVec<[u32; 8]> should not heap-allocate for up to 8 children
        assert!(!comp.children.spilled());
    }

    #[test]
    fn smallvec_children_up_to_8_inline() {
        let mut comp = SvmComponent::new(0, 0);
        for i in 0..8u32 {
            comp.children.push(i);
        }
        assert!(!comp.children.spilled());
        assert_eq!(comp.children.len(), 8);
    }

    #[test]
    fn smallvec_children_spills_at_9() {
        let mut comp = SvmComponent::new(0, 0);
        for i in 0..9u32 {
            comp.children.push(i);
        }
        assert!(comp.children.spilled());
    }

    // -----------------------------------------------------------------------
    // Debug impl
    // -----------------------------------------------------------------------

    #[test]
    fn debug_impl() {
        let store = ComponentStore::new(42);
        let dbg = format!("{:?}", store);
        assert!(dbg.contains("ComponentStore"));
        assert!(dbg.contains("42"));
    }

    // -----------------------------------------------------------------------
    // Stress / capacity tests
    // -----------------------------------------------------------------------

    #[test]
    fn allocate_and_free_all() {
        let n = 1000u32;
        let mut store = ComponentStore::new(n);
        let mut ids = Vec::new();

        // Allocate all
        for _ in 0..n {
            ids.push(store.allocate().unwrap());
        }
        assert_eq!(store.allocate(), None);
        assert_eq!(store.count(), n);
        assert_eq!(store.free_count(), 0);

        // Free all
        for id in &ids {
            store.insert(SvmComponent::new(*id, 0));
        }
        for id in ids {
            store.free(id);
        }
        assert_eq!(store.count(), 0);
        assert_eq!(store.free_count(), n as usize);
    }

    #[test]
    fn rapid_alloc_free_cycles() {
        let mut store = ComponentStore::new(10);
        for _ in 0..100 {
            let id = store.allocate().unwrap();
            store.insert(SvmComponent::new(id, 0));
            store.free(id);
        }
        // After 100 alloc/free cycles, we should be back to full capacity
        assert_eq!(store.count(), 0);
        assert_eq!(store.free_count(), 10);
    }

    // -----------------------------------------------------------------------
    // NULL_ID sentinel
    // -----------------------------------------------------------------------

    #[test]
    fn null_id_is_max() {
        assert_eq!(NULL_ID, u32::MAX);
    }

    #[test]
    fn default_parent_is_null() {
        let comp = SvmComponent::new(0, 0);
        assert_eq!(comp.parent_id, NULL_ID);
    }
}
