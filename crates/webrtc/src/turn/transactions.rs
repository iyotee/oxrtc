//! Requests in flight to a STUN/TURN server over UDP: retransmitted until
//! answered (RFC 8489 section 6.2.1), then given up.

use std::time::{Duration, Instant};

use crate::stun::TransactionId;

/// First retransmission timeout (RFC 8489's recommended RTO).
const INITIAL_RTO: Duration = Duration::from_millis(500);
/// Sends of one request at most (RFC 8489's Rc is 7; 6 gives up sooner).
const MAX_SENDS: u32 = 6;
/// How long the last send is waited for (RFC 8489's Rm x RTO, 16 x 500 ms).
const LAST_WAIT: Duration = Duration::from_secs(8);
/// Requests in flight at once; one more is refused.
pub(crate) const MAX_IN_FLIGHT: usize = 32;

/// A fresh, unpredictable transaction id: an off-path attacker who cannot
/// guess it cannot forge an answer to a request (the answers that carry no
/// MESSAGE-INTEGRITY, errors and the first 401, rest on this alone).
pub(crate) fn transaction_id() -> Option<TransactionId> {
    let mut id = [0u8; 12];
    getrandom::fill(&mut id).ok()?;
    Some(id)
}

struct Pending<K> {
    id: TransactionId,
    kind: K,
    bytes: Vec<u8>,
    due: Instant,
    rto: Duration,
    sends: u32,
}

/// The table of requests in flight.
pub(crate) struct Transactions<K> {
    pending: Vec<Pending<K>>,
}

impl<K> Default for Transactions<K> {
    fn default() -> Self {
        Self { pending: Vec::new() }
    }
}

impl<K: Clone> Transactions<K> {
    /// Adds a request, due to be sent at once. `false` when the table is full.
    pub(crate) fn start(&mut self, now: Instant, id: TransactionId, kind: K, bytes: Vec<u8>) -> bool {
        if self.pending.len() >= MAX_IN_FLIGHT {
            return false;
        }
        self.pending.push(Pending { id, kind, bytes, due: now, rto: INITIAL_RTO, sends: 0 });
        true
    }

    /// The request a response answers, removed from the table.
    pub(crate) fn take(&mut self, id: &TransactionId) -> Option<K> {
        self.take_sent(id).map(|(kind, _)| kind)
    }

    /// The same, with the datagram that was sent.
    pub(crate) fn take_sent(&mut self, id: &TransactionId) -> Option<(K, Vec<u8>)> {
        let index = self.pending.iter().position(|p| &p.id == id)?;
        let pending = self.pending.swap_remove(index);
        Some((pending.kind, pending.bytes))
    }

    /// Whether a request of a kind matching `f` is in flight.
    pub(crate) fn any(&self, mut f: impl FnMut(&K) -> bool) -> bool {
        self.pending.iter().any(|p| f(&p.kind))
    }

    /// Requests in flight.
    pub(crate) fn len(&self) -> usize {
        self.pending.len()
    }

    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn ids(&self) -> Vec<TransactionId> {
        self.pending.iter().map(|p| p.id).collect()
    }

    /// The datagrams due now (first sends and retransmissions) go to `out`;
    /// the requests that were never answered are returned.
    pub(crate) fn handle_timeout(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) -> Vec<K> {
        let mut expired = Vec::new();
        self.pending.retain_mut(|p| {
            if p.due > now {
                return true;
            }
            if p.sends >= MAX_SENDS {
                expired.push(p.kind.clone());
                return false;
            }
            out.push(p.bytes.clone());
            p.sends += 1;
            if p.sends >= MAX_SENDS {
                p.due = now + LAST_WAIT;
            } else {
                p.due = now + p.rto;
                p.rto = p.rto.saturating_mul(2);
            }
            true
        });
        expired
    }

    /// When the next send or give-up is due.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        self.pending.iter().map(|p| p.due).min()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn requests_are_retransmitted_then_given_up() {
        let start = Instant::now();
        let mut table = Transactions::default();
        assert!(table.start(start, [1; 12], "allocate", vec![1, 2, 3]));
        let mut out = Vec::new();
        let mut now = start;
        let mut sends = 0;
        loop {
            out.clear();
            let expired = table.handle_timeout(now, &mut out);
            sends += out.len();
            if !expired.is_empty() {
                assert_eq!(expired, vec!["allocate"]);
                break;
            }
            now = table.next_due().unwrap();
        }
        assert_eq!(sends, MAX_SENDS as usize);
        // 0.5 + 1 + 2 + 4 + 8 between the six sends, then the last wait.
        assert_eq!(now - start, Duration::from_millis(15_500) + LAST_WAIT);
        assert!(table.next_due().is_none());
    }

    #[test]
    fn an_answer_removes_its_request_and_the_table_is_bounded() {
        let now = Instant::now();
        let mut table = Transactions::default();
        for i in 0..MAX_IN_FLIGHT {
            assert!(table.start(now, [i as u8; 12], i, Vec::new()));
        }
        assert!(!table.start(now, [0xff; 12], 99, Vec::new()), "one more than the bound");
        assert_eq!(table.take(&[3; 12]), Some(3));
        assert_eq!(table.take(&[3; 12]), None, "answered once");
        assert_eq!(table.len(), MAX_IN_FLIGHT - 1);
        assert!(table.any(|k| *k == 4));
    }

    #[test]
    fn transaction_ids_differ() {
        assert_ne!(transaction_id().unwrap(), transaction_id().unwrap());
    }
}
