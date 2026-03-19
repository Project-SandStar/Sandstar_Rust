use std::collections::HashMap;

use crate::{ChannelId, EngineValue, Result};

/// Poll retry interval (poll cycles before retrying a failed channel).
/// At 1Hz poll rate, this is ~30 seconds.
pub const POLL_RETRY_INTERVAL: u32 = 30;

/// A single polled channel (maps C `POLL_ITEM`).
#[derive(Debug, Clone)]
pub struct PollItem {
    pub channel: ChannelId,
    pub last_value: EngineValue,
    /// Consecutive polls with unchanged value (for I2C stuck detection).
    pub unchanged_count: u32,
    /// Consecutive read failures (for I2C bus reset triggering).
    pub consecutive_fail_count: u32,
}

impl PollItem {
    pub fn new(channel: ChannelId) -> Self {
        Self {
            channel,
            last_value: EngineValue::default(),
            unchanged_count: 0,
            consecutive_fail_count: 0,
        }
    }
}

/// Collection of polled channels (maps C `POLL` struct).
///
/// The actual poll_update cycle (reading hardware, notifying watchers)
/// is deferred to Phase 3 when HAL implementations are available.
pub struct PollStore {
    items: HashMap<ChannelId, PollItem>,
}

impl PollStore {
    pub fn new() -> Self {
        Self {
            items: HashMap::new(),
        }
    }

    /// Add a channel to the poll list.
    pub fn add(&mut self, channel: ChannelId) -> Result<()> {
        if self.items.contains_key(&channel) {
            return Ok(()); // Already polling, silently succeed
        }
        self.items.insert(channel, PollItem::new(channel));
        Ok(())
    }

    /// Remove a channel from the poll list.
    pub fn remove(&mut self, channel: ChannelId) -> Result<()> {
        self.items.remove(&channel);
        Ok(())
    }

    /// Get a poll item by channel ID.
    pub fn get(&self, channel: ChannelId) -> Option<&PollItem> {
        self.items.get(&channel)
    }

    /// Get a mutable poll item by channel ID.
    pub fn get_mut(&mut self, channel: ChannelId) -> Option<&mut PollItem> {
        self.items.get_mut(&channel)
    }

    /// Check if a channel is being polled.
    pub fn contains(&self, channel: ChannelId) -> bool {
        self.items.contains_key(&channel)
    }

    /// Number of polled channels.
    pub fn count(&self) -> usize {
        self.items.len()
    }

    /// Remove all poll items. Used during config reload.
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Take a snapshot of the current poll list for rollback.
    ///
    /// Returns a clone of the internal map. Use `restore()` to put it back
    /// if a config reload fails completely.
    pub fn snapshot(&self) -> HashMap<ChannelId, PollItem> {
        self.items.clone()
    }

    /// Restore a previously saved snapshot (rollback after failed reload).
    pub fn restore(&mut self, snapshot: HashMap<ChannelId, PollItem>) {
        self.items = snapshot;
    }

    /// Iterate over all poll items.
    pub fn iter(&self) -> impl Iterator<Item = (&ChannelId, &PollItem)> {
        self.items.iter()
    }

    /// Record a value update for a polled channel.
    /// Returns true if the value changed from the previous poll.
    pub fn record_value(&mut self, channel: ChannelId, new_value: &EngineValue) -> bool {
        if let Some(item) = self.items.get_mut(&channel) {
            let changed = !item.last_value.values_equal(new_value);
            if changed {
                item.unchanged_count = 0;
            } else {
                item.unchanged_count += 1;
            }
            item.last_value = *new_value;
            item.consecutive_fail_count = 0;
            changed
        } else {
            false
        }
    }

    /// Record a read failure for a polled channel.
    pub fn record_failure(&mut self, channel: ChannelId) {
        if let Some(item) = self.items.get_mut(&channel) {
            item.consecutive_fail_count += 1;
        }
    }
}

impl Default for PollStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EngineStatus, EngineValue, ValueFlags};

    #[test]
    fn test_poll_add_remove() {
        let mut store = PollStore::new();
        store.add(1100).unwrap();
        assert_eq!(store.count(), 1);
        assert!(store.contains(1100));

        store.remove(1100).unwrap();
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn test_poll_duplicate_add() {
        let mut store = PollStore::new();
        store.add(1100).unwrap();
        store.add(1100).unwrap(); // Should succeed silently
        assert_eq!(store.count(), 1);
    }

    #[test]
    fn test_poll_record_value_changed() {
        let mut store = PollStore::new();
        store.add(1100).unwrap();

        let mut value = EngineValue::default();
        value.status = EngineStatus::Ok;
        value.set_cur(25.0);

        assert!(store.record_value(1100, &value));

        // Same value -> not changed
        assert!(!store.record_value(1100, &value));

        // Different value -> changed
        value.set_cur(26.0);
        assert!(store.record_value(1100, &value));
    }

    #[test]
    fn test_poll_unchanged_count() {
        let mut store = PollStore::new();
        store.add(1100).unwrap();

        let value = EngineValue {
            status: EngineStatus::Ok,
            cur: 25.0,
            raw: 0.0,
            flags: ValueFlags::CUR,
            trigger: false,
        };

        store.record_value(1100, &value); // First time -> changed
        store.record_value(1100, &value); // Same -> unchanged (1)
        store.record_value(1100, &value); // Same -> unchanged (2)

        assert_eq!(store.get(1100).unwrap().unchanged_count, 2);
    }

    #[test]
    fn test_poll_record_failure() {
        let mut store = PollStore::new();
        store.add(1100).unwrap();

        store.record_failure(1100);
        store.record_failure(1100);

        assert_eq!(store.get(1100).unwrap().consecutive_fail_count, 2);
    }

    #[test]
    fn test_poll_failure_resets_on_success() {
        let mut store = PollStore::new();
        store.add(1100).unwrap();

        store.record_failure(1100);
        store.record_failure(1100);

        let value = EngineValue::default();
        store.record_value(1100, &value);

        assert_eq!(store.get(1100).unwrap().consecutive_fail_count, 0);
    }

    #[test]
    fn test_poll_clear() {
        let mut store = PollStore::new();
        store.add(1100).unwrap();
        store.add(1200).unwrap();
        assert_eq!(store.count(), 2);

        store.clear();
        assert_eq!(store.count(), 0);
        assert!(!store.contains(1100));
        assert!(!store.contains(1200));
    }

    // ========================================================================
    // snapshot / restore tests
    // ========================================================================

    #[test]
    fn test_snapshot_empty() {
        // Snapshot of empty PollStore, restore is a no-op.
        let mut store = PollStore::new();
        let snap = store.snapshot();
        assert!(snap.is_empty());

        store.restore(snap);
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn test_snapshot_preserves_state() {
        // Add polls, record values, snapshot, modify state, restore,
        // verify original state returned.
        let mut store = PollStore::new();
        store.add(1100).unwrap();
        store.add(1200).unwrap();

        let value = EngineValue {
            status: EngineStatus::Ok,
            cur: 72.5,
            raw: 2048.0,
            flags: ValueFlags::CUR,
            trigger: false,
        };
        store.record_value(1100, &value);

        // Take snapshot
        let snap = store.snapshot();

        // Modify state: remove a poll, add a new one, record different value
        store.remove(1100).unwrap();
        store.add(1300).unwrap();
        let new_value = EngineValue {
            status: EngineStatus::Ok,
            cur: 99.0,
            raw: 4000.0,
            flags: ValueFlags::CUR,
            trigger: false,
        };
        store.record_value(1200, &new_value);

        // Restore
        store.restore(snap);

        // Original state: 1100 and 1200 present, 1300 gone
        assert_eq!(store.count(), 2);
        assert!(store.contains(1100));
        assert!(store.contains(1200));
        assert!(!store.contains(1300));

        // 1100 should have the value from before snapshot
        let item = store.get(1100).unwrap();
        assert_eq!(item.last_value.cur, 72.5);
    }

    #[test]
    fn test_snapshot_restore_idempotent() {
        // Snapshot, restore twice, state is the same.
        let mut store = PollStore::new();
        store.add(1100).unwrap();
        store.add(1200).unwrap();

        let value = EngineValue {
            status: EngineStatus::Ok,
            cur: 25.0,
            raw: 1024.0,
            flags: ValueFlags::CUR,
            trigger: false,
        };
        store.record_value(1100, &value);

        let snap = store.snapshot();

        // First restore
        store.clear();
        store.restore(snap.clone());
        assert_eq!(store.count(), 2);
        assert_eq!(store.get(1100).unwrap().last_value.cur, 25.0);

        // Second restore (from same snapshot)
        store.clear();
        store.restore(snap);
        assert_eq!(store.count(), 2);
        assert_eq!(store.get(1100).unwrap().last_value.cur, 25.0);
    }

    #[test]
    fn test_snapshot_with_active_polls() {
        // Snapshot while polls have recorded values and failure counts,
        // restore resets to snapshot point.
        let mut store = PollStore::new();
        store.add(1100).unwrap();

        let value = EngineValue {
            status: EngineStatus::Ok,
            cur: 50.0,
            raw: 2048.0,
            flags: ValueFlags::CUR,
            trigger: false,
        };
        store.record_value(1100, &value);
        store.record_value(1100, &value); // unchanged_count = 1
        store.record_failure(1100); // This resets on next record_value, but bump fail count here

        // Actually, record_failure bumps consecutive_fail_count.
        // record_value resets consecutive_fail_count.
        // Let's re-do: record a value (resets fail), then record failures
        let val2 = EngineValue {
            status: EngineStatus::Ok,
            cur: 51.0,
            raw: 2050.0,
            flags: ValueFlags::CUR,
            trigger: false,
        };
        store.record_value(1100, &val2);
        store.record_failure(1100);
        store.record_failure(1100);

        assert_eq!(store.get(1100).unwrap().consecutive_fail_count, 2);
        assert_eq!(store.get(1100).unwrap().last_value.cur, 51.0);

        let snap = store.snapshot();

        // Modify: more failures, different value
        store.record_failure(1100);
        store.record_failure(1100);
        let val3 = EngineValue {
            status: EngineStatus::Ok,
            cur: 99.0,
            raw: 4000.0,
            flags: ValueFlags::CUR,
            trigger: false,
        };
        store.record_value(1100, &val3);

        // Restore to snapshot point
        store.restore(snap);

        let item = store.get(1100).unwrap();
        assert_eq!(item.last_value.cur, 51.0);
        assert_eq!(item.consecutive_fail_count, 2);
    }

    #[test]
    fn test_restore_after_failed_reload() {
        // Simulate: snapshot -> modify polls (add/remove) -> restore
        // -> verify original poll set intact.
        let mut store = PollStore::new();
        store.add(1100).unwrap();
        store.add(1200).unwrap();
        store.add(1300).unwrap();

        let value = EngineValue {
            status: EngineStatus::Ok,
            cur: 42.0,
            raw: 1700.0,
            flags: ValueFlags::CUR,
            trigger: false,
        };
        store.record_value(1100, &value);
        store.record_value(1200, &value);
        store.record_value(1300, &value);

        // Snapshot before "reload"
        let snap = store.snapshot();
        assert_eq!(snap.len(), 3);

        // Simulate a failed reload: clear all, add different set
        store.clear();
        assert_eq!(store.count(), 0);
        store.add(2000).unwrap();
        store.add(2100).unwrap();
        assert_eq!(store.count(), 2);

        // Reload failed — rollback
        store.restore(snap);

        // Original set restored
        assert_eq!(store.count(), 3);
        assert!(store.contains(1100));
        assert!(store.contains(1200));
        assert!(store.contains(1300));
        assert!(!store.contains(2000));
        assert!(!store.contains(2100));

        // Values preserved
        assert_eq!(store.get(1100).unwrap().last_value.cur, 42.0);
        assert_eq!(store.get(1200).unwrap().last_value.cur, 42.0);
        assert_eq!(store.get(1300).unwrap().last_value.cur, 42.0);
    }
}
