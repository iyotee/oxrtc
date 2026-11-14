//! The UDP socket every session shares, the session registry, and the
//! answer to an offer.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use oxrtc_core::{
    CancellationToken, ConsumerTag, Event, PerIpLimit, PerIpPermit, Stream, SubscribeError, Subscription, TrackInfo,
};
use str0m::Input;
use str0m::config::DtlsCert;
use str0m::crypto::CryptoProvider;
use str0m::media::Mid;
use str0m::net::{DatagramRecv, Protocol, Receive};
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::candidates::{arrival_address, host_candidates};
use crate::negotiate::{negotiate, outputs, rtc_config};
use crate::offer::{OfferedCodecs, parse_offer};
use crate::payload::choose;
use crate::session::{self, Peer};
use crate::{WebRtcConfig, WhepError};

/// Addresses one session can be known by (ICE may try several pairs).
pub(crate) const MAX_ADDRS_PER_SESSION: usize = 8;
/// Largest datagram read: the largest UDP payload. WebRTC stays under the
/// path MTU, but a smaller buffer turns an oversized datagram into a receive
/// *error* on Windows (`WSAEMSGSIZE`), which a stranger could repeat at will.
const RECV_BUFFER: usize = 65_536;
/// Kernel receive buffer asked for the shared socket. Every session's STUN,
/// DTLS and RTCP arrive on this one socket; at the Windows default (64 KiB,
/// seven 9000-byte datagrams) a stranger's flood filled it and the kernel
/// dropped a real client's binding requests: 3 connections in 10 failed in
/// the test `oversized_junk_datagrams_do_not_stall_the_socket`, 10 in 10
/// succeeded in about 105 ms with 4 MiB. Linux clamps it to `rmem_max`.
const SOCKET_RECV_BUFFER: usize = 4 << 20;
/// Pause after a socket error that is not about one datagram.
const ERROR_PAUSE: std::time::Duration = std::time::Duration::from_millis(10);

/// One session as the receive loop sees it.
pub(crate) struct Session {
    pub(crate) id: u64,
    pub(crate) peer: Mutex<Peer>,
    pub(crate) wake: Notify,
    pub(crate) cancel: CancellationToken,
    pub(crate) candidates: Vec<SocketAddr>,
}

impl Session {
    pub(crate) fn peer(&self) -> MutexGuard<'_, Peer> {
        self.peer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hands a datagram to this session if its `Rtc` accepts it.
    fn offer(&self, now: Instant, data: &[u8], source: SocketAddr, bound: SocketAddr) -> bool {
        let Ok(contents) = DatagramRecv::try_from(data) else { return false };
        let destination = arrival_address(bound, &self.candidates, source);
        let input = Input::Receive(now, Receive { proto: Protocol::Udp, source, destination, contents });
        let mut peer = self.peer();
        if !peer.rtc.accepts(&input) {
            return false;
        }
        peer.last_rx = now;
        // This runs on the receive loop every session shares: a panic in
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
struct Registry {
    sessions: HashMap<u64, Arc<Session>>,
    by_addr: HashMap<SocketAddr, u64>,
    reserved: usize,
    /// What each session counts against its requester's address.
    addresses: HashMap<u64, PerIpPermit>,
}

pub(crate) struct Shared {
    pub(crate) socket: UdpSocket,
    pub(crate) local: SocketAddr,
    pub(crate) config: WebRtcConfig,
    registry: Mutex<Registry>,
    per_ip: PerIpLimit,
    next_id: AtomicU64,
    crypto: Arc<CryptoProvider>,
    cert: DtlsCert,
    pub(crate) cancel: CancellationToken,
}

impl Shared {
    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A place for a session requested from `remote` (the WHEP caller): one
    /// of `max_sessions`, and one of `max_sessions_per_ip` for that address.
    /// A caller that names no address is counted in the total only.
    fn reserve(self: &Arc<Self>, remote: Option<SocketAddr>) -> Result<Reservation, WhepError> {
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

    fn register(&self, mut reservation: Reservation, session: Arc<Session>) {
        let mut registry = self.registry();
        registry.reserved = registry.reserved.saturating_sub(1);
        if let Some(address) = reservation.address.take() {
            registry.addresses.insert(session.id, address);
        }
        registry.sessions.insert(session.id, session);
        reservation.held = false;
    }

    pub(crate) fn unregister(&self, id: u64) {
        let mut registry = self.registry();
        registry.sessions.remove(&id);
        registry.by_addr.retain(|_, owner| *owner != id);
        registry.addresses.remove(&id);
    }

    fn remember(&self, addr: SocketAddr, id: u64) {
        let mut registry = self.registry();
        let known = registry.by_addr.values().filter(|owner| **owner == id).count();
        if known < MAX_ADDRS_PER_SESSION && registry.sessions.contains_key(&id) {
            registry.by_addr.insert(addr, id);
        }
    }

    #[cfg(test)]
    fn addrs_of(&self, id: u64) -> usize {
        self.registry().by_addr.values().filter(|owner| **owner == id).count()
    }

    /// Finds the session a datagram belongs to: by its source address, else
    /// by asking each session (STUN carries the session's ICE user name).
    fn dispatch(&self, data: &[u8], source: SocketAddr) {
        let now = Instant::now();
        let known = {
            let registry = self.registry();
            registry.by_addr.get(&source).and_then(|id| registry.sessions.get(id)).cloned()
        };
        if let Some(session) = known
            && session.offer(now, data, source, self.local)
        {
            return;
        }
        let sessions: Vec<Arc<Session>> = self.registry().sessions.values().cloned().collect();
        for session in sessions {
            if session.offer(now, data, source, self.local) {
                self.remember(source, session.id);
                return;
            }
        }
    }
}

/// A place counted against `max_sessions` (and the caller's address) while
/// an offer is answered.
struct Reservation {
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

/// The WebRTC egress: one UDP socket, any number of sessions up to the limit.
pub struct WebRtcServer {
    shared: Arc<Shared>,
}

impl Drop for WebRtcServer {
    fn drop(&mut self) {
        self.shared.cancel.cancel();
    }
}

impl std::fmt::Debug for WebRtcServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebRtcServer").field("local", &self.shared.local).finish()
    }
}

impl WebRtcServer {
    /// Binds the UDP socket and starts the receive loop, which stops (and
    /// every session with it) when `cancel` fires.
    pub async fn bind(config: WebRtcConfig, cancel: CancellationToken) -> io::Result<Self> {
        let socket = bind_socket(config.listen)?;
        let local = socket.local_addr()?;
        let crypto = Arc::new(str0m::crypto::from_feature_flags());
        let cert = crypto
            .dtls_provider
            .generate_certificate()
            .ok_or_else(|| io::Error::other("the crypto provider cannot generate a DTLS certificate"))?;
        let per_ip = PerIpLimit::new(config.max_sessions_per_ip);
        let shared = Arc::new(Shared {
            socket,
            local,
            config,
            registry: Mutex::new(Registry::default()),
            per_ip,
            next_id: AtomicU64::new(1),
            crypto,
            cert,
            // A child: dropping the server stops it without cancelling the caller.
            cancel: cancel.child_token(),
        });
        tokio::spawn(receive_loop(shared.clone()));
        info!(%local, "webrtc listening");
        Ok(Self { shared })
    }

    /// Stops the receive loop and every session (also done on drop).
    pub fn shutdown(&self) {
        self.shared.cancel.cancel();
    }

    /// The bound UDP address.
    pub fn local_addr(&self) -> SocketAddr {
        self.shared.local
    }

    /// Sessions running now (connecting or connected).
    pub fn session_count(&self) -> usize {
        self.shared.registry().sessions.len()
    }

    /// Answers a WHEP offer for `stream`: subscribes to what the offer can
    /// receive, waits for the stream's tracks, and returns the complete SDP
    /// answer. The session then runs until the peer leaves, stays silent for
    /// the idle timeout, or the stream closes.
    pub async fn answer(
        &self,
        stream: Arc<Stream>,
        offer_sdp: &str,
        remote: Option<SocketAddr>,
    ) -> Result<String, WhepError> {
        let shared = &self.shared;
        if shared.cancel.is_cancelled() {
            return Err(WhepError::Internal("server stopped".into()));
        }
        let reservation = shared.reserve(remote)?;
        let (offer, codecs) = parse_offer(offer_sdp)?;
        if codecs.is_empty() {
            return Err(WhepError::NoMatchingCodec);
        }
        let mids: Vec<Mid> = offer.media_lines.iter().map(|line| line.mid()).collect();
        let mut tag = ConsumerTag::new("webrtc");
        if let Some(remote) = remote {
            tag = tag.with_remote(remote.to_string());
        }
        let wait = shared.config.answer_timeout;
        let (subscription, tracks) = tokio::time::timeout(wait, subscribe(&stream, &codecs, tag))
            .await
            .map_err(|_| WhepError::SourceUnavailable(format!("no tracks within {} ms", wait.as_millis())))??;
        let candidates = host_candidates(shared.local, &shared.config.candidates);
        let config = rtc_config(&shared.crypto, &shared.cert, &codecs, &tracks);
        let (mut rtc, answer) = negotiate(config, &candidates, offer)?;
        let outputs = outputs(&mut rtc, &mids);
        if !tracks.iter().any(|t| choose(&outputs, t).is_some()) {
            return Err(WhepError::NoMatchingCodec);
        }
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        let session = Arc::new(Session {
            id,
            peer: Mutex::new(Peer { rtc, last_rx: Instant::now() }),
            wake: Notify::new(),
            cancel: shared.cancel.child_token(),
            candidates,
        });
        shared.register(reservation, session.clone());
        info!(session = id, stream = stream.name(), "webrtc session negotiated");
        tokio::spawn(session::run(shared.clone(), session, subscription, outputs, tracks));
        Ok(answer.to_sdp_string())
    }
}

async fn subscribe(
    stream: &Arc<Stream>,
    codecs: &OfferedCodecs,
    tag: ConsumerTag,
) -> Result<(Subscription, Arc<[TrackInfo]>), WhepError> {
    let mut subscription = stream.subscribe(&codecs.want(), tag).await.map_err(|e| subscribe_error(stream, e))?;
    loop {
        match subscription.recv().await {
            Ok(Event::Tracks(tracks)) => return Ok((subscription, tracks)),
            Ok(_) => continue,
            Err(end) => return Err(WhepError::SourceUnavailable(end.to_string())),
        }
    }
}

/// A source that never connected is unavailable; one that connected (or
/// declared its output) without a wanted codec is a codec mismatch.
fn subscribe_error(stream: &Stream, error: SubscribeError) -> WhepError {
    match error {
        SubscribeError::NoMatchingSource => {
            let snapshot = stream.snapshot();
            let connected = snapshot.sources.iter().any(|s| !s.tracks.is_empty());
            let attempted = snapshot.sources.iter().any(|s| !matches!(s.state, oxrtc_core::SourceState::Idle));
            if !connected && attempted {
                WhepError::SourceUnavailable("no source delivered tracks".into())
            } else {
                WhepError::NoMatchingCodec
            }
        }
        SubscribeError::NothingWanted => WhepError::NoMatchingCodec,
        // The stream's own consumer cap: a capacity limit, like ours.
        SubscribeError::TooManyConsumers => WhepError::TooManySessions,
        other => WhepError::SourceUnavailable(other.to_string()),
    }
}

/// Binds the shared UDP socket with a receive buffer of [`SOCKET_RECV_BUFFER`].
/// A system that refuses the size keeps its default: a warning, not a failure.
fn bind_socket(listen: SocketAddr) -> io::Result<UdpSocket> {
    let raw =
        socket2::Socket::new(socket2::Domain::for_address(listen), socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    if let Err(error) = raw.set_recv_buffer_size(SOCKET_RECV_BUFFER) {
        warn!(%error, "the webrtc socket keeps the system's receive buffer");
    }
    debug!(bytes = raw.recv_buffer_size().unwrap_or(0), "webrtc receive buffer");
    raw.bind(&listen.into())?;
    raw.set_nonblocking(true)?;
    UdpSocket::from_std(raw.into())
}

async fn receive_loop(shared: Arc<Shared>) {
    let mut buf = vec![0u8; RECV_BUFFER];
    loop {
        let received = tokio::select! {
            biased;
            () = shared.cancel.cancelled() => break,
            received = shared.socket.recv_from(&mut buf) => received,
        };
        match received {
            Ok((len, source)) => {
                if let Some(data) = buf.get(..len) {
                    shared.dispatch(data, source);
                }
            }
            // About one datagram (an ICMP error of an earlier send, a
            // truncated datagram): the next one is read at once, or a
            // stranger could stall every session by repeating it.
            Err(error) if per_datagram(&error) => {}
            Err(error) => {
                debug!(%error, "udp receive failed");
                tokio::time::sleep(ERROR_PAUSE).await;
            }
        }
    }
    debug!("webrtc receive loop stopped");
}

/// Whether a receive error concerns a single datagram rather than the socket.
fn per_datagram(error: &io::Error) -> bool {
    // WSAEMSGSIZE and WSAENETRESET (TTL expired) on Windows.
    const WINDOWS_PER_DATAGRAM: [i32; 2] = [10040, 10052];
    matches!(error.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused)
        || (cfg!(windows) && error.raw_os_error().is_some_and(|code| WINDOWS_PER_DATAGRAM.contains(&code)))
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
