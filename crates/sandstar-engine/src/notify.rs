use crate::{ChannelId, EngineValue};

/// Notify subscriber identifier.
///
/// In C, this was `key_t sender` (System V IPC message queue key).
/// In Rust, we use an opaque u64 ID. IPC delivery is deferred to Phase 4.
pub type NotifyId = u64;

/// Collection of global change notification subscribers (maps C `NOTIFY` struct).
///
/// Unlike watches (per-channel), notifies are global: all subscribers are
/// notified when ANY channel value changes. This is used by the REST API
/// layer to push real-time updates.
pub struct NotifyStore {
    subscribers: Vec<NotifyId>,
}

impl NotifyStore {
    pub fn new() -> Self {
        Self {
            subscribers: Vec::new(),
        }
    }

    /// Add a subscriber for global change notifications.
    pub fn add(&mut self, subscriber: NotifyId) -> bool {
        if self.subscribers.contains(&subscriber) {
            return false;
        }
        self.subscribers.push(subscriber);
        true
    }

    /// Remove a subscriber.
    pub fn remove(&mut self, subscriber: NotifyId) -> bool {
        let before = self.subscribers.len();
        self.subscribers.retain(|&s| s != subscriber);
        self.subscribers.len() < before
    }

    /// Collect notifications for a channel value change.
    ///
    /// Returns list of (subscriber, channel, value) tuples for delivery.
    /// Every subscriber gets notified for every channel change.
    pub fn collect_notifications(
        &self,
        channel: ChannelId,
        value: &EngineValue,
    ) -> Vec<(NotifyId, ChannelId, EngineValue)> {
        self.subscribers
            .iter()
            .map(|&sub| (sub, channel, *value))
            .collect()
    }

    /// Number of registered subscribers.
    pub fn count(&self) -> usize {
        self.subscribers.len()
    }

    /// Iterate over all subscribers.
    pub fn iter(&self) -> impl Iterator<Item = &NotifyId> {
        self.subscribers.iter()
    }
}

impl Default for NotifyStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EngineStatus, EngineValue, ValueFlags};

    #[test]
    fn test_notify_add_remove() {
        let mut store = NotifyStore::new();
        assert!(store.add(1));
        assert_eq!(store.count(), 1);

        assert!(store.remove(1));
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn test_notify_duplicate() {
        let mut store = NotifyStore::new();
        assert!(store.add(1));
        assert!(!store.add(1)); // Duplicate
        assert_eq!(store.count(), 1);
    }

    #[test]
    fn test_notify_remove_nonexistent() {
        let mut store = NotifyStore::new();
        assert!(!store.remove(99));
    }

    #[test]
    fn test_notify_collect() {
        let mut store = NotifyStore::new();
        store.add(1);
        store.add(2);
        store.add(3);

        let value = EngineValue {
            status: EngineStatus::Ok,
            cur: 25.0,
            raw: 2048.0,
            flags: ValueFlags::RAW | ValueFlags::CUR,
            trigger: false,
        };

        let notifs = store.collect_notifications(1100, &value);
        assert_eq!(notifs.len(), 3);

        // All subscribers get the same channel/value
        for (_, ch, v) in &notifs {
            assert_eq!(*ch, 1100);
            assert_eq!(v.cur, 25.0);
        }
    }

    #[test]
    fn test_notify_empty() {
        let store = NotifyStore::new();
        let value = EngineValue::default();
        let notifs = store.collect_notifications(1100, &value);
        assert!(notifs.is_empty());
    }
}
