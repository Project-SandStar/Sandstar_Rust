//! Lightweight internal metrics using atomic counters.
//!
//! No external dependencies — just `std::sync::atomic`, a JSON endpoint at
//! `/api/metrics` (legacy), and a Prometheus exposition-format endpoint at
//! `/metrics` (scraper-friendly).

use std::fmt::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Global server metrics.
pub struct Metrics {
    pub poll_count: AtomicU64,
    pub poll_duration_us_last: AtomicU64,
    pub poll_duration_us_max: AtomicU64,
    pub poll_overrun_count: AtomicU64,
    pub hal_errors: AtomicU64,
    pub rest_requests: AtomicU64,
    pub ipc_requests: AtomicU64,
    pub watch_active: AtomicI64,
    pub history_points: AtomicU64,
    pub ws_active: AtomicI64,
    pub ws_total: AtomicU64,
    pub ws_messages_in: AtomicU64,
    pub ws_messages_out: AtomicU64,
    pub rows_active: AtomicI64,
    pub rows_total: AtomicU64,
    pub rows_messages_in: AtomicU64,
    pub rows_messages_out: AtomicU64,
    // ── Driver-framework counters (Phase 12) ──────────────
    /// Total number of `sync_all` invocations across all drivers.
    pub driver_sync_total: AtomicU64,
    /// Total number of per-point errors returned by any driver's `sync_cur`.
    pub driver_sync_errors_total: AtomicU64,
    /// Total CovEvents broadcast from the driver actor (Phase 12.0D).
    pub driver_cov_events_total: AtomicU64,
}

static METRICS: Metrics = Metrics {
    poll_count: AtomicU64::new(0),
    poll_duration_us_last: AtomicU64::new(0),
    poll_duration_us_max: AtomicU64::new(0),
    poll_overrun_count: AtomicU64::new(0),
    hal_errors: AtomicU64::new(0),
    rest_requests: AtomicU64::new(0),
    ipc_requests: AtomicU64::new(0),
    watch_active: AtomicI64::new(0),
    history_points: AtomicU64::new(0),
    ws_active: AtomicI64::new(0),
    ws_total: AtomicU64::new(0),
    ws_messages_in: AtomicU64::new(0),
    ws_messages_out: AtomicU64::new(0),
    rows_active: AtomicI64::new(0),
    rows_total: AtomicU64::new(0),
    rows_messages_in: AtomicU64::new(0),
    rows_messages_out: AtomicU64::new(0),
    driver_sync_total: AtomicU64::new(0),
    driver_sync_errors_total: AtomicU64::new(0),
    driver_cov_events_total: AtomicU64::new(0),
};

/// Get the global metrics instance.
pub fn metrics() -> &'static Metrics {
    &METRICS
}

impl Metrics {
    /// Serialize all counters to JSON.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "pollCount": self.poll_count.load(Ordering::Relaxed),
            "pollDurationUsLast": self.poll_duration_us_last.load(Ordering::Relaxed),
            "pollDurationUsMax": self.poll_duration_us_max.load(Ordering::Relaxed),
            "pollOverrunCount": self.poll_overrun_count.load(Ordering::Relaxed),
            "halErrors": self.hal_errors.load(Ordering::Relaxed),
            "restRequests": self.rest_requests.load(Ordering::Relaxed),
            "ipcRequests": self.ipc_requests.load(Ordering::Relaxed),
            "watchActive": self.watch_active.load(Ordering::Relaxed),
            "historyPoints": self.history_points.load(Ordering::Relaxed),
            "wsActive": self.ws_active.load(Ordering::Relaxed),
            "wsTotal": self.ws_total.load(Ordering::Relaxed),
            "wsMessagesIn": self.ws_messages_in.load(Ordering::Relaxed),
            "wsMessagesOut": self.ws_messages_out.load(Ordering::Relaxed),
            "rowsActive": self.rows_active.load(Ordering::Relaxed),
            "rowsTotal": self.rows_total.load(Ordering::Relaxed),
            "rowsMessagesIn": self.rows_messages_in.load(Ordering::Relaxed),
            "rowsMessagesOut": self.rows_messages_out.load(Ordering::Relaxed),
            "driverSyncTotal": self.driver_sync_total.load(Ordering::Relaxed),
            "driverSyncErrorsTotal": self.driver_sync_errors_total.load(Ordering::Relaxed),
            "driverCovEventsTotal": self.driver_cov_events_total.load(Ordering::Relaxed),
        })
    }

    /// Serialize all counters to the Prometheus text exposition format
    /// (version 0.0.4). Safe to return as `text/plain; version=0.0.4;
    /// charset=utf-8` — suitable for `/metrics` scraping.
    ///
    /// `build_version` and `build_git_hash` populate the `sandstar_build_info`
    /// gauge (always 1) as labels. Both are typically
    /// `env!("CARGO_PKG_VERSION")` and `env!("BUILD_GIT_HASH")`.
    ///
    /// Counter vs gauge:
    ///   - counters end in `_total` and monotonically increase
    ///   - gauges represent instantaneous values (may go up or down)
    pub fn to_prometheus(&self, build_version: &str, build_git_hash: &str) -> String {
        let mut out = String::with_capacity(4096);

        // Build info — a gauge with static value 1, labels carry the info.
        writeln!(
            &mut out,
            "# HELP sandstar_build_info Build metadata (version + git hash) as labels; value is always 1."
        )
        .unwrap();
        writeln!(&mut out, "# TYPE sandstar_build_info gauge").unwrap();
        writeln!(
            &mut out,
            "sandstar_build_info{{version=\"{}\",git_hash=\"{}\"}} 1",
            escape_label(build_version),
            escape_label(build_git_hash),
        )
        .unwrap();

        emit_counter(
            &mut out,
            "sandstar_poll_count",
            "Total number of poll-cycle iterations the engine has executed.",
            self.poll_count.load(Ordering::Relaxed),
        );
        emit_counter(
            &mut out,
            "sandstar_poll_overrun_count",
            "Poll cycles that exceeded their scheduled interval.",
            self.poll_overrun_count.load(Ordering::Relaxed),
        );
        emit_counter(
            &mut out,
            "sandstar_hal_errors_total",
            "HAL (GPIO/ADC/I2C/PWM) read or write errors since start.",
            self.hal_errors.load(Ordering::Relaxed),
        );
        emit_counter(
            &mut out,
            "sandstar_rest_requests_total",
            "REST requests served (any method or status).",
            self.rest_requests.load(Ordering::Relaxed),
        );
        emit_counter(
            &mut out,
            "sandstar_ipc_requests_total",
            "IPC requests served to the local CLI over UDS/TCP.",
            self.ipc_requests.load(Ordering::Relaxed),
        );
        emit_counter(
            &mut out,
            "sandstar_history_points_total",
            "Total historical sample rows recorded.",
            self.history_points.load(Ordering::Relaxed),
        );
        emit_counter(
            &mut out,
            "sandstar_ws_total",
            "Total WebSocket connections accepted since start.",
            self.ws_total.load(Ordering::Relaxed),
        );
        emit_counter(
            &mut out,
            "sandstar_ws_messages_in_total",
            "Frames received on the /api/ws endpoint.",
            self.ws_messages_in.load(Ordering::Relaxed),
        );
        emit_counter(
            &mut out,
            "sandstar_ws_messages_out_total",
            "Frames sent on the /api/ws endpoint.",
            self.ws_messages_out.load(Ordering::Relaxed),
        );
        emit_counter(
            &mut out,
            "sandstar_rows_total",
            "Total RoWS (/api/rows SOX-over-WS) sessions accepted since start.",
            self.rows_total.load(Ordering::Relaxed),
        );
        emit_counter(
            &mut out,
            "sandstar_rows_messages_in_total",
            "Frames received on the /api/rows endpoint.",
            self.rows_messages_in.load(Ordering::Relaxed),
        );
        emit_counter(
            &mut out,
            "sandstar_rows_messages_out_total",
            "Frames sent on the /api/rows endpoint.",
            self.rows_messages_out.load(Ordering::Relaxed),
        );
        emit_counter(
            &mut out,
            "sandstar_driver_sync_total",
            "Phase 12.0D: total sync_all invocations across all drivers.",
            self.driver_sync_total.load(Ordering::Relaxed),
        );
        emit_counter(
            &mut out,
            "sandstar_driver_sync_errors_total",
            "Phase 12.0B: total per-point errors returned by any driver's sync_cur.",
            self.driver_sync_errors_total.load(Ordering::Relaxed),
        );
        emit_counter(
            &mut out,
            "sandstar_driver_cov_events_total",
            "Phase 12.0D: total CovEvents broadcast from the driver actor.",
            self.driver_cov_events_total.load(Ordering::Relaxed),
        );

        emit_gauge_u64(
            &mut out,
            "sandstar_poll_duration_microseconds_last",
            "Duration of the last poll cycle in microseconds.",
            self.poll_duration_us_last.load(Ordering::Relaxed),
        );
        emit_gauge_u64(
            &mut out,
            "sandstar_poll_duration_microseconds_max",
            "Maximum poll-cycle duration observed since start (microseconds).",
            self.poll_duration_us_max.load(Ordering::Relaxed),
        );
        emit_gauge_i64(
            &mut out,
            "sandstar_watch_active",
            "Currently-active Haystack watches.",
            self.watch_active.load(Ordering::Relaxed),
        );
        emit_gauge_i64(
            &mut out,
            "sandstar_ws_active",
            "Currently-connected /api/ws WebSocket clients.",
            self.ws_active.load(Ordering::Relaxed),
        );
        emit_gauge_i64(
            &mut out,
            "sandstar_rows_active",
            "Currently-connected /api/rows WebSocket clients.",
            self.rows_active.load(Ordering::Relaxed),
        );

        out
    }
}

// ── Private helpers for Prometheus text format ─────────────

fn emit_counter(out: &mut String, name: &str, help: &str, value: u64) {
    writeln!(out, "# HELP {name} {help}").unwrap();
    writeln!(out, "# TYPE {name} counter").unwrap();
    writeln!(out, "{name} {value}").unwrap();
}

fn emit_gauge_u64(out: &mut String, name: &str, help: &str, value: u64) {
    writeln!(out, "# HELP {name} {help}").unwrap();
    writeln!(out, "# TYPE {name} gauge").unwrap();
    writeln!(out, "{name} {value}").unwrap();
}

fn emit_gauge_i64(out: &mut String, name: &str, help: &str, value: i64) {
    writeln!(out, "# HELP {name} {help}").unwrap();
    writeln!(out, "# TYPE {name} gauge").unwrap();
    writeln!(out, "{name} {value}").unwrap();
}

/// Minimal escaping for Prometheus label values — backslash, newline,
/// and double-quote. Label-value per the exposition format spec.
fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '"' => out.push_str(r#"\""#),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_label_handles_specials() {
        assert_eq!(escape_label("ok"), "ok");
        assert_eq!(escape_label(r#"has"quote"#), r#"has\"quote"#);
        assert_eq!(escape_label("a\nb"), r"a\nb");
        assert_eq!(escape_label(r"back\slash"), r"back\\slash");
    }

    #[test]
    fn to_prometheus_contains_expected_sections() {
        let text = metrics().to_prometheus("2.8.9", "abc1234");

        // Build info line with labels.
        assert!(text.contains(r#"sandstar_build_info{version="2.8.9",git_hash="abc1234"} 1"#));

        // A couple of counters we expect to see.
        assert!(text.contains("# TYPE sandstar_poll_count counter"));
        assert!(text.contains("# TYPE sandstar_driver_cov_events_total counter"));
        assert!(text.contains("# TYPE sandstar_driver_sync_errors_total counter"));

        // A couple of gauges we expect to see.
        assert!(text.contains("# TYPE sandstar_ws_active gauge"));
        assert!(text.contains("# TYPE sandstar_watch_active gauge"));
    }

    #[test]
    fn to_prometheus_is_well_formed_per_spec() {
        let text = metrics().to_prometheus("v", "h");
        // Every line is either a comment (# HELP|TYPE) or `<metric_name>[{labels}] <value>`.
        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert!(
                line.contains(' '),
                "metric line missing space separator: {line:?}"
            );
            let name = line.split(' ').next().unwrap();
            // Metric name must start with letter / underscore per the spec.
            let first = name.chars().next().unwrap();
            assert!(
                first.is_ascii_alphabetic() || first == '_',
                "invalid metric name start: {line:?}"
            );
        }
    }
}
