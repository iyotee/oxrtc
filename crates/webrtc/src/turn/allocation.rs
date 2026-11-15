//! One TURN allocation over UDP as a state machine, without I/O (as str0m
//! is): datagrams from the server go in through [`Allocation::handle`],
//! datagrams for it come out of [`Allocation::poll_transmit`], and time
//! moves through [`Allocation::handle_timeout`].
//!
//! Allocate with the long-term credential (RFC 8656 section 7, after the
//! server's 401 challenge), Refresh before the lifetime lapses,
//! CreatePermission for every peer IP sent to, ChannelBind for every peer
//! address, Send indications until its channel is bound, then ChannelData.

use std::collections::VecDeque;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use crate::stun::{ChannelData, Class, Encoder, Hmac, Message, attr, channel_data, method};
use crate::turn::peers::Peers;
use crate::turn::transactions::{MAX_IN_FLIGHT, Transactions, transaction_id};

/// The lifetime asked for, in seconds (RFC 8656's default).
const REQUESTED_LIFETIME: u32 = 600;
/// A lifetime outside these bounds is read as the nearest one.
const MIN_LIFETIME: u32 = 60;
const MAX_LIFETIME: u32 = 3600;
/// Stale-nonce answers followed in a row before giving up.
const MAX_STALE: u8 = 3;
/// Longest REALM and NONCE read (RFC 8489: fewer than 128 characters,
/// up to 763 bytes).
const MAX_TEXT: usize = 763;
/// Datagrams waiting for the driver at most; beyond, control requests are
/// sent at their next retransmission instead.
const MAX_OUT: usize = 256;
/// Places in the request table only the allocation's own requests
/// (Allocate, Refresh) may take: a burst of peer requests the server has not
/// answered yet never leaves the refresh without a place, which would lose
/// the allocation and every relayed session with it.
const RESERVED_FOR_ALLOCATION: usize = 2;

/// A TURN user name and password. Never displayed: they are what the relay
/// bills to the installation.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Credentials {
    pub(crate) username: String,
    pub(crate) password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials(***)")
    }
}

/// Why an allocation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The server refused the credentials (a second 401).
    Refused,
    /// An error answer: its code and (shortened) reason.
    Error(u16, String),
    /// The server did not answer.
    NoAnswer,
    /// The allocation lapsed or its refresh was refused.
    Lost(&'static str),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused => f.write_str("credentials refused"),
            Self::Error(code, reason) => write!(f, "error {code} {reason}"),
            Self::NoAnswer => f.write_str("no answer"),
            Self::Lost(why) => f.write_str(why),
        }
    }
}

/// A reason phrase from the server, safe to log: short and printable.
fn shown(reason: &str) -> String {
    reason.chars().take(64).map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// What happened to the allocation, for the driver to log and act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    Allocated { relayed: SocketAddr, mapped: Option<SocketAddr>, lifetime: Duration },
    Failed(Failure),
}

/// What a datagram from the server was.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Incoming<'a> {
    /// Data a peer sent to the relayed address.
    Relayed { peer: SocketAddr, data: &'a [u8] },
    /// An answer to one of our requests.
    Control,
    /// Anything else: dropped.
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Request {
    Allocate,
    Refresh(u32),
    Permission(IpAddr),
    Bind(u16, SocketAddr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Allocating,
    Allocated { relayed: SocketAddr, expires: Instant, refresh_at: Instant },
    Failed,
    Closed,
}

struct Auth {
    realm: String,
    nonce: String,
    key: [u8; 16],
}

pub(crate) struct Allocation {
    credentials: Credentials,
    hmac: Hmac,
    auth: Option<Auth>,
    state: State,
    tx: Transactions<Request>,
    peers: Peers,
    out: VecDeque<Vec<u8>>,
    events: VecDeque<Event>,
    stale: u8,
    indications: u64,
}

impl Allocation {
    /// Starts allocating: the first request is ready to be sent.
    pub(crate) fn new(credentials: Credentials, hmac: Hmac, now: Instant) -> Self {
        let mut allocation = Self {
            credentials,
            hmac,
            auth: None,
            state: State::Allocating,
            tx: Transactions::default(),
            peers: Peers::default(),
            out: VecDeque::new(),
            events: VecDeque::new(),
            stale: 0,
            indications: 0,
        };
        allocation.request(now, Request::Allocate);
        allocation
    }

    /// The relayed address, once allocated.
    pub(crate) fn relayed(&self) -> Option<SocketAddr> {
        match self.state {
            State::Allocated { relayed, .. } => Some(relayed),
            _ => None,
        }
    }

    /// Whether it is allocating or allocated (not failed, not closed).
    pub(crate) fn is_alive(&self) -> bool {
        matches!(self.state, State::Allocating | State::Allocated { .. })
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    pub(crate) fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.out.pop_front()
    }

    /// When [`Allocation::handle_timeout`] is next needed.
    pub(crate) fn poll_timeout(&self) -> Option<Instant> {
        let state = match self.state {
            State::Allocated { expires, refresh_at, .. } => {
                let refreshing = self.tx.any(|k| matches!(k, Request::Refresh(_)));
                Some(if refreshing { expires } else { refresh_at.min(expires) })
            }
            _ => None,
        };
        [self.tx.next_due(), state, self.peers.next_due()].into_iter().flatten().min()
    }

    pub(crate) fn handle_timeout(&mut self, now: Instant) {
        self.flush(now);
        let State::Allocated { expires, refresh_at, .. } = self.state else { return };
        if now >= expires {
            return self.fail(Failure::Lost("the allocation lapsed"));
        }
        if now >= refresh_at && !self.tx.any(|k| matches!(k, Request::Refresh(_))) {
            self.request(now, Request::Refresh(REQUESTED_LIFETIME));
        }
        for ip in self.peers.permissions_due(now) {
            self.request(now, Request::Permission(ip));
        }
        for (number, peer) in self.peers.channels_due(now) {
            self.request(now, Request::Bind(number, peer));
        }
    }

    /// Asks for a permission for `ip` ahead of any data (an offer's
    /// candidate, whose checks the server would otherwise drop).
    pub(crate) fn permit(&mut self, now: Instant, ip: IpAddr) {
        if matches!(self.state, State::Allocated { .. }) && self.peers.want_permission(ip, now) {
            self.request(now, Request::Permission(ip));
        }
    }

    /// The datagram that carries `data` to `peer` through the relay:
    /// ChannelData once a channel is bound, a Send indication until then.
    /// `None` when not allocated.
    pub(crate) fn send(&mut self, now: Instant, peer: SocketAddr, data: &[u8]) -> Option<Vec<u8>> {
        if !matches!(self.state, State::Allocated { .. }) {
            return None;
        }
        if self.peers.want_permission(peer.ip(), now) {
            self.request(now, Request::Permission(peer.ip()));
        }
        if let Some(number) = self.peers.channel(peer, now) {
            return channel_data(number, data);
        }
        if let Some(number) = self.peers.want_channel(peer, now) {
            self.request(now, Request::Bind(number, peer));
        }
        // An indication's id needs no secrecy: nothing answers it.
        self.indications = self.indications.wrapping_add(1);
        let [a, b, c, d, e, f, g, h] = self.indications.to_be_bytes();
        let id = [0, 0, 0, 0, a, b, c, d, e, f, g, h];
        let message = Encoder::new(Class::Indication, method::SEND, id).xor_address(attr::XOR_PEER_ADDRESS, peer);
        Some(message.attr(attr::DATA, data).finish(None, false))
    }

    /// Releases the allocation (a Refresh with a lifetime of 0, sent once).
    pub(crate) fn close(&mut self, now: Instant) {
        if matches!(self.state, State::Allocated { .. }) {
            self.request(now, Request::Refresh(0));
        }
        self.state = State::Closed;
    }

    /// A datagram from the server.
    pub(crate) fn handle<'a>(&mut self, now: Instant, datagram: &'a [u8]) -> Incoming<'a> {
        let allocated = matches!(self.state, State::Allocated { .. });
        if datagram.first().is_some_and(|b| (0x40..=0x4F).contains(b)) {
            let Some(message) = ChannelData::parse(datagram) else { return Incoming::Ignored };
            return match self.peers.peer_of(message.channel) {
                Some(peer) if allocated => Incoming::Relayed { peer, data: message.data },
                _ => Incoming::Ignored,
            };
        }
        let Ok(message) = Message::parse(datagram) else { return Incoming::Ignored };
        if !message.fingerprint_ok() {
            return Incoming::Ignored;
        }
        match message.class {
            Class::Indication if message.method == method::DATA && allocated => {
                match (message.xor_address(attr::XOR_PEER_ADDRESS), message.get(attr::DATA)) {
                    (Some(peer), Some(data)) => Incoming::Relayed { peer, data },
                    _ => Incoming::Ignored,
                }
            }
            Class::Success | Class::Error => {
                let Some((kind, sent)) = self.tx.take_sent(&message.transaction) else { return Incoming::Ignored };
                if message.class == Class::Success {
                    self.succeeded(now, kind, &message);
                } else {
                    self.errored(now, kind, &message, &sent);
                }
                Incoming::Control
            }
            _ => Incoming::Ignored,
        }
    }

    fn fail(&mut self, failure: Failure) {
        if self.is_alive() {
            self.state = State::Failed;
            self.events.push_back(Event::Failed(failure));
        }
    }

    fn request(&mut self, now: Instant, kind: Request) {
        let room = match kind {
            Request::Allocate | Request::Refresh(_) => MAX_IN_FLIGHT,
            Request::Permission(_) | Request::Bind(..) => MAX_IN_FLIGHT - RESERVED_FOR_ALLOCATION,
        };
        if self.tx.len() >= room {
            return self.not_sent(now, kind);
        }
        let Some(id) = transaction_id() else { return self.unanswered(now, kind) };
        let bytes = self.encode(&kind, id);
        if !self.tx.start(now, id, kind.clone(), bytes) {
            return self.not_sent(now, kind);
        }
        self.flush(now);
    }

    /// A request the table had no place for: a peer's is asked for again at
    /// its next use, in a moment, not after a server's refusal's minute.
    fn not_sent(&mut self, now: Instant, kind: Request) {
        match kind {
            Request::Permission(ip) => self.peers.permission_not_sent(ip, now),
            Request::Bind(number, _) => self.peers.channel_not_sent(number, now),
            Request::Allocate | Request::Refresh(_) => self.unanswered(now, kind),
        }
    }

    /// Moves the datagrams due now to the output, and settles the requests
    /// that were never answered.
    fn flush(&mut self, now: Instant) {
        let mut due = Vec::new();
        for kind in self.tx.handle_timeout(now, &mut due) {
            self.unanswered(now, kind);
        }
        let room = MAX_OUT.saturating_sub(self.out.len());
        self.out.extend(due.into_iter().take(room));
    }

    fn encode(&self, kind: &Request, id: [u8; 12]) -> Vec<u8> {
        let message = match kind {
            Request::Allocate => Encoder::new(Class::Request, method::ALLOCATE, id)
                .udp_transport()
                .u32(attr::LIFETIME, REQUESTED_LIFETIME),
            Request::Refresh(lifetime) => {
                Encoder::new(Class::Request, method::REFRESH, id).u32(attr::LIFETIME, *lifetime)
            }
            Request::Permission(ip) => Encoder::new(Class::Request, method::CREATE_PERMISSION, id)
                .xor_address(attr::XOR_PEER_ADDRESS, SocketAddr::new(*ip, 0)),
            Request::Bind(number, peer) => Encoder::new(Class::Request, method::CHANNEL_BIND, id)
                .channel(*number)
                .xor_address(attr::XOR_PEER_ADDRESS, *peer),
        };
        match &self.auth {
            None => message.finish(None, true),
            Some(auth) => message
                .attr(attr::USERNAME, self.credentials.username.as_bytes())
                .attr(attr::REALM, auth.realm.as_bytes())
                .attr(attr::NONCE, auth.nonce.as_bytes())
                .finish(Some((&auth.key, self.hmac)), true),
        }
    }

    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn peer_counts(&self) -> (usize, usize) {
        self.peers.counts()
    }

    /// The ids of the requests in flight (for a harness to answer them).
    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn in_flight(&self) -> Vec<[u8; 12]> {
        self.tx.ids()
    }
}

#[path = "answers.rs"]
mod answers;

#[cfg(test)]
#[path = "allocation_tests.rs"]
mod tests;
