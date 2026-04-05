//! Lightweight internal metrics using atomic counters.
//!
//! No external dependencies — just `std::sync::atomic` and a JSON endpoint.

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
        })
    }
}
