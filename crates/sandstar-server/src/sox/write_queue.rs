//! SOX channel writes to the engine through one bounded queue and one
//! writer task (R-40, S-47).
//!
//! The SOX loop used to `tokio::spawn` one task per write (client `w`
//! packets and logic→channel bridge writes). Each task waited up to
//! [`ENGINE_REPLY_TIMEOUT`](crate::rest::ENGINE_REPLY_TIMEOUT) in the engine
//! command queue, which five clients share and which is not read during a
//! poll, so SOX could fill it and hold up REST and the editor. Now:
//!
//! - the SOX loop hands writes over with [`SoxWriteQueue::submit`], which
//!   never waits; a full queue or a non-finite value drops the write;
//! - a single writer sends them one at a time, so SOX waits on at most one
//!   write at a time. A write that timed out stays in the engine queue and
//!   may still run, so after a long hang up to one write per timeout period
//!   (10 s) can be left there;
//! - the writer takes every queued write at once and keeps only the last
//!   value per `(channel, level)`, in first-seen order;
//! - on an engine timeout the rest of that batch is dropped as stale.
//!
//! A dropped write is not retried: the logic→channel bridge writes on change,
//! so the channel keeps its old value until the next change. Every drop and
//! failure is counted and logged (one `warn` per minute for all of them, each
//! dropped write at `debug` with its channel, level and value).
//!
//! The writer ends when the queue is dropped (the SOX task ends).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::rest::{is_engine_timeout, EngineHandle};

/// Writes the SOX loop can hand over before new ones are dropped (S-47).
pub const SOX_WRITE_QUEUE: usize = 64;

/// Minimum gap between two warnings about dropped or failed writes.
const WARN_INTERVAL: Duration = Duration::from_secs(60);

/// One channel write from SOX.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteJob {
    pub channel: u32,
    pub value: f64,
    /// Priority level (8 for client `w`, 17 for logic→channel).
    pub level: u8,
    pub who: &'static str,
}

/// Counters and one shared warn limiter for both sides of the queue.
struct Report {
    dropped: AtomicU64,
    failed: AtomicU64,
    last_warn: Mutex<Option<Instant>>,
}

impl Report {
    fn new() -> Self {
        Self {
            dropped: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            last_warn: Mutex::new(None),
        }
    }

    /// Whether a warning may be logged now (at most one per `WARN_INTERVAL`).
    fn warn_due(&self) -> bool {
        let mut last = crate::lock::lock_or_recover(&self.last_warn, "SOX write report");
        let due = last.is_none_or(|t| t.elapsed() >= WARN_INTERVAL);
        if due {
            *last = Some(Instant::now());
        }
        due
    }

    fn drop_jobs(&self, jobs: &[WriteJob], reason: &'static str) {
        if jobs.is_empty() {
            return;
        }
        for j in jobs {
            debug!(
                channel = j.channel,
                level = j.level,
                value = j.value,
                who = j.who,
                reason,
                "SOX: channel write dropped"
            );
        }
        let n = jobs.len() as u64;
        let total = self
            .dropped
            .fetch_add(n, Ordering::Relaxed)
            .saturating_add(n);
        if self.warn_due() {
            warn!(
                dropped = n,
                total,
                reason,
                "SOX: channel writes dropped (R-40); not retried; further reports within 60 s at debug"
            );
        }
    }

    fn fail(&self, job: &WriteJob, err: &str) {
        let total = self
            .failed
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if self.warn_due() {
            warn!(
                channel = job.channel,
                level = job.level,
                who = job.who,
                failed_total = total,
                "SOX write failed: {err}; further reports within 60 s at debug"
            );
        } else {
            debug!(
                channel = job.channel,
                who = job.who,
                "SOX write failed: {err}"
            );
        }
    }
}

/// Sending side, owned by the SOX loop.
pub struct SoxWriteQueue {
    tx: mpsc::Sender<WriteJob>,
    report: Arc<Report>,
}

impl SoxWriteQueue {
    /// Start the writer task for `engine` and return the queue.
    pub fn spawn(engine: EngineHandle) -> Self {
        Self::spawn_with_capacity(engine, SOX_WRITE_QUEUE)
    }

    fn spawn_with_capacity(engine: EngineHandle, capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity);
        let report = Arc::new(Report::new());
        tokio::spawn(run_writer(rx, engine, report.clone()));
        Self { tx, report }
    }

    /// Hand a write to the writer without waiting. Returns `false` when it
    /// was dropped (non-finite value, queue full, or the writer is gone).
    pub fn submit(&mut self, job: WriteJob) -> bool {
        // The engine rejects these too; dropping here counts them (R-9).
        if !job.value.is_finite() {
            self.report.drop_jobs(&[job], "non-finite value");
            return false;
        }
        match self.tx.try_send(job) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(job)) => {
                self.report.drop_jobs(&[job], "queue full");
                false
            }
            Err(mpsc::error::TrySendError::Closed(job)) => {
                self.report.drop_jobs(&[job], "writer stopped");
                false
            }
        }
    }

    /// Writes dropped so far (non-finite, queue full, writer stopped, or
    /// stale after an engine timeout).
    pub fn dropped(&self) -> u64 {
        self.report.dropped.load(Ordering::Relaxed)
    }

    /// Writes the engine refused with an error other than a timeout.
    pub fn failed(&self) -> u64 {
        self.report.failed.load(Ordering::Relaxed)
    }
}

/// Add `job` to `batch`, replacing an earlier write to the same channel and
/// level (its place in the order is kept; value and `who` are the new ones).
fn push_coalesced(batch: &mut Vec<WriteJob>, job: WriteJob) {
    match batch
        .iter_mut()
        .find(|j| j.channel == job.channel && j.level == job.level)
    {
        Some(earlier) => *earlier = job,
        None => batch.push(job),
    }
}

async fn run_writer(mut rx: mpsc::Receiver<WriteJob>, engine: EngineHandle, report: Arc<Report>) {
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while let Ok(job) = rx.try_recv() {
            push_coalesced(&mut batch, job);
        }
        let mut jobs = batch.into_iter();
        while let Some(job) = jobs.next() {
            match engine
                .write_channel(
                    job.channel,
                    Some(job.value),
                    job.level,
                    job.who.to_string(),
                    0.0,
                )
                .await
            {
                Ok(()) => {}
                // EngineHandle already logs timeouts (rate-limited). The
                // rest of this batch is stale by now.
                Err(e) if is_engine_timeout(&e) => {
                    let stale: Vec<WriteJob> = jobs.collect();
                    report.drop_jobs(&stale, "stale after engine timeout");
                    break;
                }
                Err(e) => report.fail(&job, &e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rest::EngineCmd;

    fn job(channel: u32, value: f64, level: u8) -> WriteJob {
        WriteJob {
            channel,
            value,
            level,
            who: "sox",
        }
    }

    type Seen = Arc<Mutex<Vec<(u32, Option<f64>, u8, String)>>>;

    #[derive(Clone, Copy)]
    enum Mode {
        Answer,
        Hang,
        /// Refuse writes to channel 1, answer the rest.
        FailChannel1,
    }

    /// Engine that records writes and answers them as `mode` says.
    fn engine(mode: Mode) -> (EngineHandle, Seen) {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let (tx, mut rx) = mpsc::channel::<EngineCmd>(64);
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Some(cmd) = rx.recv().await {
                if let EngineCmd::WriteChannel {
                    channel,
                    value,
                    level,
                    who,
                    reply,
                    ..
                } = cmd
                {
                    log.lock().unwrap().push((channel, value, level, who));
                    match mode {
                        Mode::Answer => {
                            let _ = reply.send(Ok(()));
                        }
                        Mode::FailChannel1 if channel == 1 => {
                            let _ = reply.send(Err("channel not found".into()));
                        }
                        Mode::FailChannel1 => {
                            let _ = reply.send(Ok(()));
                        }
                        Mode::Hang => held.push(reply),
                    }
                }
            }
        });
        (EngineHandle::new(tx), seen)
    }

    fn writes(seen: &Seen) -> Vec<(u32, Option<f64>, u8)> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|(c, v, l, _)| (*c, *v, *l))
            .collect()
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn writes_reach_the_engine_with_their_level_and_who() {
        let (eh, seen) = engine(Mode::Answer);
        let mut q = SoxWriteQueue::spawn(eh);
        assert!(q.submit(job(1, 1.5, 8)));
        settle().await;
        assert!(q.submit(WriteJob {
            who: "sox-logic",
            ..job(2, 2.0, 17)
        }));
        settle().await;
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                (1, Some(1.5), 8, "sox".to_string()),
                (2, Some(2.0), 17, "sox-logic".to_string())
            ]
        );
        assert_eq!(q.dropped(), 0);
    }

    #[tokio::test]
    async fn a_batch_keeps_the_last_value_per_channel_and_level() {
        let (eh, seen) = engine(Mode::Answer);
        let mut q = SoxWriteQueue::spawn(eh);
        // Submitted before the writer runs: taken as one batch.
        for j in [
            job(1, 1.0, 8),
            job(2, 2.0, 8),
            job(1, 1.1, 8),
            job(1, 9.0, 17),
            job(1, 1.2, 8),
        ] {
            assert!(q.submit(j));
        }
        settle().await;
        assert_eq!(
            writes(&seen),
            vec![(1, Some(1.2), 8), (2, Some(2.0), 8), (1, Some(9.0), 17)]
        );
    }

    #[tokio::test]
    async fn a_full_queue_drops_without_waiting() {
        let (eh, _seen) = engine(Mode::Hang);
        let mut q = SoxWriteQueue::spawn_with_capacity(eh, 2);
        assert!(q.submit(job(1, 1.0, 8)));
        assert!(q.submit(job(2, 1.0, 8)));
        // The writer has not run yet; the queue holds two.
        assert!(!q.submit(job(3, 1.0, 8)));
        assert!(!q.submit(job(4, 1.0, 8)));
        assert_eq!(q.dropped(), 2);
    }

    #[tokio::test]
    async fn a_non_finite_value_is_dropped_before_the_queue() {
        let (eh, seen) = engine(Mode::Answer);
        let mut q = SoxWriteQueue::spawn(eh);
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(!q.submit(job(1, v, 8)));
        }
        settle().await;
        assert_eq!(q.dropped(), 3);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn at_most_one_write_waits_in_the_engine() {
        let (eh, seen) = engine(Mode::Hang);
        let mut q = SoxWriteQueue::spawn(eh);
        for ch in 0..10 {
            assert!(q.submit(job(ch, 1.0, 8)));
        }
        settle().await;
        assert_eq!(seen.lock().unwrap().len(), 1, "one write in flight");
    }

    #[tokio::test(start_paused = true)]
    async fn an_engine_timeout_drops_the_rest_of_the_batch() {
        let (eh, seen) = engine(Mode::Hang);
        let mut q = SoxWriteQueue::spawn(eh);
        for ch in 0..5 {
            assert!(q.submit(job(ch, 1.0, 8)));
        }
        // Past the 10 s reply timeout of the first write.
        tokio::time::sleep(crate::rest::ENGINE_REPLY_TIMEOUT + Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(q.dropped(), 4, "four stale writes");
        assert_eq!(q.failed(), 0);
        // The next writes are sent again, at their own levels.
        assert!(q.submit(job(9, 2.0, 17)));
        assert!(q.submit(job(9, 3.0, 8)));
        settle().await;
        assert_eq!(writes(&seen).get(1), Some(&(9, Some(2.0), 17)));
    }

    #[tokio::test]
    async fn an_engine_error_is_counted_and_the_batch_goes_on() {
        let (eh, seen) = engine(Mode::FailChannel1);
        let mut q = SoxWriteQueue::spawn(eh);
        for ch in [1, 2, 3] {
            assert!(q.submit(job(ch, 1.0, 8)));
        }
        settle().await;
        assert_eq!(writes(&seen).len(), 3, "the rest of the batch is sent");
        assert_eq!(q.failed(), 1);
        assert_eq!(q.dropped(), 0);
    }

    #[tokio::test]
    async fn the_writer_ends_when_the_queue_is_dropped() {
        let (tx, mut rx) = mpsc::channel::<EngineCmd>(4);
        let q = SoxWriteQueue::spawn(EngineHandle::new(tx));
        drop(q);
        // The writer held the only other EngineHandle; once it ends the
        // engine channel closes.
        let closed = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(matches!(closed, Ok(None)), "writer did not end");
    }

    #[tokio::test(start_paused = true)]
    async fn warnings_are_limited_to_one_a_minute_for_both_sides() {
        let r = Report::new();
        assert!(r.warn_due());
        assert!(!r.warn_due());
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(!r.warn_due());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(r.warn_due());
        // Drops and failures share the limiter.
        r.drop_jobs(&[job(1, 1.0, 8)], "queue full");
        r.fail(&job(2, 1.0, 8), "boom");
        assert_eq!(r.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(r.failed.load(Ordering::Relaxed), 1);
        assert!(!r.warn_due(), "neither reset the limiter early");
    }

    #[test]
    fn coalescing_keeps_first_seen_order() {
        let mut b = Vec::new();
        for j in [
            job(3, 1.0, 8),
            job(1, 1.0, 8),
            WriteJob {
                who: "sox-logic",
                ..job(3, 2.0, 8)
            },
        ] {
            push_coalesced(&mut b, j);
        }
        assert_eq!(
            b,
            vec![
                WriteJob {
                    who: "sox-logic",
                    ..job(3, 2.0, 8)
                },
                job(1, 1.0, 8)
            ]
        );
    }
}
