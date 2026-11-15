//! The address a STUN server sees the shared socket as (a server-reflexive
//! candidate, RFC 8445 section 5.1.1.1), asked for again from time to time:
//! the NAT in front of the server may change it.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::stun::{Class, Encoder, Message, attr, method};
use crate::turn::transactions::{Transactions, transaction_id};

/// The mapping is asked for again this long after an answer...
const REPROBE: Duration = Duration::from_secs(300);
/// ...and this long after a request nobody answered.
const RETRY: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(crate) struct Reflexive {
    tx: Transactions<()>,
    mapped: Option<SocketAddr>,
    next_probe: Option<Instant>,
    out: VecDeque<Vec<u8>>,
    /// Whether an answer, or a failure, has been seen at least once.
    settled: bool,
}

impl Reflexive {
    /// Starts with a request due at once.
    pub(crate) fn new(now: Instant) -> Self {
        Self { next_probe: Some(now), ..Self::default() }
    }

    /// The mapped address, once a server answered.
    pub(crate) fn mapped(&self) -> Option<SocketAddr> {
        self.mapped
    }

    /// Whether the first probe ended, answered or not.
    pub(crate) fn settled(&self) -> bool {
        self.settled
    }

    pub(crate) fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.out.pop_front()
    }

    pub(crate) fn poll_timeout(&self) -> Option<Instant> {
        [self.tx.next_due(), self.next_probe].into_iter().flatten().min()
    }

    pub(crate) fn handle_timeout(&mut self, now: Instant) {
        if self.next_probe.is_some_and(|at| at <= now) {
            self.next_probe = None;
            match transaction_id() {
                Some(id) => {
                    let request = Encoder::new(Class::Request, method::BINDING, id).finish(None, true);
                    self.tx.start(now, id, (), request);
                }
                None => self.next_probe = Some(now + RETRY),
            }
        }
        let mut due = Vec::new();
        if !self.tx.handle_timeout(now, &mut due).is_empty() {
            self.settled = true;
            self.next_probe = Some(now + RETRY);
        }
        self.out.extend(due);
    }

    /// A datagram from the STUN server: `true` when it answered our request.
    pub(crate) fn handle(&mut self, now: Instant, datagram: &[u8]) -> bool {
        let Ok(message) = Message::parse(datagram) else { return false };
        if message.method != method::BINDING || !message.fingerprint_ok() {
            return false;
        }
        if !matches!(message.class, Class::Success | Class::Error) || self.tx.take(&message.transaction).is_none() {
            return false;
        }
        self.settled = true;
        if message.class == Class::Success
            && let Some(mapped) = message.xor_address(attr::XOR_MAPPED_ADDRESS)
            && !mapped.ip().is_unspecified()
        {
            self.mapped = Some(mapped);
            self.next_probe = Some(now + REPROBE);
        } else {
            self.next_probe = Some(now + RETRY);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_mapped_address_is_learnt_and_asked_again() {
        let start = Instant::now();
        let mut probe = Reflexive::new(start);
        assert!(!probe.settled());
        probe.handle_timeout(start);
        let request = probe.poll_transmit().unwrap();
        let parsed = Message::parse(&request).unwrap();
        assert_eq!((parsed.class, parsed.method), (Class::Request, method::BINDING));
        let mapped: SocketAddr = "198.51.100.4:8555".parse().unwrap();
        let answer = Encoder::new(Class::Success, method::BINDING, parsed.transaction)
            .xor_address(attr::XOR_MAPPED_ADDRESS, mapped)
            .finish(None, true);
        assert!(probe.handle(start, &answer));
        assert!(!probe.handle(start, &answer), "answered once");
        assert_eq!(probe.mapped(), Some(mapped));
        assert!(probe.settled());
        assert_eq!(probe.poll_timeout(), Some(start + REPROBE));
    }

    #[test]
    fn a_silent_server_settles_and_is_retried() {
        let start = Instant::now();
        let mut probe = Reflexive::new(start);
        let mut now = start;
        loop {
            probe.handle_timeout(now);
            while probe.poll_transmit().is_some() {}
            if probe.settled() {
                break;
            }
            now = probe.poll_timeout().unwrap();
        }
        assert_eq!(probe.mapped(), None);
        assert_eq!(probe.poll_timeout(), Some(now + RETRY));
        // Not a binding answer: ignored.
        let other = Encoder::new(Class::Success, method::ALLOCATE, [1; 12]).finish(None, true);
        assert!(!probe.handle(now, &other));
    }
}
