//! COV (Change of Value) subscription manager for driver points.
//!
//! Tracks which subscribers are watching which points, enabling
//! efficient fan-out of value change notifications. Used by the
//! [`DriverManager`] to call [`Driver::on_watch`] and
//! [`Driver::on_unwatch`] when subscription sets change.

use std::collections::{HashMap, HashSet};

// ── DriverWatchManager ─────────────────────────────────────

/// Manages COV (Change of Value) subscriptions for driver points.
///
/// Maintains a bidirectional mapping between subscribers and points,
/// allowing efficient lookup in both directions:
/// - Given a point, find all subscribers
/// - Given a subscriber, find all watched points
pub struct DriverWatchManager {
    /// point_id -> set of subscriber IDs
    watches: HashMap<u32, HashSet<String>>,
    /// subscriber_id -> set of point_ids
    subscribers: HashMap<String, HashSet<u32>>,
}

impl DriverWatchManager {
    /// Create a new empty watch manager.
    pub fn new() -> Self {
        Self {
            watches: HashMap::new(),
            subscribers: HashMap::new(),
        }
    }

    /// Subscribe a client to point changes.
    ///
    /// Adds the subscriber to each specified point's watch set, and
    /// records the reverse mapping for cleanup.
    pub fn subscribe(&mut self, subscriber_id: &str, point_ids: &[u32]) {
        let sub_set = self
            .subscribers
            .entry(subscriber_id.to_string())
            .or_default();

        for &pid in point_ids {
            self.watches
                .entry(pid)
                .or_default()
                .insert(subscriber_id.to_string());
            sub_set.insert(pid);
        }
    }

    /// Unsubscribe a client from specific points.
    ///
    /// If `point_ids` is empty, unsubscribes from all points
    /// (equivalent to [`remove_subscriber`]).
    pub fn unsubscribe(&mut self, subscriber_id: &str, point_ids: &[u32]) {
        if point_ids.is_empty() {
            self.remove_subscriber(subscriber_id);
            return;
        }

        for &pid in point_ids {
            if let Some(subs) = self.watches.get_mut(&pid) {
                subs.remove(subscriber_id);
                if subs.is_empty() {
                    self.watches.remove(&pid);
                }
            }
        }

        if let Some(sub_set) = self.subscribers.get_mut(subscriber_id) {
            for &pid in point_ids {
                sub_set.remove(&pid);
            }
            if sub_set.is_empty() {
                self.subscribers.remove(subscriber_id);
            }
        }
    }

    /// Remove all subscriptions for a subscriber (e.g., on disconnect).
    pub fn remove_subscriber(&mut self, subscriber_id: &str) {
        if let Some(points) = self.subscribers.remove(subscriber_id) {
            for pid in points {
                if let Some(subs) = self.watches.get_mut(&pid) {
                    subs.remove(subscriber_id);
                    if subs.is_empty() {
                        self.watches.remove(&pid);
                    }
                }
            }
        }
    }

    /// Get all subscribers watching a given point.
    pub fn subscribers_for(&self, point_id: u32) -> HashSet<String> {
        self.watches
            .get(&point_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Check if any subscriber is watching this point.
    pub fn is_watched(&self, point_id: u32) -> bool {
        self.watches
            .get(&point_id)
            .map_or(false, |s| !s.is_empty())
    }

    /// Total number of active watch entries (sum of all point->subscriber pairs).
    pub fn watch_count(&self) -> usize {
        self.watches.values().map(|s| s.len()).sum()
    }

    /// Number of unique subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }

    /// Number of unique watched points.
    pub fn watched_point_count(&self) -> usize {
        self.watches.len()
    }

    /// Get all point IDs being watched by a subscriber.
    pub fn points_for_subscriber(&self, subscriber_id: &str) -> HashSet<u32> {
        self.subscribers
            .get(subscriber_id)
            .cloned()
            .unwrap_or_default()
    }
}

impl Default for DriverWatchManager {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_manager_is_empty() {
        let m = DriverWatchManager::new();
        assert_eq!(m.watch_count(), 0);
        assert_eq!(m.subscriber_count(), 0);
        assert_eq!(m.watched_point_count(), 0);
    }

    #[test]
    fn subscribe_single_point() {
        let mut m = DriverWatchManager::new();
        m.subscribe("client-1", &[100]);

        assert!(m.is_watched(100));
        assert!(!m.is_watched(200));
        assert_eq!(m.watch_count(), 1);
        assert_eq!(m.subscriber_count(), 1);
        assert_eq!(m.watched_point_count(), 1);

        let subs = m.subscribers_for(100);
        assert!(subs.contains("client-1"));
    }

    #[test]
    fn subscribe_multiple_points() {
        let mut m = DriverWatchManager::new();
        m.subscribe("client-1", &[100, 200, 300]);

        assert!(m.is_watched(100));
        assert!(m.is_watched(200));
        assert!(m.is_watched(300));
        assert_eq!(m.watch_count(), 3);
        assert_eq!(m.watched_point_count(), 3);
    }

    #[test]
    fn multiple_subscribers_same_point() {
        let mut m = DriverWatchManager::new();
        m.subscribe("client-1", &[100]);
        m.subscribe("client-2", &[100]);

        assert_eq!(m.watch_count(), 2);
        assert_eq!(m.subscriber_count(), 2);
        assert_eq!(m.watched_point_count(), 1); // same point

        let subs = m.subscribers_for(100);
        assert_eq!(subs.len(), 2);
        assert!(subs.contains("client-1"));
        assert!(subs.contains("client-2"));
    }

    #[test]
    fn unsubscribe_specific_points() {
        let mut m = DriverWatchManager::new();
        m.subscribe("client-1", &[100, 200, 300]);

        m.unsubscribe("client-1", &[200]);
        assert!(m.is_watched(100));
        assert!(!m.is_watched(200));
        assert!(m.is_watched(300));
        assert_eq!(m.watch_count(), 2);
    }

    #[test]
    fn unsubscribe_empty_removes_all() {
        let mut m = DriverWatchManager::new();
        m.subscribe("client-1", &[100, 200]);

        m.unsubscribe("client-1", &[]);
        assert!(!m.is_watched(100));
        assert!(!m.is_watched(200));
        assert_eq!(m.watch_count(), 0);
        assert_eq!(m.subscriber_count(), 0);
    }

    #[test]
    fn remove_subscriber_cleans_all() {
        let mut m = DriverWatchManager::new();
        m.subscribe("client-1", &[100, 200]);
        m.subscribe("client-2", &[100]);

        m.remove_subscriber("client-1");

        assert!(m.is_watched(100)); // client-2 still watching
        assert!(!m.is_watched(200)); // only client-1 was watching
        assert_eq!(m.subscriber_count(), 1);
        assert_eq!(m.watch_count(), 1);
    }

    #[test]
    fn remove_nonexistent_subscriber_is_noop() {
        let mut m = DriverWatchManager::new();
        m.subscribe("client-1", &[100]);
        m.remove_subscriber("nonexistent");
        assert_eq!(m.watch_count(), 1);
    }

    #[test]
    fn subscribers_for_unwatched_point() {
        let m = DriverWatchManager::new();
        assert!(m.subscribers_for(999).is_empty());
    }

    #[test]
    fn is_watched_false_for_unknown() {
        let m = DriverWatchManager::new();
        assert!(!m.is_watched(42));
    }

    #[test]
    fn points_for_subscriber() {
        let mut m = DriverWatchManager::new();
        m.subscribe("client-1", &[100, 200, 300]);

        let pts = m.points_for_subscriber("client-1");
        assert_eq!(pts.len(), 3);
        assert!(pts.contains(&100));
        assert!(pts.contains(&200));
        assert!(pts.contains(&300));
    }

    #[test]
    fn points_for_unknown_subscriber() {
        let m = DriverWatchManager::new();
        assert!(m.points_for_subscriber("nobody").is_empty());
    }

    #[test]
    fn duplicate_subscribe_is_idempotent() {
        let mut m = DriverWatchManager::new();
        m.subscribe("client-1", &[100]);
        m.subscribe("client-1", &[100]); // duplicate

        assert_eq!(m.watch_count(), 1);
        assert_eq!(m.subscriber_count(), 1);
    }

    #[test]
    fn unsubscribe_nonexistent_point_is_noop() {
        let mut m = DriverWatchManager::new();
        m.subscribe("client-1", &[100]);
        m.unsubscribe("client-1", &[999]); // not subscribed to 999
        assert_eq!(m.watch_count(), 1);
    }

    #[test]
    fn unsubscribe_nonexistent_subscriber_is_noop() {
        let mut m = DriverWatchManager::new();
        m.unsubscribe("nobody", &[100]); // nothing happens
        assert_eq!(m.watch_count(), 0);
    }

    #[test]
    fn default_trait() {
        let m = DriverWatchManager::default();
        assert_eq!(m.watch_count(), 0);
    }

    #[test]
    fn complex_scenario() {
        let mut m = DriverWatchManager::new();

        // Two clients subscribe to overlapping points
        m.subscribe("alpha", &[1, 2, 3]);
        m.subscribe("beta", &[2, 3, 4]);

        assert_eq!(m.watch_count(), 6); // 3 + 3 subscriber-point pairs
        assert_eq!(m.watched_point_count(), 4); // points 1,2,3,4
        assert_eq!(m.subscriber_count(), 2);

        // Unsubscribe alpha from point 2
        m.unsubscribe("alpha", &[2]);
        assert_eq!(m.watch_count(), 5);
        assert!(m.is_watched(2)); // beta still watching

        // Remove beta entirely
        m.remove_subscriber("beta");
        assert_eq!(m.watch_count(), 2); // alpha has 1,3
        assert!(!m.is_watched(2)); // nobody watching now
        assert!(!m.is_watched(4)); // nobody watching now
        assert!(m.is_watched(1));
        assert!(m.is_watched(3));
    }
}
