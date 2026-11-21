//! `GET /api/ws?src=<stream>`: go2rtc's WebSocket API (F2, D39), the
//! signalling Home Assistant's go2rtc integration, the WebRTC Camera
//! component, Frigate's cards and go2rtc's own player use.
//!
//! The upgrade is a route of the authenticated router (Basic or Bearer on
//! the handshake, as every route), under the listener's connection bounds;
//! then the socket has its own: a page of another host is refused unless
//! `api.origin` says otherwise, sockets are counted in all and per address,
//! messages are capped in size and in rate, and a socket that answers no ping
//! is closed ([`WsLimits`]). The WebRTC session an offer makes is the one
//! `POST /api/webrtc` makes ([`crate::WebRtcAnswerer`]), and it outlives its
//! socket, as go2rtc's does: go2rtc's player closes the socket once the
//! picture plays.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use oxrtc_core::{CancellationToken, PerIpLimit, PerIpPermit, Refusals};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::ApiState;
use crate::listener::PeerAddr;
use crate::routes::{Query, Reply, text};

mod conn;
pub mod message;

/// The bounds of `/api/ws`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WsLimits {
    /// Sockets open at once, every address together: half the listener's
    /// connections, so that sockets never take the slots of plain requests.
    pub max_sockets: usize,
    /// Sockets one address keeps open at once (an IPv6 peer by its `/64`):
    /// twice a caller's WebRTC sessions, Home Assistant holding one socket per
    /// viewer of its dashboards from its one address.
    pub max_sockets_per_ip: usize,
    /// Largest message (and frame) read: an SDP offer with its candidates.
    pub max_message_bytes: usize,
    /// Messages one socket may send in a second; one more closes it.
    pub max_messages_per_second: u32,
    /// Candidates kept until the session they belong to is answered.
    pub max_early_candidates: usize,
    /// WebRTC sessions one socket may ask for in its life.
    pub max_offers: usize,
    /// A ping is sent after this long without one.
    pub ping_every: Duration,
    /// A socket from which nothing (a pong included) arrived for this long
    /// is closed.
    pub idle_timeout: Duration,
    /// A message that cannot be written within this long closes the socket
    /// (go2rtc's 5 s write deadline).
    pub write_timeout: Duration,
}

/// Default [`WsLimits::max_sockets`].
pub const MAX_SOCKETS: usize = crate::MAX_CONNECTIONS / 2;
/// Default [`WsLimits::max_sockets_per_ip`].
pub const MAX_SOCKETS_PER_IP: usize = 64;
/// Default [`WsLimits::max_message_bytes`]: a request body's bound.
pub const MAX_MESSAGE_BYTES: usize = crate::MAX_BODY_BYTES;

impl Default for WsLimits {
    fn default() -> Self {
        Self {
            max_sockets: MAX_SOCKETS,
            max_sockets_per_ip: MAX_SOCKETS_PER_IP,
            max_message_bytes: MAX_MESSAGE_BYTES,
            max_messages_per_second: 50,
            max_early_candidates: 64,
            max_offers: 4,
            ping_every: Duration::from_secs(20),
            idle_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(5),
        }
    }
}

/// Which pages may open a socket (go2rtc's `api.origin`). A request with no
/// `Origin` is not a browser page's, and is always taken: what protects the
/// API from other programs is its credential.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum OriginPolicy {
    /// A page of the request's own host, whatever its port (go2rtc's
    /// default: a proxy or Home Assistant's add-on serves it on another).
    #[default]
    SameHost,
    /// Any page (`api.origin: "*"`).
    Any,
    /// A page whose host and port are the request's `Host` (any other value
    /// of `api.origin`, as go2rtc then leaves the check to its library).
    SameHostAndPort,
}

impl OriginPolicy {
    /// `api.origin`.
    pub fn from_config(origin: Option<&str>) -> Self {
        match origin.map(str::trim) {
            None | Some("") => Self::SameHost,
            Some("*") => Self::Any,
            Some(_) => Self::SameHostAndPort,
        }
    }

    /// Whether a handshake with these headers comes from an allowed page.
    pub fn allows(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = headers.get(header::ORIGIN) else { return true };
        if *self == Self::Any {
            return true;
        }
        let (Ok(origin), Some(Ok(host))) = (origin.to_str(), headers.get(header::HOST).map(|h| h.to_str())) else {
            return false;
        };
        // `scheme://host[:port]`, nothing after it (RFC 6454).
        let Some((_, authority)) = origin.split_once("://") else { return false };
        if authority.contains(['/', '@', '?', '#']) {
            return false;
        }
        if authority.eq_ignore_ascii_case(host) {
            return true;
        }
        *self == Self::SameHost && without_port(authority).eq_ignore_ascii_case(without_port(host))
    }
}

/// `host[:port]` or `[v6][:port]` without its port.
fn without_port(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority.split_once(']').map_or(authority, |(v6, _)| v6.trim_start_matches('['));
    }
    authority.rsplit_once(':').map_or(authority, |(host, _)| host)
}

/// The sockets open, in all and per address.
pub(crate) struct Sockets {
    total: Arc<Semaphore>,
    max: usize,
    per_ip: PerIpLimit,
    full: Refusals,
}

/// A socket's two slots, held as long as it is open.
pub(crate) struct SocketSlot {
    _total: OwnedSemaphorePermit,
    _address: PerIpPermit,
}

impl Sockets {
    pub(crate) fn new(limits: &WsLimits) -> Self {
        let max = limits.max_sockets.max(1);
        Self {
            total: Arc::new(Semaphore::new(max)),
            max,
            per_ip: PerIpLimit::new(limits.max_sockets_per_ip),
            full: Refusals::new(),
        }
    }

    fn try_open(&self, remote: Option<SocketAddr>) -> Option<SocketSlot> {
        let Ok(total) = self.total.clone().try_acquire_owned() else {
            if let Some(refused) = self.full.refused() {
                tracing::warn!(limit = self.max, refused, "api: websocket limit reached, refusing");
            }
            return None;
        };
        // An address the listener did not give (a test's router) counts as
        // the unspecified one.
        let ip = remote.map_or(std::net::IpAddr::from([0, 0, 0, 0]), |r| r.ip());
        let Some(address) = self.per_ip.try_acquire(ip) else {
            if let Some(refused) = self.per_ip.refused() {
                tracing::warn!(
                    limit = self.per_ip.max_per_ip(),
                    refused,
                    "api: per-address websocket limit reached, refusing"
                );
            }
            return None;
        };
        Some(SocketSlot { _total: total, _address: address })
    }
}

/// What the listener hands every request: the server's own stop, which ends
/// every socket (a socket is not a request, and the listener's graceful stop
/// does not wait for it).
#[derive(Debug, Clone)]
pub(crate) struct Shutdown(pub(crate) CancellationToken);

/// `GET /api/ws`.
pub(crate) async fn upgrade(State(state): State<ApiState>, request: Request) -> Reply {
    let (mut parts, _body) = request.into_parts();
    if !state.ws_origin.allows(&parts.headers) {
        tracing::debug!("api: websocket from a page of another host refused");
        return Err(text(StatusCode::FORBIDDEN, "origin not allowed"));
    }
    let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &state)
        .await
        .map_err(|rejection| text(rejection.status(), rejection.body_text()))?;
    let query = Query::of(&parts.uri)?;
    let peer = parts.extensions.get::<ConnectInfo<PeerAddr>>().map(|c| c.0.clone());
    let remote = peer.as_ref().map(|p| p.0).or_else(|| parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0));
    let slot =
        state.sockets.try_open(remote).ok_or_else(|| text(StatusCode::SERVICE_UNAVAILABLE, "too many websockets"))?;
    let shutdown = parts.extensions.get::<Shutdown>().map_or_else(CancellationToken::new, |s| s.0.clone());
    let user_agent = parts.headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let context = conn::Context {
        state: state.clone(),
        src: query.first("src").map(str::to_owned),
        remote,
        user_agent,
        limits: state.ws_limits,
        shutdown,
        _slot: slot,
    };
    // From here the connection carries frames, not requests: the listener's
    // clock for a request's head must not run on them.
    if let Some(peer) = &peer {
        peer.upgraded();
    }
    let limits = state.ws_limits;
    Ok(upgrade
        .max_message_size(limits.max_message_bytes)
        .max_frame_size(limits.max_message_bytes)
        .write_buffer_size(0)
        .max_write_buffer_size(limits.max_message_bytes.saturating_mul(4))
        .on_failed_upgrade(|error| tracing::debug!("api: websocket upgrade failed: {error}"))
        .on_upgrade(move |socket| conn::run(socket, context))
        .into_response())
}

#[cfg(test)]
#[path = "origin_tests.rs"]
mod origin_tests;
