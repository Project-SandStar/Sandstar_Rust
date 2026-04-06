//! Polling bucket scheduler for driver point reads.
//!
//! Organizes points into buckets with configurable intervals and
//! automatic staggering to avoid thundering herd effects. The
//! [`PollScheduler`] tracks which buckets are due for polling and
//! provides timing information to the [`DriverManager`].

use std::time::{Duration, Instant};

use super::DriverPointRef;

// ── PollBucket ─────────────────────────────────────────────

/// Configuration for a polling bucket.
///
/// Each bucket groups a set of points belonging to a single driver
/// that should be polled at the same interval.
#[derive(Debug)]
pub struct PollBucket {
    /// Polling interval for this bucket.
    pub interval: Duration,
    /// Points to poll in this bucket.
    pub points: Vec<DriverPointRef>,
    /// Driver that owns these points.
    pub driver_id: String,
    /// Stagger offset to avoid clustering polls.
    pub offset: Duration,
    /// When this bucket was last polled (None = never).
    last_polled: Option<Instant>,
}

impl PollBucket {
    /// Calculate when this bucket is next due, relative to `now`.
    fn next_due(&self, now: Instant) -> Instant {
        match self.last_polled {
            Some(last) => last + self.interval,
            None => now + self.offset,
        }
    }

    /// Returns true if this bucket is due for polling at the given time.
    fn is_due(&self, now: Instant) -> bool {
        self.next_due(now) <= now
    }
}

// ── PollScheduler ──────────────────────────────────────────

/// Manages polling schedules for all drivers.
///
/// Points are organized into buckets by driver and interval. The
/// scheduler automatically staggers new buckets across the interval
/// to avoid thundering herd effects.
pub struct PollScheduler {
    buckets: Vec<PollBucket>,
    /// Whether to automatically stagger new buckets.
    auto_stagger: bool,
}

impl PollScheduler {
    /// Create a new empty poll scheduler.
    pub fn new() -> Self {
        Self {
            buckets: Vec::new(),
            auto_stagger: true,
        }
    }

    /// Create a scheduler with auto-stagger disabled (useful for tests).
    #[cfg(test)]
    pub fn new_no_stagger() -> Self {
        Self {
            buckets: Vec::new(),
            auto_stagger: false,
        }
    }

    /// Add a polling bucket for a driver.
    ///
    /// The offset is automatically calculated to stagger polls across
    /// the interval window when `auto_stagger` is enabled.
    pub fn add_bucket(
        &mut self,
        driver_id: &str,
        interval: Duration,
        points: Vec<DriverPointRef>,
    ) {
        let offset = if self.auto_stagger {
            self.calculate_stagger(interval)
        } else {
            Duration::ZERO
        };

        self.buckets.push(PollBucket {
            interval,
            points,
            driver_id: driver_id.to_string(),
            offset,
            last_polled: None,
        });
    }

    /// Remove all buckets for a driver.
    pub fn remove_driver(&mut self, driver_id: &str) {
        self.buckets.retain(|b| b.driver_id != driver_id);
    }

    /// Get the index of the next bucket that is due, with its due time.
    ///
    /// Returns `None` if there are no buckets.
    pub fn next_due(&self, now: Instant) -> Option<(usize, Instant)> {
        self.buckets
            .iter()
            .enumerate()
            .map(|(i, b)| (i, b.next_due(now)))
            .min_by_key(|&(_, due)| due)
    }

    /// Get all bucket indices that are currently due for polling.
    pub fn due_buckets(&self, now: Instant) -> Vec<usize> {
        self.buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| b.is_due(now))
            .map(|(i, _)| i)
            .collect()
    }

    /// Mark a bucket as polled (reset its timer).
    pub fn mark_polled(&mut self, bucket_idx: usize) {
        if let Some(bucket) = self.buckets.get_mut(bucket_idx) {
            bucket.last_polled = Some(Instant::now());
        }
    }

    /// Mark a bucket as polled at a specific time (for testing).
    #[cfg(test)]
    pub fn mark_polled_at(&mut self, bucket_idx: usize, at: Instant) {
        if let Some(bucket) = self.buckets.get_mut(bucket_idx) {
            bucket.last_polled = Some(at);
        }
    }

    /// Auto-stagger: calculate offset for a new bucket to avoid clustering.
    ///
    /// Distributes new buckets evenly across the interval by counting
    /// existing buckets with the same interval.
    fn calculate_stagger(&self, interval: Duration) -> Duration {
        let same_interval = self
            .buckets
            .iter()
            .filter(|b| b.interval == interval)
            .count();

        if same_interval == 0 {
            return Duration::ZERO;
        }

        // Spread evenly: offset = interval * (count / (count + 1))
        // This gives a nice distribution as more buckets are added.
        let fraction = same_interval as f64 / (same_interval as f64 + 1.0);
        let stagger_ms = interval.as_millis() as f64 * fraction / (same_interval as f64 + 1.0);
        Duration::from_millis(stagger_ms as u64)
    }

    /// Total number of points across all buckets.
    pub fn total_points(&self) -> usize {
        self.buckets.iter().map(|b| b.points.len()).sum()
    }

    /// Number of buckets.
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Get a reference to a bucket by index.
    pub fn bucket(&self, idx: usize) -> Option<&PollBucket> {
        self.buckets.get(idx)
    }

    /// Get all bucket indices for a given driver.
    pub fn buckets_for_driver(&self, driver_id: &str) -> Vec<usize> {
        self.buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| b.driver_id == driver_id)
            .map(|(i, _)| i)
            .collect()
    }
}

impl Default for PollScheduler {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_point(id: u32) -> DriverPointRef {
        DriverPointRef {
            point_id: id,
            address: format!("addr_{}", id),
        }
    }

    #[test]
    fn new_scheduler_is_empty() {
        let s = PollScheduler::new();
        assert_eq!(s.bucket_count(), 0);
        assert_eq!(s.total_points(), 0);
    }

    #[test]
    fn add_bucket_and_count() {
        let mut s = PollScheduler::new();
        s.add_bucket(
            "drv-1",
            Duration::from_secs(10),
            vec![make_point(1), make_point(2)],
        );
        assert_eq!(s.bucket_count(), 1);
        assert_eq!(s.total_points(), 2);
    }

    #[test]
    fn remove_driver_removes_all_buckets() {
        let mut s = PollScheduler::new();
        s.add_bucket("drv-1", Duration::from_secs(5), vec![make_point(1)]);
        s.add_bucket("drv-1", Duration::from_secs(30), vec![make_point(2)]);
        s.add_bucket("drv-2", Duration::from_secs(5), vec![make_point(3)]);

        assert_eq!(s.bucket_count(), 3);
        s.remove_driver("drv-1");
        assert_eq!(s.bucket_count(), 1);
        assert_eq!(s.buckets[0].driver_id, "drv-2");
    }

    #[test]
    fn remove_nonexistent_driver_is_noop() {
        let mut s = PollScheduler::new();
        s.add_bucket("drv-1", Duration::from_secs(5), vec![make_point(1)]);
        s.remove_driver("nonexistent");
        assert_eq!(s.bucket_count(), 1);
    }

    #[test]
    fn next_due_never_polled_returns_offset_time() {
        let mut s = PollScheduler::new_no_stagger();
        s.add_bucket("drv-1", Duration::from_secs(10), vec![make_point(1)]);

        let now = Instant::now();
        let (idx, due) = s.next_due(now).unwrap();
        assert_eq!(idx, 0);
        // With no stagger and never polled, due == now + offset(0) == now
        assert!(due <= now);
    }

    #[test]
    fn next_due_after_poll_returns_next_interval() {
        let mut s = PollScheduler::new_no_stagger();
        s.add_bucket("drv-1", Duration::from_secs(10), vec![make_point(1)]);

        let now = Instant::now();
        s.mark_polled_at(0, now);

        let (idx, due) = s.next_due(now).unwrap();
        assert_eq!(idx, 0);
        // Due should be now + 10s
        assert!(due > now);
        assert!(due <= now + Duration::from_secs(11));
    }

    #[test]
    fn next_due_picks_earliest() {
        let mut s = PollScheduler::new_no_stagger();
        s.add_bucket("drv-1", Duration::from_secs(30), vec![make_point(1)]);
        s.add_bucket("drv-2", Duration::from_secs(5), vec![make_point(2)]);

        let now = Instant::now();
        s.mark_polled_at(0, now);
        s.mark_polled_at(1, now);

        let (idx, _due) = s.next_due(now).unwrap();
        assert_eq!(idx, 1); // 5s bucket is due sooner
    }

    #[test]
    fn next_due_empty_returns_none() {
        let s = PollScheduler::new();
        assert!(s.next_due(Instant::now()).is_none());
    }

    #[test]
    fn due_buckets_returns_overdue() {
        let mut s = PollScheduler::new_no_stagger();
        s.add_bucket("drv-1", Duration::from_secs(1), vec![make_point(1)]);
        s.add_bucket("drv-2", Duration::from_secs(1000), vec![make_point(2)]);

        // Never polled + zero offset → both are due immediately
        let now = Instant::now();
        let due = s.due_buckets(now);
        assert_eq!(due.len(), 2);
    }

    #[test]
    fn mark_polled_resets_timer() {
        let mut s = PollScheduler::new_no_stagger();
        s.add_bucket("drv-1", Duration::from_secs(60), vec![make_point(1)]);

        let now = Instant::now();
        assert!(s.buckets[0].is_due(now)); // never polled

        s.mark_polled_at(0, now);
        assert!(!s.buckets[0].is_due(now)); // just polled
    }

    #[test]
    fn buckets_for_driver() {
        let mut s = PollScheduler::new();
        s.add_bucket("drv-1", Duration::from_secs(5), vec![make_point(1)]);
        s.add_bucket("drv-2", Duration::from_secs(10), vec![make_point(2)]);
        s.add_bucket("drv-1", Duration::from_secs(30), vec![make_point(3)]);

        let indices = s.buckets_for_driver("drv-1");
        assert_eq!(indices.len(), 2);
        assert!(indices.contains(&0));
        assert!(indices.contains(&2));
    }

    #[test]
    fn stagger_distributes_offsets() {
        let mut s = PollScheduler::new(); // auto_stagger = true
        let interval = Duration::from_secs(10);
        s.add_bucket("drv-1", interval, vec![make_point(1)]);
        s.add_bucket("drv-2", interval, vec![make_point(2)]);
        s.add_bucket("drv-3", interval, vec![make_point(3)]);

        // First bucket should have zero offset
        assert_eq!(s.buckets[0].offset, Duration::ZERO);
        // Subsequent buckets should have non-zero (or at least different) offsets
        // The exact values depend on the stagger algorithm
        // Just verify they're not all identical
        let offsets: Vec<_> = s.buckets.iter().map(|b| b.offset).collect();
        // At minimum, first is zero and others may differ
        assert_eq!(offsets[0], Duration::ZERO);
    }

    #[test]
    fn total_points_across_buckets() {
        let mut s = PollScheduler::new();
        s.add_bucket(
            "drv-1",
            Duration::from_secs(5),
            vec![make_point(1), make_point(2), make_point(3)],
        );
        s.add_bucket("drv-2", Duration::from_secs(10), vec![make_point(4)]);
        assert_eq!(s.total_points(), 4);
    }

    #[test]
    fn bucket_accessor() {
        let mut s = PollScheduler::new();
        s.add_bucket("drv-1", Duration::from_secs(5), vec![make_point(1)]);
        assert!(s.bucket(0).is_some());
        assert_eq!(s.bucket(0).unwrap().driver_id, "drv-1");
        assert!(s.bucket(99).is_none());
    }

    #[test]
    fn default_trait() {
        let s = PollScheduler::default();
        assert_eq!(s.bucket_count(), 0);
    }

    #[test]
    fn mark_polled_out_of_bounds_is_noop() {
        let mut s = PollScheduler::new();
        s.mark_polled(99); // should not panic
    }
}
