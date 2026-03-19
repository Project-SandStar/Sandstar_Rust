use crate::{ChannelId, EngineValue};

/// Watch subscriber identifier.
///
/// In the C code, this was `key_t sender` (System V IPC message queue key).
/// In Rust, we use an opaque u64 ID. The actual IPC delivery is deferred to Phase 4.
pub type WatchId = u64;

/// A single watch subscription (maps C `WATCH_ITEM`).
#[derive(Debug, Clone)]
pub struct WatchItem {
    pub channel: ChannelId,
    pub subscriber: WatchId,
}

/// Collection of watch subscriptions (maps C `WATCH` struct).
///
/// When a channel value changes, all subscribers watching that channel
/// are notified. The notification delivery mechanism (IPC) is deferred
/// to Phase 4.
pub struct WatchStore {
    items: Vec<WatchItem>,
}

impl WatchStore {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// Add a watch on a channel for a subscriber.
    pub fn add(&mut self, channel: ChannelId, subscriber: WatchId) -> bool {
        // Check for duplicate
        if self
            .items
            .iter()
            .any(|w| w.channel == channel && w.subscriber == subscriber)
        {
            return false;
        }

        self.items.push(WatchItem {
            channel,
            subscriber,
        });
        true
    }

    /// Remove a watch on a channel for a subscriber.
    pub fn remove(&mut self, channel: ChannelId, subscriber: WatchId) -> bool {
        let before = self.items.len();
        self.items
            .retain(|w| !(w.channel == channel && w.subscriber == subscriber));
        self.items.len() < before
    }

    /// Get all subscribers watching a specific channel.
    pub fn subscribers_for(&self, channel: ChannelId) -> Vec<WatchId> {
        self.items
            .iter()
            .filter(|w| w.channel == channel)
            .map(|w| w.subscriber)
            .collect()
    }

    /// Collect notifications for a channel value change.
    ///
    /// Returns list of (subscriber, channel, value) tuples for delivery.
    /// Actual IPC delivery is handled by the caller (deferred to Phase 4).
    pub fn collect_notifications(
        &self,
        channel: ChannelId,
        value: &EngineValue,
    ) -> Vec<(WatchId, ChannelId, EngineValue)> {
        self.subscribers_for(channel)
            .into_iter()
            .map(|sub| (sub, channel, *value))
            .collect()
    }

    /// Number of active watches.
    pub fn count(&self) -> usize {
        self.items.len()
    }

    /// Iterate over all watches.
    pub fn iter(&self) -> impl Iterator<Item = &WatchItem> {
        self.items.iter()
    }
}

impl Default for WatchStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EngineStatus, EngineValue, ValueFlags};

    #[test]
    fn test_watch_add_remove() {
        let mut store = WatchStore::new();
        assert!(store.add(1100, 1));
        assert_eq!(store.count(), 1);

        assert!(store.remove(1100, 1));
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn test_watch_duplicate() {
        let mut store = WatchStore::new();
        assert!(store.add(1100, 1));
        assert!(!store.add(1100, 1)); // Duplicate
        assert_eq!(store.count(), 1);
    }

    #[test]
    fn test_watch_remove_nonexistent() {
        let mut store = WatchStore::new();
        assert!(!store.remove(1100, 1));
    }

    #[test]
    fn test_watch_subscribers_for() {
        let mut store = WatchStore::new();
        store.add(1100, 1);
        store.add(1100, 2);
        store.add(1200, 3);

        let subs = store.subscribers_for(1100);
        assert_eq!(subs.len(), 2);
        assert!(subs.contains(&1));
        assert!(subs.contains(&2));

        let subs = store.subscribers_for(1200);
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn test_watch_collect_notifications() {
        let mut store = WatchStore::new();
        store.add(1100, 1);
        store.add(1100, 2);

        let value = EngineValue {
            status: EngineStatus::Ok,
            cur: 25.0,
            raw: 2048.0,
            flags: ValueFlags::RAW | ValueFlags::CUR,
            trigger: false,
        };

        let notifs = store.collect_notifications(1100, &value);
        assert_eq!(notifs.len(), 2);
    }

    #[test]
    fn test_watch_multiple_channels() {
        let mut store = WatchStore::new();
        store.add(1100, 1);
        store.add(1200, 1); // Same subscriber, different channels

        assert_eq!(store.count(), 2);
        assert_eq!(store.subscribers_for(1100).len(), 1);
        assert_eq!(store.subscribers_for(1200).len(), 1);
    }
}
