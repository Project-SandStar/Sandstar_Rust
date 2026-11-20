//! An engine request that runs beside an event loop instead of inside one of
//! its `select!` arms (R-28, S-22).
//!
//! The `/api/ws` connection task and the SOX loop used to `await` engine
//! commands inside a `select!` arm. While the engine was busy or hung, the
//! loop handled nothing else (pings, client messages, DASP packets) for up to
//! [`ENGINE_REPLY_TIMEOUT`](super::ENGINE_REPLY_TIMEOUT). With [`EngineRound`]
//! the loop starts the request, keeps serving its other arms, and picks up
//! the result in a separate arm via [`EngineRound::next`].

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::time::Instant;

type BoxedRound<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// At most one in-flight request, plus an optional back-off before the next.
pub struct EngineRound<T> {
    fut: Option<BoxedRound<T>>,
    not_before: Option<Instant>,
}

impl<T> Default for EngineRound<T> {
    fn default() -> Self {
        Self {
            fut: None,
            not_before: None,
        }
    }
}

impl<T> EngineRound<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when no request is in flight and any back-off has passed.
    pub fn is_idle(&self) -> bool {
        self.fut.is_none() && self.not_before.is_none_or(|t| Instant::now() >= t)
    }

    /// True while a request is in flight.
    pub fn is_running(&self) -> bool {
        self.fut.is_some()
    }

    /// Start a request. Replaces (and drops) a request still in flight, so
    /// callers check [`is_running`](Self::is_running) or
    /// [`is_idle`](Self::is_idle) first. A pending back-off is kept: a
    /// request started during it (e.g. a client request in `/api/ws`, S-23)
    /// does not let the next back-off-respecting start happen early.
    pub fn start(&mut self, fut: impl Future<Output = T> + Send + 'static) {
        self.fut = Some(Box::pin(fut));
    }

    /// Do not start the next request before `delay` has passed.
    pub fn back_off(&mut self, delay: Duration) {
        self.not_before = Some(Instant::now() + delay);
    }

    /// Resolve with the result of the in-flight request; pending forever when
    /// there is none. Cancel-safe: if another `select!` arm wins, the request
    /// stays in flight and is polled again on the next call.
    pub async fn next(&mut self) -> T {
        match self.fut.as_mut() {
            Some(fut) => {
                let out = fut.await;
                self.fut = None;
                out
            }
            None => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn idle_round_never_resolves() {
        let mut r: EngineRound<u8> = EngineRound::new();
        assert!(r.is_idle());
        let res = tokio::time::timeout(Duration::from_secs(60), r.next()).await;
        assert!(res.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn result_survives_a_lost_select() {
        let mut r = EngineRound::new();
        r.start(async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            7u8
        });
        assert!(!r.is_idle());
        assert!(r.is_running());
        // Another arm wins first; the request must stay in flight.
        tokio::select! {
            _ = r.next() => panic!("round finished too early"),
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        assert!(r.is_running());
        assert_eq!(r.next().await, 7);
        assert!(r.is_idle());
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_started_during_a_back_off_keeps_it() {
        let mut r = EngineRound::new();
        r.back_off(Duration::from_secs(10));
        r.start(async { 1u8 });
        assert_eq!(r.next().await, 1);
        assert!(!r.is_idle(), "the back-off must survive the request");
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(r.is_idle());
    }

    #[tokio::test(start_paused = true)]
    async fn back_off_delays_the_next_start() {
        let mut r: EngineRound<u8> = EngineRound::new();
        r.back_off(Duration::from_secs(10));
        assert!(!r.is_idle());
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(!r.is_idle());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(r.is_idle());
    }
}
