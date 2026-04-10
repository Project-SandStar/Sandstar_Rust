//! In-memory history/trending storage for channel values.
//!
//! Maintains a VecDeque ring buffer per polled channel, capturing all
//! values each poll cycle. Queryable via REST endpoint.
//!
//! Memory: ~32 bytes per point × 1000 points × 138 channels ≈ 4.3MB

use std::collections::{HashMap, VecDeque};

use sandstar_engine::EngineStatus;
use serde::Serialize;

/// A single historical data point for a channel.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryPoint {
    /// Unix epoch seconds when this value was captured.
    pub ts: u64,
    /// Converted engineering-units value.
    pub cur: f64,
    /// Raw sensor/ADC value.
    pub raw: f64,
    /// Channel status at time of capture.
    #[serde(serialize_with = "serialize_status")]
    pub status: EngineStatus,
}

fn serialize_status<S: serde::Serializer>(status: &EngineStatus, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(status.as_str())
}

/// Ring buffer for one channel's history.
struct ChannelHistory {
    buffer: VecDeque<HistoryPoint>,
    max_cap: usize,
}

impl ChannelHistory {
    fn new(capacity: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(capacity.min(1024)),
            max_cap: capacity,
        }
    }

    fn push(&mut self, point: HistoryPoint) {
        if self.buffer.len() >= self.max_cap {
            self.buffer.pop_front();
        }
        self.buffer.push_back(point);
    }

    fn query(&self, since_unix: u64, limit: usize) -> Vec<HistoryPoint> {
        self.buffer
            .iter()
            .filter(|p| p.ts >= since_unix)
            .take(limit)
            .cloned()
            .collect()
    }

    fn len(&self) -> usize {
        self.buffer.len()
    }
}

/// Container for all channel histories.
pub struct HistoryStore {
    histories: HashMap<u32, ChannelHistory>,
    max_per_channel: usize,
}

impl HistoryStore {
    /// Create a new history store.
    pub fn new(max_per_channel: usize) -> Self {
        Self {
            histories: HashMap::new(),
            max_per_channel,
        }
    }

    /// Record a data point for a channel.
    pub fn record(&mut self, channel: u32, point: HistoryPoint) {
        self.histories
            .entry(channel)
            .or_insert_with(|| ChannelHistory::new(self.max_per_channel))
            .push(point);
    }

    /// Query history for a channel.
    ///
    /// Returns points with `ts >= since_unix`, limited to `limit` results.
    /// Returns empty vec if channel has no history.
    pub fn query(&self, channel: u32, since_unix: u64, limit: usize) -> Vec<HistoryPoint> {
        self.histories
            .get(&channel)
            .map(|h| h.query(since_unix, limit))
            .unwrap_or_default()
    }

    /// Get statistics about the history store.
    pub fn stats(&self) -> (usize, usize) {
        let channels = self.histories.len();
        let total: usize = self.histories.values().map(|h| h.len()).sum();
        (channels, total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_query() {
        let mut store = HistoryStore::new(5);
        for i in 0..3 {
            store.record(
                1113,
                HistoryPoint {
                    ts: 1000 + i,
                    cur: 72.0 + i as f64,
                    raw: 2048.0,
                    status: EngineStatus::Ok,
                },
            );
        }

        let points = store.query(1113, 0, 100);
        assert_eq!(points.len(), 3);
        assert_eq!(points[0].ts, 1000);
        assert_eq!(points[2].cur, 74.0);
    }

    #[test]
    fn ring_buffer_eviction() {
        let mut store = HistoryStore::new(3);
        for i in 0..5 {
            store.record(
                100,
                HistoryPoint {
                    ts: i,
                    cur: i as f64,
                    raw: 0.0,
                    status: EngineStatus::Ok,
                },
            );
        }

        let points = store.query(100, 0, 100);
        assert_eq!(points.len(), 3);
        // Oldest 2 evicted, remaining: ts=2,3,4
        assert_eq!(points[0].ts, 2);
        assert_eq!(points[2].ts, 4);
    }

    #[test]
    fn query_with_since_filter() {
        let mut store = HistoryStore::new(10);
        for i in 0..5 {
            store.record(
                200,
                HistoryPoint {
                    ts: 100 + i,
                    cur: i as f64,
                    raw: 0.0,
                    status: EngineStatus::Ok,
                },
            );
        }

        let points = store.query(200, 103, 100);
        assert_eq!(points.len(), 2); // ts=103, ts=104
        assert_eq!(points[0].ts, 103);
    }

    #[test]
    fn query_with_limit() {
        let mut store = HistoryStore::new(100);
        for i in 0..10 {
            store.record(
                300,
                HistoryPoint {
                    ts: i,
                    cur: 0.0,
                    raw: 0.0,
                    status: EngineStatus::Ok,
                },
            );
        }

        let points = store.query(300, 0, 3);
        assert_eq!(points.len(), 3);
    }

    #[test]
    fn query_unknown_channel() {
        let store = HistoryStore::new(10);
        let points = store.query(9999, 0, 100);
        assert!(points.is_empty());
    }

    #[test]
    fn stats() {
        let mut store = HistoryStore::new(10);
        store.record(
            1,
            HistoryPoint {
                ts: 0,
                cur: 0.0,
                raw: 0.0,
                status: EngineStatus::Ok,
            },
        );
        store.record(
            1,
            HistoryPoint {
                ts: 1,
                cur: 0.0,
                raw: 0.0,
                status: EngineStatus::Ok,
            },
        );
        store.record(
            2,
            HistoryPoint {
                ts: 0,
                cur: 0.0,
                raw: 0.0,
                status: EngineStatus::Ok,
            },
        );

        let (channels, total) = store.stats();
        assert_eq!(channels, 2);
        assert_eq!(total, 3);
    }
}
