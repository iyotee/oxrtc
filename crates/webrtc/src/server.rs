//! The server: its shared UDP socket, its ICE-TCP listener, its ICE
//! servers, and the answer to an offer.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oxrtc_core::{CancellationToken, ConsumerTag, Event, PerIpLimit, Stream, SubscribeError, Subscription, TrackInfo};
use str0m::config::DtlsCert;
use str0m::crypto::CryptoProvider;
use str0m::media::Mid;
use tokio::net::UdpSocket;
use tracing::{info, warn};

use crate::candidates::{LocalCandidates, host_candidates, offer_addresses};
use crate::msid::{new_stream_id, one_stream};
use crate::negotiate::{negotiate, outputs, rtc_config};
use crate::offer::{OfferedCodecs, parse_offer};
use crate::payload::choose;
use crate::registry::{Registry, Session, Via};
use crate::resource::{Answered, ResourceError, new_resource_id, offer_ufrag};
use crate::session;
use crate::socket::{bind_udp, receive_loop};
use crate::tcp::TcpEgress;
use crate::turn::servers::{IceServer, IceServers, IceSummary};
use crate::{WebRtcConfig, WhepError};

/// How long an offer answered right after startup waits for the ICE
/// servers' first answers (a TURN allocation takes two round trips).
const ICE_SETTLE: Duration = Duration::from_secs(2);

pub(crate) struct Shared {
    pub(crate) socket: Arc<UdpSocket>,
    pub(crate) local: SocketAddr,
    pub(crate) config: WebRtcConfig,
    pub(crate) registry: Mutex<Registry>,
    pub(crate) per_ip: PerIpLimit,
    next_id: AtomicU64,
    crypto: Arc<CryptoProvider>,
    cert: DtlsCert,
    pub(crate) cancel: CancellationToken,
    pub(crate) ice: IceServers,
    pub(crate) tcp: Option<Arc<TcpEgress>>,
}

impl Shared {
    /// The candidates an answer offers now.
    fn local_candidates(&self) -> LocalCandidates {
        let host = host_candidates(self.local, &self.config.candidates);
        let tcp = self.tcp.as_ref().map(|t| t.local_addr().port());
        LocalCandidates::gather(host, tcp, &self.ice.reflexive(), self.ice.relayed())
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
    /// Binds the UDP socket (and the ICE-TCP listener, when enabled: a
    /// listener that cannot bind is warned about, and only TCP is lost),
    /// starts the receive loops and the ICE servers' tasks, which stop (and
    /// every session with them) when `cancel` fires.
    pub async fn bind(config: WebRtcConfig, cancel: CancellationToken) -> io::Result<Self> {
        let socket = Arc::new(bind_udp(config.listen)?);
        let local = socket.local_addr()?;
        let crypto = Arc::new(str0m::crypto::from_feature_flags());
        let cert = crypto
            .dtls_provider
            .generate_certificate()
            .ok_or_else(|| io::Error::other("the crypto provider cannot generate a DTLS certificate"))?;
        let tcp = match config.tcp {
            // The UDP port, unless the system chose it (then its own).
            true => {
                let port = if config.listen.port() == 0 { 0 } else { local.port() };
                let listen = SocketAddr::new(config.listen.ip(), port);
                match TcpEgress::bind(listen, config.max_sessions, config.max_sessions_per_ip).await {
                    Ok(bound) => Some(bound),
                    Err(error) => {
                        warn!(%listen, %error, "webrtc tcp not available; UDP only");
                        None
                    }
                }
            }
            false => None,
        };
        let per_ip = PerIpLimit::new(config.max_sessions_per_ip);
        // A child: dropping the server stops it without cancelling the caller.
        let stop = cancel.child_token();
        let ice = IceServers::new(&config.ice_servers, &stop);
        let shared = Arc::new(Shared {
            socket,
            local,
            config,
            registry: Mutex::new(Registry::default()),
            per_ip,
            next_id: AtomicU64::new(1),
            crypto,
            cert,
            cancel: stop,
            ice,
            tcp: tcp.as_ref().map(|(egress, _)| egress.clone()),
        });
        tokio::spawn(receive_loop(shared.clone()));
        if let Some((egress, listener)) = tcp {
            let weak = Arc::downgrade(&shared);
            let dispatch: crate::tcp::Dispatch =
                Arc::new(move |data, remote, local| weak.upgrade()?.dispatch(data, remote, Via::Tcp(local)));
            tokio::spawn(egress.accept_loop(listener, dispatch, shared.cancel.clone()));
        }
        // Started even with no server: `set_ice_servers` may add some.
        let weak = Arc::downgrade(&shared);
        let deliver: crate::turn::driver::Deliver = Arc::new(move |data, peer, relayed| {
            if let Some(shared) = weak.upgrade() {
                shared.dispatch(data, peer, Via::Relay(relayed));
            }
        });
        let hmac = shared.crypto.sha1_hmac_provider;
        shared.ice.start(shared.socket.clone(), hmac, deliver);
        tokio::spawn(reap_loop(Arc::downgrade(&shared), shared.config.ice_retire_grace, shared.cancel.clone()));
        info!(%local, "webrtc listening");
        Ok(Self { shared })
    }

    /// Stops the receive loop and every session (also done on drop).
    pub fn shutdown(&self) {
        self.shared.cancel.cancel();
    }

    /// Replaces `webrtc.ice_servers` while the server runs, for the sessions
    /// made from now on: a TURN server whose credential changed gets a new
    /// allocation, and the old one keeps the sessions it was offered to until
    /// they end (D22). Nothing is restarted and no session is cut.
    pub fn set_ice_servers(&self, servers: &[IceServer]) -> IceSummary {
        self.shared.ice.replace(servers)
    }

    /// The bound UDP address.
    pub fn local_addr(&self) -> SocketAddr {
        self.shared.local
    }

    /// The ICE-TCP listener's address, when it is bound.
    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        self.shared.tcp.as_ref().map(|t| t.local_addr())
    }

    /// Sessions running now (connecting or connected).
    pub fn session_count(&self) -> usize {
        self.shared.registry().len()
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
        self.answer_as(stream, offer_sdp, remote, None).await
    }

    /// [`Self::answer`], the session listed by the `User-Agent` of the
    /// request that carried the offer, as go2rtc lists a WHEP consumer
    /// (openIVR names each of its sessions so).
    pub async fn answer_as(
        &self,
        stream: Arc<Stream>,
        offer_sdp: &str,
        remote: Option<SocketAddr>,
        user_agent: Option<&str>,
    ) -> Result<String, WhepError> {
        Ok(self.answer_resource(stream, offer_sdp, remote, user_agent).await?.sdp)
    }

    /// [`Self::answer_as`], also giving the id of the session resource the
    /// client reaches its session by ([`Self::trickle`], [`Self::close`]).
    pub async fn answer_resource(
        &self,
        stream: Arc<Stream>,
        offer_sdp: &str,
        remote: Option<SocketAddr>,
        user_agent: Option<&str>,
    ) -> Result<Answered, WhepError> {
        let shared = &self.shared;
        if shared.cancel.is_cancelled() {
            return Err(WhepError::Internal("server stopped".into()));
        }
        let reservation = shared.reserve(remote)?;
        let (offer, codecs) = parse_offer(offer_sdp)?;
        if codecs.is_empty() {
            return Err(WhepError::NoMatchingCodec);
        }
        let peer_ips = offer_addresses(offer_sdp);
        let mids: Vec<Mid> = offer.media_lines.iter().map(|line| line.mid()).collect();
        // go2rtc's name for a consumer whose offer came over WHEP.
        let mut tag = ConsumerTag::new("webrtc/whep");
        if let Some(remote) = remote {
            tag = tag.with_remote(remote.to_string());
        }
        if let Some(agent) = user_agent {
            tag = tag.with_user_agent(agent);
        }
        let wait = shared.config.answer_timeout;
        let subscribed = tokio::time::timeout(wait, subscribe(&stream, &codecs, tag));
        let (subscribed, ()) = tokio::join!(subscribed, shared.ice.settle(ICE_SETTLE.min(wait)));
        let (subscription, tracks) = subscribed
            .map_err(|_| WhepError::SourceUnavailable(format!("no tracks within {} ms", wait.as_millis())))??;
        let local = shared.local_candidates();
        let config = rtc_config(&shared.crypto, &shared.cert, &codecs, &tracks, local.host_only());
        let config = crate::negotiate::with_playout(config);
        let (mut rtc, answer) = negotiate(config, &local, offer)?;
        let outputs = outputs(&mut rtc, &mids);
        if !tracks.iter().any(|t| choose(&outputs, t).is_some()) {
            return Err(WhepError::NoMatchingCodec);
        }
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        let no_random = || WhepError::Internal("no random bytes".into());
        let resource = new_resource_id().ok_or_else(no_random)?;
        // The picture and the sound of the session as ONE MediaStream: str0m
        // names a stream per media line (`msid.rs`).
        let sdp = one_stream(&answer.to_sdp_string(), &new_stream_id().ok_or_else(no_random)?);
        let session = Arc::new(
            Session::new(id, rtc, shared.cancel.child_token(), local.host.clone(), local.tcp.clone())
                .with_resource(resource.clone(), offer_ufrag(offer_sdp)),
        );
        shared.register(reservation, session.clone());
        // The viewer's checks to a relayed candidate pass the server only
        // with a permission for its address: asked for before it sends them.
        if !local.relayed.is_empty() {
            for outgoing in shared.ice.permit(&peer_ips) {
                outgoing.send().await;
            }
        }
        info!(session = id, stream = stream.name(), relayed = local.relayed.len(), "webrtc session negotiated");
        tokio::spawn(session::run(shared.clone(), session, subscription, outputs, tracks));
        Ok(Answered { sdp, resource })
    }

    /// Adds the candidates of an `application/trickle-ice-sdpfrag` body to
    /// the session the resource `id` names (`PATCH`). Where the session is
    /// offered a relayed address, the TURN server is asked for permissions
    /// for the new candidates' addresses as it was for the offer's.
    pub async fn trickle(&self, id: &str, fragment: &str) -> Result<(), ResourceError> {
        let ips = self.shared.trickle(id, fragment)?;
        for outgoing in self.shared.ice.permit(&ips) {
            outgoing.send().await;
        }
        Ok(())
    }

    /// Ends the session the resource `id` names (`DELETE`), returning once
    /// its consumer has left the stream.
    pub async fn close(&self, id: &str) -> Result<(), ResourceError> {
        self.shared.close_resource(id).await
    }
}

/// Ends the retired TURN allocations no session uses any more, every
/// quarter of the grace (at most every 10 s) until the server stops.
async fn reap_loop(shared: std::sync::Weak<Shared>, grace: Duration, cancel: CancellationToken) {
    let every = (grace / 4).clamp(Duration::from_millis(50), Duration::from_secs(10));
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(every) => {}
        }
        let Some(shared) = shared.upgrade() else { return };
        shared.ice.reap(std::time::Instant::now(), grace, |relayed| shared.registry().uses_relay(relayed));
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

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
