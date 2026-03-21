//! Kit 9: datetimeStd native methods — pure Rust replacements.
//!
//! These replace the C implementations in `datetimeStd_DateTimeServiceStd.c`.
//!
//! | ID | Method         | Return | Description                              |
//! |----|----------------|--------|------------------------------------------|
//! |  0 | doNow          | i64    | Nanos since Sedona epoch (2000-01-01)    |
//! |  1 | doSetClock     | i32    | Set system clock (no-op)                 |
//! |  2 | doGetUtcOffset | i32    | UTC offset in seconds (incl. DST)        |

use std::time::{SystemTime, UNIX_EPOCH};

use crate::native_table::{NativeContext, NativeTable};
use crate::vm_error::VmResult;

/// Seconds between Unix epoch (1970-01-01) and Sedona epoch (2000-01-01).
///
/// 30 years with 7 leap years (1972, 1976, 1980, 1984, 1988, 1992, 1996):
/// `(365 * 30 + 7) * 86400 = 946_684_800`
const SEDONA_EPOCH_OFFSET_SECS: i64 = ((365 * 30) + 7) * 24 * 60 * 60;

// ────────────────────────────────────────────────────────────────────
// Native method implementations
// ────────────────────────────────────────────────────────────────────

/// `doNow()` — returns current time as nanoseconds since Sedona epoch.
///
/// Wide return (i64).  Mirrors the C implementation which calls
/// `time(NULL)`, subtracts `SEDONA_EPOCH_OFFSET_SECS`, and multiplies
/// by 1 000 000 000.
pub fn datetime_do_now(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let unix_secs = now.as_secs() as i64;
    let sedona_secs = unix_secs - SEDONA_EPOCH_OFFSET_SECS;
    // The C code only uses whole seconds (time(NULL)), no sub-second precision.
    let nanos = sedona_secs * 1_000_000_000;
    Ok(nanos)
}

/// `doSetClock()` — set system clock.
///
/// No-op on all platforms — we never allow setting the system clock from
/// the VM.  Returns 0, matching the C implementation.
pub fn datetime_do_set_clock(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    Ok(0)
}

/// `doGetUtcOffset()` — returns UTC offset in seconds, including DST.
///
/// The C code uses `tzset()` + `localtime()` + the `timezone`/`daylight`
/// globals.  We use `chrono::Local::now().offset().local_minus_utc()` for
/// a clean cross-platform implementation.
pub fn datetime_do_get_utc_offset(_ctx: &mut NativeContext<'_>, _params: &[i32]) -> VmResult<i32> {
    let offset_secs = chrono::Local::now().offset().local_minus_utc();
    Ok(offset_secs)
}

// ────────────────────────────────────────────────────────────────────
// Registration
// ────────────────────────────────────────────────────────────────────

/// Register all Kit 9 (datetimeStd) methods in a [`NativeTable`].
///
/// Replaces the stub registrations with real implementations:
/// - Method 0: `doNow` (wide return — i64 nanos)
/// - Method 1: `doSetClock` (normal — always returns 0)
/// - Method 2: `doGetUtcOffset` (normal — seconds including DST)
pub fn register_kit9(table: &mut NativeTable) {
    table.set_kit_name(9, "datetimeStd");
    table.register_wide(9, 0, datetime_do_now);
    table.register(9, 1, datetime_do_set_clock);
    table.register(9, 2, datetime_do_get_utc_offset);
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_table::{NativeContext, NativeEntry, NativeTable};

    /// Helper to create a throwaway NativeContext for testing.
    fn test_ctx() -> (Vec<u8>, Vec<i32>) {
        (vec![0u8; 64], vec![])
    }

    #[test]
    fn do_now_returns_positive_nanos() {
        let (mut mem, params) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let nanos = datetime_do_now(&mut ctx, &params).expect("doNow failed");
        assert!(nanos > 0, "doNow should return positive nanos, got {nanos}");
    }

    #[test]
    fn do_now_roughly_correct() {
        // Verify the value is within 1 day of the expected value.
        let (mut mem, params) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let nanos = datetime_do_now(&mut ctx, &params).expect("doNow failed");

        // Compute expected from std::time directly
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let expected_nanos = (now - SEDONA_EPOCH_OFFSET_SECS) * 1_000_000_000;

        let diff = (nanos - expected_nanos).abs();
        let one_day_nanos: i64 = 86_400 * 1_000_000_000;
        assert!(
            diff < one_day_nanos,
            "doNow off by more than 1 day: got {nanos}, expected ~{expected_nanos}"
        );
    }

    #[test]
    fn do_now_monotonically_increasing() {
        let (mut mem, params) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let first = datetime_do_now(&mut ctx, &params).expect("doNow first call");
        let second = datetime_do_now(&mut ctx, &params).expect("doNow second call");
        assert!(
            second >= first,
            "doNow should be monotonically increasing: first={first}, second={second}"
        );
    }

    #[test]
    fn do_now_is_after_2025() {
        // Sanity: nanos since 2000-01-01 should represent > 25 years
        let (mut mem, params) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let nanos = datetime_do_now(&mut ctx, &params).expect("doNow failed");
        let secs = nanos / 1_000_000_000;
        let years_approx = secs / (365 * 86400);
        assert!(
            years_approx >= 25,
            "doNow should show >= 25 years since 2000, got ~{years_approx}"
        );
    }

    #[test]
    fn do_set_clock_returns_zero() {
        let (mut mem, params) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let result = datetime_do_set_clock(&mut ctx, &params).expect("doSetClock failed");
        assert_eq!(result, 0, "doSetClock should always return 0");
    }

    #[test]
    fn do_set_clock_ignores_params() {
        let (mut mem, _) = test_ctx();
        let params = vec![42i32, 99];
        let mut ctx = NativeContext::new(&mut mem);
        let result = datetime_do_set_clock(&mut ctx, &params).expect("doSetClock failed");
        assert_eq!(result, 0, "doSetClock should return 0 regardless of params");
    }

    #[test]
    fn do_get_utc_offset_in_valid_range() {
        let (mut mem, params) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let offset = datetime_do_get_utc_offset(&mut ctx, &params).expect("doGetUtcOffset failed");
        // Valid UTC offsets range from -12h to +14h
        let min_offset = -12 * 3600;
        let max_offset = 14 * 3600;
        assert!(
            offset >= min_offset && offset <= max_offset,
            "UTC offset {offset}s is outside valid range [{min_offset}, {max_offset}]"
        );
    }

    #[test]
    fn do_get_utc_offset_is_whole_minutes() {
        // Most time zones are whole-hour or half-hour offsets (divisible by 900s)
        // A few are at 45min (e.g., Nepal +5:45). All are divisible by 60s.
        let (mut mem, params) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);
        let offset = datetime_do_get_utc_offset(&mut ctx, &params).expect("doGetUtcOffset failed");
        assert_eq!(
            offset % 60,
            0,
            "UTC offset {offset}s should be divisible by 60"
        );
    }

    #[test]
    fn register_kit9_populates_table() {
        let mut table = NativeTable::new();
        register_kit9(&mut table);

        // Method 0 should be Wide (doNow)
        let entry0 = table.lookup(9, 0).expect("kit9 method 0 missing");
        assert!(
            matches!(entry0, NativeEntry::Wide(_)),
            "method 0 should be Wide, got {entry0:?}"
        );

        // Method 1 should be Normal (doSetClock)
        let entry1 = table.lookup(9, 1).expect("kit9 method 1 missing");
        assert!(
            matches!(entry1, NativeEntry::Normal(_)),
            "method 1 should be Normal, got {entry1:?}"
        );

        // Method 2 should be Normal (doGetUtcOffset)
        let entry2 = table.lookup(9, 2).expect("kit9 method 2 missing");
        assert!(
            matches!(entry2, NativeEntry::Normal(_)),
            "method 2 should be Normal, got {entry2:?}"
        );
    }

    #[test]
    fn register_kit9_methods_callable() {
        let mut table = NativeTable::new();
        register_kit9(&mut table);

        let (mut mem, params) = test_ctx();
        let mut ctx = NativeContext::new(&mut mem);

        // Call doSetClock via dispatch
        let result = table.call(9, 1, &mut ctx, &params).expect("dispatch doSetClock");
        assert_eq!(result, 0);

        // Call doGetUtcOffset via dispatch
        let offset = table.call(9, 2, &mut ctx, &params).expect("dispatch doGetUtcOffset");
        assert!(offset >= -12 * 3600 && offset <= 14 * 3600);
    }

    #[test]
    fn sedona_epoch_offset_is_correct() {
        // Verify our constant matches the well-known Unix timestamp for 2000-01-01 00:00:00 UTC
        assert_eq!(SEDONA_EPOCH_OFFSET_SECS, 946_684_800);
    }
}
