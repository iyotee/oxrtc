//! The sessions the receive paths know, and which session a datagram from
//! a peer belongs to - whether it came on the shared socket, through a TURN
//! relay or on an ICE-TCP connection.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use oxrtc_core::{CancellationToken, PerIpPermit};
use str0m::Input;
use str0m::net::{DatagramRecv, Protocol, Receive};
use tokio::sync::Notify;
use tracing::{debug, warn};

use crate::WhepError;
use crate::candidates::arrival_address;
use crate::server::Shared;
use crate::session::Peer;

/// Addresses one session can be known by (ICE may try several pairs).
pub(crate) const MAX_ADDRS_PER_SESSION: usize = 8;

/// How a datagram reached this server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Via {
    /// The shared UDP socket.
    Udp,
    /// A TURN relay: the relayed address the peer sent to.
    Relay(SocketAddr),
    /// An ICE-TCP connection accepted on this local address.
    Tcp(SocketAddr),
}

/// One session as the receive paths see it.
pub(crate) struct Session {
    pub(crate) id: u64,
    pub(crate) peer: Mutex<Peer>,
    pub(crate) wake: Notify,
    pub(crate) cancel: CancellationToken,
    /// Its UDP host candidates, and its TCP ones.
    pub(crate) candidates: Vec<SocketAddr>,
    pub(crate) tcp_candidates: Vec<SocketAddr>,
}

impl Session {
    pub(crate) fn peer(&self) -> MutexGuard<'_, Peer> {
        self.peer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The local candidate a datagram from `source` arrived on.
    fn destination(&self, source: SocketAddr, via: Via, bound: SocketAddr) -> (Protocol, SocketAddr) {
        match via {
            Via::Udp => (Protocol::Udp, arrival_address(bound, &self.candidates, source)),
            Via::Relay(relayed) => (Protocol::Udp, relayed),
            // The connection knows its local address; when it is not one of
            // the advertised ones (a NAT's), the first of its family stands in.
            Via::Tcp(local) => {
                let advertised = self.tcp_candidates.contains(&local);
                let first = self.tcp_candidates.iter().find(|c| c.is_ipv4() == local.is_ipv4());
                (Protocol::Tcp, if advertised { local } else { first.copied().unwrap_or(local) })
            }
        }
    }

    /// Hands a datagram to this session if its `Rtc` accepts it.
    pub(crate) fn offer(&self, now: Instant, data: &[u8], source: SocketAddr, via: Via, bound: SocketAddr) -> bool {
        let Ok(contents) = DatagramRecv::try_from(data) else { return false };
        let (proto, destination) = self.destination(source, via, bound);
        let input = Input::Receive(now, Receive { proto, source, destination, contents });
        let mut peer = self.peer();
        if !peer.rtc.accepts(&input) {
            return false;
        }
        peer.last_rx = now;
        // This runs on a receive loop every session shares: a panic in
        // str0m on one peer's datagram must end that session, not the loop.
        match std::panic::catch_unwind(AssertUnwindSafe(|| peer.rtc.handle_input(input))) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                debug!(session = self.id, %error, "input refused, ending the session");
                self.cancel.cancel();
            }
            Err(_) => {
                warn!(session = self.id, "str0m panicked on a datagram, ending the session");
                self.cancel.cancel();
            }
        }
        drop(peer);
        self.wake.notify_one();
        true
    }
}

#[derive(Default)]
pub(crate) struct Registry {
    sessions: HashMap<u64, Arc<Session>>,
    by_route: HashMap<(Via, SocketAddr), u64>,
    pub(crate) reserved: usize,
    /// What each session counts against its requester's address.
    addresses: HashMap<u64, PerIpPermit>,
}

impl Registry {
    pub(crate) fn len(&self) -> usize {
        self.sessions.len()
    }
}

/// A place counted against `max_sessions` (and the caller's address) while
/// an offer is answered.
pub(crate) struct Reservation {
    shared: Arc<Shared>,
    held: bool,
    address: Option<PerIpPermit>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.held {
            let mut registry = self.shared.registry();
            registry.reserved = registry.reserved.saturating_sub(1);
        }
    }
}

impl Shared {
    pub(crate) fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A place for a session requested from `remote` (the WHEP caller): one
    /// of `max_sessions`, and one of `max_sessions_per_ip` for that address.
    /// A caller that names no address is counted in the total only.
    pub(crate) fn reserve(self: &Arc<Self>, remote: Option<SocketAddr>) -> Result<Reservation, WhepError> {
        let mut registry = self.registry();
        if registry.sessions.len() + registry.reserved >= self.config.max_sessions {
            return Err(WhepError::TooManySessions);
        }
        let address = match remote {
            Some(remote) => match self.per_ip.try_acquire(remote.ip()) {
                Some(permit) => Some(permit),
                None => {
                    if let Some(refused) = self.per_ip.refused() {
                        warn!(%remote, limit = self.per_ip.max_per_ip(), refused, "per-address session limit reached");
                    }
                    return Err(WhepError::TooManySessions);
                }
            },
            None => None,
        };
        registry.reserved += 1;
        Ok(Reservation { shared: self.clone(), held: true, address })
    }

    pub(crate) fn register(&self, mut reservation: Reservation, session: Arc<Session>) {
        let mut registry = self.registry();
        registry.reserved = registry.reserved.saturating_sub(1);
        if let Some(address) = reservation.address.take() {
            registry.addresses.insert(session.id, address);
        }
        registry.sessions.insert(session.id, session);
        reservation.held = false;
    }

    pub(crate) fn unregister(&self, id: u64) {
        {
            let mut registry = self.registry();
            registry.sessions.remove(&id);
            registry.by_route.retain(|_, owner| *owner != id);
            registry.addresses.remove(&id);
        }
        if let Some(tcp) = &self.tcp {
            tcp.release(id);
        }
    }

    pub(crate) fn remember(&self, route: (Via, SocketAddr), id: u64) {
        let mut registry = self.registry();
        let known = registry.by_route.values().filter(|owner| **owner == id).count();
        if known < MAX_ADDRS_PER_SESSION && registry.sessions.contains_key(&id) {
            registry.by_route.insert(route, id);
        }
    }

    #[cfg(test)]
    pub(crate) fn addrs_of(&self, id: u64) -> usize {
        self.registry().by_route.values().filter(|owner| **owner == id).count()
    }

    /// Finds the session a datagram belongs to: by the route it came by,
    /// else by asking each session (STUN carries the session's ICE user
    /// name). The session that took it, if any.
    pub(crate) fn dispatch(&self, data: &[u8], source: SocketAddr, via: Via) -> Option<u64> {
        let now = Instant::now();
        let route = (via, source);
        let known = {
            let registry = self.registry();
            registry.by_route.get(&route).and_then(|id| registry.sessions.get(id)).cloned()
        };
        if let Some(session) = known
            && session.offer(now, data, source, via, self.local)
        {
            return Some(session.id);
        }
        let sessions: Vec<Arc<Session>> = self.registry().sessions.values().cloned().collect();
        for session in sessions {
            if session.offer(now, data, source, via, self.local) {
                self.remember(route, session.id);
                return Some(session.id);
            }
        }
        None
    }
}
