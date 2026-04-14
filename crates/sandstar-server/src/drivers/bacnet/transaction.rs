//! BACnet transaction management — invoke ID allocation and request/response matching.
//!
//! Phase B3 will implement the full async request/response cycle with
//! retransmission, segmentation, and window-flow control. This skeleton
//! provides the types and infrastructure that the rest of the driver will
//! build on.
//!
//! # Invoke IDs
//!
//! BACnet Confirmed-Request APDUs carry an 8-bit invoke ID chosen by the
//! originator. The originator must match every response (Simple-Ack,
//! Complex-Ack, Error, Reject, Abort) back to the outstanding request
//! using this field. We allocate IDs sequentially and wrap at 255.

use std::collections::HashMap;

use tokio::sync::oneshot;

use super::{frame::Apdu, BacnetError};

// ── Internal types ─────────────────────────────────────────

/// A pending confirmed request waiting for its matching response APDU.
struct PendingRequest {
    /// Channel half that delivers the result to the waiter.
    sender: oneshot::Sender<Result<Apdu, BacnetError>>,
}

// ── TransactionTable ───────────────────────────────────────

/// Manages in-flight BACnet transactions keyed by invoke ID.
///
/// BACnet allows 256 concurrent invoke IDs (0–255). We track which IDs
/// are in-flight and route responses to the correct waiter via a
/// `tokio::sync::oneshot` channel.
///
/// Typical usage (Phase B3):
/// 1. `allocate()` — obtain an invoke ID and a receiver for the reply.
/// 2. Send a Confirmed-Request APDU using that invoke ID over UDP.
/// 3. When a response arrives, call `dispatch(invoke_id, apdu)`.
/// 4. The waiter wakes up with the result.
///
/// If no reply arrives within the retransmit timeout, call `timeout(id)`
/// to cancel the waiter with a `BacnetError::Timeout`.
#[derive(Default)]
pub struct TransactionTable {
    pending: HashMap<u8, PendingRequest>,
    next_id: u8,
}

impl TransactionTable {
    /// Create an empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next available invoke ID and register a waiter.
    ///
    /// Returns `Some((invoke_id, receiver))` on success, or `None` if all
    /// 256 IDs are currently in-flight (extremely unlikely in practice).
    ///
    /// The returned `receiver` will resolve when `dispatch` or `timeout`
    /// is called for the same `invoke_id`.
    pub fn allocate(&mut self) -> Option<(u8, oneshot::Receiver<Result<Apdu, BacnetError>>)> {
        use std::collections::hash_map::Entry;
        // Try up to 256 candidates starting from `next_id`.
        for _ in 0u16..256 {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            if let Entry::Vacant(slot) = self.pending.entry(id) {
                let (tx, rx) = oneshot::channel();
                slot.insert(PendingRequest { sender: tx });
                return Some((id, rx));
            }
        }
        None
    }

    /// Route a received APDU to the correct waiter.
    ///
    /// If no waiter is registered for `invoke_id` (e.g. it already timed
    /// out) the APDU is silently discarded.
    pub fn dispatch(&mut self, invoke_id: u8, apdu: Apdu) {
        if let Some(pending) = self.pending.remove(&invoke_id) {
            let _ = pending.sender.send(Ok(apdu));
        }
    }

    /// Cancel a pending request with a `BacnetError::Timeout`.
    ///
    /// Removes the entry from the table and wakes the waiter with an
    /// error carrying the configured retry count.
    pub fn timeout(&mut self, invoke_id: u8) {
        if let Some(pending) = self.pending.remove(&invoke_id) {
            let _ = pending.sender.send(Err(BacnetError::Timeout(3)));
        }
    }

    /// Number of in-flight requests.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

// ── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_apdu(invoke_id: u8) -> Apdu {
        Apdu::Other {
            pdu_type: 0x30,
            invoke_id,
            data: vec![],
        }
    }

    // ── Allocation ──────────────────────────────────────────

    #[test]
    fn allocate_starts_at_zero() {
        let mut t = TransactionTable::new();
        let (id, _rx) = t.allocate().expect("first allocation should succeed");
        assert_eq!(id, 0, "first invoke ID should be 0");
    }

    #[test]
    fn allocate_returns_sequential_ids() {
        let mut t = TransactionTable::new();
        let ids: Vec<u8> = (0..5).map(|_| t.allocate().unwrap().0).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn allocate_returns_none_when_all_256_in_flight() {
        let mut t = TransactionTable::new();
        // Hold all receivers so none are dropped (which would free the slot).
        let mut receivers = Vec::with_capacity(256);
        for _ in 0..256 {
            let (_, rx) = t.allocate().expect("should allocate 256 IDs");
            receivers.push(rx);
        }
        assert_eq!(t.pending_count(), 256);
        assert!(
            t.allocate().is_none(),
            "257th allocation should return None"
        );
    }

    #[test]
    fn ids_wrap_around_at_255() {
        let mut t = TransactionTable::new();
        // Consume IDs 0-254, dispatch each immediately to free the slot.
        for expected_id in 0u8..=254 {
            let (id, _rx) = t.allocate().unwrap();
            assert_eq!(id, expected_id);
            t.dispatch(id, dummy_apdu(id));
        }
        // ID 255 next.
        let (id_255, _rx) = t.allocate().unwrap();
        assert_eq!(id_255, 255);
        t.dispatch(255, dummy_apdu(255));
        // After wrapping: next_id is now 0 and all prior slots are free.
        let (wrapped, _rx) = t.allocate().unwrap();
        assert_eq!(wrapped, 0, "should wrap back to 0");
    }

    // ── Dispatch ────────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_routes_to_correct_waiter() {
        let mut t = TransactionTable::new();
        let (id, rx) = t.allocate().unwrap();
        t.dispatch(id, dummy_apdu(id));
        let result = rx.await.expect("sender should not be dropped");
        match result {
            Ok(Apdu::Other { invoke_id, .. }) => assert_eq!(invoke_id, id),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn dispatch_unknown_id_is_silent() {
        let mut t = TransactionTable::new();
        // Dispatch for an ID that was never allocated — must not panic.
        t.dispatch(77, dummy_apdu(77));
        assert_eq!(t.pending_count(), 0);
    }

    #[test]
    fn dispatch_removes_pending_entry() {
        let mut t = TransactionTable::new();
        let (id, _rx) = t.allocate().unwrap();
        assert_eq!(t.pending_count(), 1);
        t.dispatch(id, dummy_apdu(id));
        assert_eq!(t.pending_count(), 0);
    }

    // ── Timeout ─────────────────────────────────────────────

    #[tokio::test]
    async fn timeout_cancels_pending_request() {
        let mut t = TransactionTable::new();
        let (id, rx) = t.allocate().unwrap();
        t.timeout(id);
        let result = rx.await.expect("sender should not be dropped");
        match result {
            Err(BacnetError::Timeout(retries)) => assert_eq!(retries, 3),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn timeout_unknown_id_is_silent() {
        let mut t = TransactionTable::new();
        t.timeout(200); // Never allocated — must not panic.
        assert_eq!(t.pending_count(), 0);
    }

    #[test]
    fn timeout_removes_pending_entry() {
        let mut t = TransactionTable::new();
        let (id, _rx) = t.allocate().unwrap();
        assert_eq!(t.pending_count(), 1);
        t.timeout(id);
        assert_eq!(t.pending_count(), 0);
    }

    // ── Reuse after free ────────────────────────────────────

    #[test]
    fn freed_slot_can_be_reallocated() {
        let mut t = TransactionTable::new();
        let (id0, _rx0) = t.allocate().unwrap(); // ID 0
        let (id1, _rx1) = t.allocate().unwrap(); // ID 1

        // Free id0 via timeout.
        t.timeout(id0);

        // Allocate 254 more to exhaust IDs 2-255.
        for _ in 2u16..=255 {
            t.allocate().unwrap();
        }
        // All 255 slots (1 + 254) are taken; id0 is free.
        assert_eq!(t.pending_count(), 255);
        let (recycled, _rx) = t.allocate().expect("id0 slot should be reusable");
        assert_eq!(recycled, id0, "recycled slot should be id0");
        let _ = id1; // keep alive
    }
}
