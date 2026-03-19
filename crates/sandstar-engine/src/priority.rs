//! BACnet-style 17-level write priority array.
//!
//! Each output channel can have up to 17 priority levels (1 = highest, 17 = lowest).
//! The effective output value is the highest-priority non-null level.
//! Levels are lazy-allocated per channel to save memory on read-only channels.
//!
//! Timed writes: if `duration > 0`, the level auto-relinquishes after that many
//! seconds. Expiration is checked eagerly via `expire_timed_levels()` each poll cycle.
//!
//! Matches the C implementation in `engineio.c` (`CHANNEL_WRITELEVEL`).

use std::time::{Duration, Instant};

/// Maximum number of priority levels (BACnet standard).
pub const MAX_LEVELS: usize = 17;

/// Maximum length of "who" identifier.
pub const MAX_WHO_LEN: usize = 16;

/// A single priority level entry.
#[derive(Debug, Clone)]
pub struct WriteLevel {
    /// The value written at this level (None = relinquished/empty).
    pub value: Option<f64>,
    /// Who wrote this level (max 16 chars).
    pub who: String,
    /// Duration in seconds (0 = permanent).
    pub duration: f64,
    /// When this timed write expires (None = permanent / no expiry).
    pub expires_at: Option<Instant>,
}

impl Default for WriteLevel {
    fn default() -> Self {
        Self {
            value: None,
            who: String::new(),
            duration: 0.0,
            expires_at: None,
        }
    }
}

/// Result of a write operation.
#[derive(Debug)]
pub struct WriteResult {
    /// The effective value after this write (None = all levels empty).
    pub effective_value: Option<f64>,
    /// The 1-based level of the effective value (0 if all empty).
    pub effective_level: u8,
}

/// Per-channel priority array of 17 levels.
#[derive(Debug, Clone)]
pub struct PriorityArray {
    pub(crate) levels: [WriteLevel; MAX_LEVELS],
    /// Cached 0-indexed active level.
    active_level: usize,
}

impl Default for PriorityArray {
    fn default() -> Self {
        Self {
            levels: std::array::from_fn(|_| WriteLevel::default()),
            active_level: MAX_LEVELS - 1,
        }
    }
}

impl PriorityArray {
    /// Set a value at a given priority level (1-based, 1 = highest).
    ///
    /// If `value` is None, this relinquishes the level.
    /// Returns the new effective value and level after this write.
    pub fn set_level(
        &mut self,
        level: u8,
        value: Option<f64>,
        who: &str,
        duration: f64,
    ) -> WriteResult {
        // Guard: level must be 1-17. In release builds debug_assert! is
        // stripped, and level=0 would cause (0u8 - 1) to wrap to usize::MAX
        // → index out of bounds panic.  Return current effective instead.
        if !(1..=MAX_LEVELS as u8).contains(&level) {
            let (eff_val, eff_lvl) = self.effective();
            return WriteResult {
                effective_value: eff_val,
                effective_level: eff_lvl,
            };
        }
        let idx = (level - 1) as usize;

        let expires_at = if duration > 0.0 && value.is_some() {
            Some(Instant::now() + Duration::from_secs_f64(duration))
        } else {
            None
        };

        self.levels[idx] = WriteLevel {
            value,
            who: who.chars().take(MAX_WHO_LEN).collect(),
            duration,
            expires_at,
        };

        if value.is_some() {
            // Writing: if this level is higher or equal priority, it wins
            if idx <= self.active_level {
                self.active_level = idx;
                return WriteResult {
                    effective_value: value,
                    effective_level: level,
                };
            }
            // Current active level unchanged
            let eff = self.levels[self.active_level].value;
            WriteResult {
                effective_value: eff,
                effective_level: (self.active_level + 1) as u8,
            }
        } else {
            // Relinquishing: if this was the active level, find next
            if idx == self.active_level {
                self.recalculate_active();
            }
            let eff = self.levels[self.active_level].value;
            WriteResult {
                effective_value: eff,
                effective_level: if eff.is_some() {
                    (self.active_level + 1) as u8
                } else {
                    0
                },
            }
        }
    }

    /// Get all 17 levels (for API response).
    pub fn levels(&self) -> &[WriteLevel; MAX_LEVELS] {
        &self.levels
    }

    /// Get the current effective value and 1-based level (0 if all empty).
    pub fn effective(&self) -> (Option<f64>, u8) {
        let val = self.levels[self.active_level].value;
        let level = if val.is_some() {
            (self.active_level + 1) as u8
        } else {
            0
        };
        (val, level)
    }

    /// Recalculate the active level by scanning from highest priority down.
    fn recalculate_active(&mut self) {
        for i in 0..MAX_LEVELS {
            if self.levels[i].value.is_some() {
                self.active_level = i;
                return;
            }
        }
        // All empty
        self.active_level = MAX_LEVELS - 1;
    }

    /// Clear all levels.
    pub fn clear(&mut self) {
        for level in &mut self.levels {
            *level = WriteLevel::default();
        }
        self.active_level = MAX_LEVELS - 1;
    }

    /// Expire any timed levels whose deadline has passed.
    ///
    /// Returns `true` if any levels were expired (effective value may have changed).
    /// Called eagerly during each poll cycle, BEFORE computing `effective()`.
    pub fn expire_timed_levels(&mut self) -> bool {
        let now = Instant::now();
        let mut any_expired = false;

        for level in &mut self.levels {
            if let Some(deadline) = level.expires_at {
                if now >= deadline && level.value.is_some() {
                    level.value = None;
                    level.who.clear();
                    level.duration = 0.0;
                    level.expires_at = None;
                    any_expired = true;
                }
            }
        }

        if any_expired {
            self.recalculate_active();
        }

        any_expired
    }

    /// Variant of `expire_timed_levels` that accepts an explicit `now` instant.
    ///
    /// Useful for deterministic testing without real wall-clock time.
    #[cfg(test)]
    pub fn expire_timed_levels_at(&mut self, now: Instant) -> bool {
        let mut any_expired = false;

        for level in &mut self.levels {
            if let Some(deadline) = level.expires_at {
                if now >= deadline && level.value.is_some() {
                    level.value = None;
                    level.who.clear();
                    level.duration = 0.0;
                    level.expires_at = None;
                    any_expired = true;
                }
            }
        }

        if any_expired {
            self.recalculate_active();
        }

        any_expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_all_empty() {
        let pa = PriorityArray::default();
        let (val, level) = pa.effective();
        assert!(val.is_none());
        assert_eq!(level, 0);
    }

    #[test]
    fn test_write_single_level() {
        let mut pa = PriorityArray::default();
        let result = pa.set_level(17, Some(42.0), "test", 0.0);
        assert_eq!(result.effective_value, Some(42.0));
        assert_eq!(result.effective_level, 17);
    }

    #[test]
    fn test_higher_priority_wins() {
        let mut pa = PriorityArray::default();

        // Write at level 17 (lowest)
        pa.set_level(17, Some(10.0), "low", 0.0);

        // Write at level 8 (higher priority)
        let result = pa.set_level(8, Some(20.0), "high", 0.0);
        assert_eq!(result.effective_value, Some(20.0));
        assert_eq!(result.effective_level, 8);

        // Effective should be level 8
        let (val, level) = pa.effective();
        assert_eq!(val, Some(20.0));
        assert_eq!(level, 8);
    }

    #[test]
    fn test_lower_priority_doesnt_override() {
        let mut pa = PriorityArray::default();

        // Write at level 8 (higher)
        pa.set_level(8, Some(20.0), "high", 0.0);

        // Write at level 17 (lower) — should NOT change effective
        let result = pa.set_level(17, Some(10.0), "low", 0.0);
        assert_eq!(result.effective_value, Some(20.0));
        assert_eq!(result.effective_level, 8);
    }

    #[test]
    fn test_relinquish_active_falls_to_next() {
        let mut pa = PriorityArray::default();

        // Set levels 8 and 17
        pa.set_level(17, Some(10.0), "low", 0.0);
        pa.set_level(8, Some(20.0), "high", 0.0);

        // Relinquish level 8 — should fall to level 17
        let result = pa.set_level(8, None, "", 0.0);
        assert_eq!(result.effective_value, Some(10.0));
        assert_eq!(result.effective_level, 17);
    }

    #[test]
    fn test_relinquish_all_levels() {
        let mut pa = PriorityArray::default();

        pa.set_level(8, Some(20.0), "test", 0.0);
        pa.set_level(17, Some(10.0), "test", 0.0);

        // Relinquish both
        pa.set_level(8, None, "", 0.0);
        let result = pa.set_level(17, None, "", 0.0);

        assert!(result.effective_value.is_none());
        assert_eq!(result.effective_level, 0);
    }

    #[test]
    fn test_relinquish_inactive_level_no_change() {
        let mut pa = PriorityArray::default();

        pa.set_level(8, Some(20.0), "active", 0.0);
        pa.set_level(17, Some(10.0), "backup", 0.0);

        // Relinquish level 17 (not active) — effective stays at level 8
        let result = pa.set_level(17, None, "", 0.0);
        assert_eq!(result.effective_value, Some(20.0));
        assert_eq!(result.effective_level, 8);
    }

    #[test]
    fn test_overwrite_same_level() {
        let mut pa = PriorityArray::default();

        pa.set_level(8, Some(20.0), "first", 0.0);

        // Overwrite level 8 with new value
        let result = pa.set_level(8, Some(30.0), "second", 0.0);
        assert_eq!(result.effective_value, Some(30.0));
        assert_eq!(result.effective_level, 8);

        // Check who
        assert_eq!(pa.levels()[7].who, "second");
    }

    #[test]
    fn test_who_truncated() {
        let mut pa = PriorityArray::default();
        pa.set_level(1, Some(1.0), "this-is-a-very-long-who-string", 0.0);
        assert_eq!(pa.levels()[0].who.len(), MAX_WHO_LEN);
    }

    #[test]
    fn test_duration_stored() {
        let mut pa = PriorityArray::default();
        pa.set_level(1, Some(1.0), "test", 3600.0);
        assert_eq!(pa.levels()[0].duration, 3600.0);
        // Non-zero duration should set expires_at
        assert!(pa.levels()[0].expires_at.is_some());
    }

    #[test]
    fn test_clear() {
        let mut pa = PriorityArray::default();
        pa.set_level(1, Some(100.0), "test", 0.0);
        pa.set_level(8, Some(50.0), "test", 0.0);

        pa.clear();

        let (val, level) = pa.effective();
        assert!(val.is_none());
        assert_eq!(level, 0);
        assert!(pa.levels()[0].value.is_none());
        assert!(pa.levels()[7].value.is_none());
    }

    #[test]
    fn test_level_1_highest_priority() {
        let mut pa = PriorityArray::default();

        for lvl in (1..=17).rev() {
            pa.set_level(lvl, Some(lvl as f64), "test", 0.0);
        }

        // Level 1 should win
        let (val, level) = pa.effective();
        assert_eq!(val, Some(1.0));
        assert_eq!(level, 1);
    }

    #[test]
    fn test_cascade_relinquish() {
        let mut pa = PriorityArray::default();

        // Set levels 1, 5, 10, 17
        pa.set_level(17, Some(17.0), "d", 0.0);
        pa.set_level(10, Some(10.0), "c", 0.0);
        pa.set_level(5, Some(5.0), "b", 0.0);
        pa.set_level(1, Some(1.0), "a", 0.0);

        // Relinquish 1 -> falls to 5
        let r = pa.set_level(1, None, "", 0.0);
        assert_eq!(r.effective_level, 5);

        // Relinquish 5 -> falls to 10
        let r = pa.set_level(5, None, "", 0.0);
        assert_eq!(r.effective_level, 10);

        // Relinquish 10 -> falls to 17
        let r = pa.set_level(10, None, "", 0.0);
        assert_eq!(r.effective_level, 17);

        // Relinquish 17 -> all empty
        let r = pa.set_level(17, None, "", 0.0);
        assert_eq!(r.effective_level, 0);
        assert!(r.effective_value.is_none());
    }

    #[test]
    fn test_level_zero_returns_current_effective() {
        let mut pa = PriorityArray::default();
        pa.set_level(8, Some(42.0), "test", 0.0);

        // Level 0 is out of range — should return current effective without panic
        let r = pa.set_level(0, Some(99.0), "bad", 0.0);
        assert_eq!(r.effective_value, Some(42.0));
        assert_eq!(r.effective_level, 8);
    }

    #[test]
    fn test_level_18_returns_current_effective() {
        let mut pa = PriorityArray::default();
        pa.set_level(17, Some(10.0), "test", 0.0);

        // Level 18 is out of range — should return current effective without panic
        let r = pa.set_level(18, Some(99.0), "bad", 0.0);
        assert_eq!(r.effective_value, Some(10.0));
        assert_eq!(r.effective_level, 17);
    }

    #[test]
    fn test_level_255_returns_current_effective() {
        let mut pa = PriorityArray::default();

        // Empty array, level 255 out of range — should return empty effective
        let r = pa.set_level(255, Some(1.0), "bad", 0.0);
        assert!(r.effective_value.is_none());
        assert_eq!(r.effective_level, 0);
    }

    // ========================================================================
    // Duration expiration tests
    // ========================================================================

    #[test]
    fn test_duration_sets_expires_at() {
        let mut pa = PriorityArray::default();
        let before = Instant::now();
        pa.set_level(8, Some(1.0), "test", 300.0);
        let after = Instant::now();

        let wl = &pa.levels()[7];
        assert!(wl.expires_at.is_some());

        let expires = wl.expires_at.unwrap();
        // Should expire ~300s from now
        assert!(expires >= before + Duration::from_secs(300));
        assert!(expires <= after + Duration::from_secs(300));
    }

    #[test]
    fn test_zero_duration_no_expiry() {
        let mut pa = PriorityArray::default();
        pa.set_level(8, Some(1.0), "test", 0.0);
        assert!(pa.levels()[7].expires_at.is_none());
    }

    #[test]
    fn test_relinquish_no_expiry() {
        let mut pa = PriorityArray::default();
        // Relinquishing (value=None) should never set expires_at
        pa.set_level(8, None, "test", 300.0);
        assert!(pa.levels()[7].expires_at.is_none());
    }

    #[test]
    fn test_expire_timed_levels_basic() {
        let mut pa = PriorityArray::default();

        // Set a permanent level at 17 and a timed level at 8
        pa.set_level(17, Some(10.0), "base", 0.0);
        pa.set_level(8, Some(20.0), "temp", 1.0); // 1 second

        // Before expiry: level 8 wins
        let (val, level) = pa.effective();
        assert_eq!(val, Some(20.0));
        assert_eq!(level, 8);

        // Manually set expires_at to the past for deterministic test
        let past = Instant::now() - Duration::from_secs(10);
        pa.levels[7].expires_at = Some(past);

        // Expire
        let expired = pa.expire_timed_levels();
        assert!(expired);

        // After expiry: level 17 wins
        let (val, level) = pa.effective();
        assert_eq!(val, Some(10.0));
        assert_eq!(level, 17);

        // The expired level should be cleared
        assert!(pa.levels()[7].value.is_none());
        assert!(pa.levels()[7].expires_at.is_none());
        assert!(pa.levels()[7].who.is_empty());
    }

    #[test]
    fn test_expire_timed_levels_at_deterministic() {
        let mut pa = PriorityArray::default();

        pa.set_level(17, Some(10.0), "base", 0.0);
        pa.set_level(8, Some(20.0), "temp", 60.0); // 60 seconds

        let write_time = Instant::now();

        // Before deadline: nothing expires
        let expired = pa.expire_timed_levels_at(write_time + Duration::from_secs(30));
        assert!(!expired);
        assert_eq!(pa.effective().0, Some(20.0));

        // After deadline: level 8 expires
        let expired = pa.expire_timed_levels_at(write_time + Duration::from_secs(120));
        assert!(expired);
        assert_eq!(pa.effective().0, Some(10.0));
    }

    #[test]
    fn test_expire_multiple_timed_levels() {
        let mut pa = PriorityArray::default();

        pa.set_level(17, Some(17.0), "permanent", 0.0);
        pa.set_level(8, Some(8.0), "temp1", 60.0);
        pa.set_level(5, Some(5.0), "temp2", 30.0);

        // Expire both timed levels
        let past = Instant::now() - Duration::from_secs(10);
        pa.levels[7].expires_at = Some(past); // level 8
        pa.levels[4].expires_at = Some(past); // level 5

        let expired = pa.expire_timed_levels();
        assert!(expired);

        // Only level 17 remains
        let (val, level) = pa.effective();
        assert_eq!(val, Some(17.0));
        assert_eq!(level, 17);
    }

    #[test]
    fn test_expire_all_timed_levels_empty() {
        let mut pa = PriorityArray::default();

        // Only timed levels, no permanent
        pa.set_level(8, Some(20.0), "temp", 60.0);

        let past = Instant::now() - Duration::from_secs(10);
        pa.levels[7].expires_at = Some(past);

        let expired = pa.expire_timed_levels();
        assert!(expired);

        // All empty
        let (val, level) = pa.effective();
        assert!(val.is_none());
        assert_eq!(level, 0);
    }

    #[test]
    fn test_expire_no_timed_levels_noop() {
        let mut pa = PriorityArray::default();
        pa.set_level(8, Some(20.0), "perm", 0.0);

        let expired = pa.expire_timed_levels();
        assert!(!expired);

        let (val, level) = pa.effective();
        assert_eq!(val, Some(20.0));
        assert_eq!(level, 8);
    }

    #[test]
    fn test_expire_future_deadline_not_expired() {
        let mut pa = PriorityArray::default();
        pa.set_level(8, Some(20.0), "future", 3600.0); // 1 hour

        // Should NOT expire yet
        let expired = pa.expire_timed_levels();
        assert!(!expired);

        let (val, level) = pa.effective();
        assert_eq!(val, Some(20.0));
        assert_eq!(level, 8);
    }
}
