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
//! picture plays. `mse` streams fragmented MP4 over the socket itself
//! ([`mse`], F3), and ends with it.

use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use oxrtc_core::CancellationToken;

use crate::ApiState;
use crate::listener::PeerAddr;
use crate::routes::{Query, Reply, text};
use crate::slots::Slots;

mod conn;
pub mod message;
pub mod mse;

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
    /// Longest candidate line taken (a browser's longest, an IPv6 one with
    /// Chrome's extensions, is under 200 bytes): what bounds the candidates
    /// held before an answer to 64 KiB a socket, not 64 messages' worth.
    pub max_candidate_bytes: usize,
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
            max_candidate_bytes: 1024,
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
    /// A page of the request's own `Host`; when that `Host` names no port
    /// (a proxy passing its host name on), a page of that host on any port
    /// (go2rtc's default, measured on 1.9.15).
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
        // go2rtc's default leaves the page's port out only when the
        // request's `Host` names none: a proxy that passes its own host name
        // on. A `Host` with a port is that server's origin, and a page of
        // another port is another program's.
        *self == Self::SameHost && without_port(host) == host && without_port(authority).eq_ignore_ascii_case(host)
    }
}

/// Longest `Origin` or `Host` a refusal quotes, in characters.
const MAX_QUOTED: usize = 128;

/// Why a handshake was refused for its page, in words an operator acts on
/// from the page's network panel or the log alone (D39, confirmed by the
/// owner 2026-11-23): the page's origin and the `Host` the request reached
/// with (the requester's own headers, printable ASCII, cut), and the two
/// ways out. go2rtc answers a bare `403 Forbidden`.
pub(crate) fn origin_refusal(headers: &HeaderMap) -> String {
    let quote = |name| {
        let value = headers.get(name).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).unwrap_or_default();
        let clean: String = value.chars().filter(|c| c.is_ascii_graphic()).take(MAX_QUOTED).collect();
        if clean.is_empty() { "(none)".to_owned() } else { clean }
    };
    format!(
        "origin not allowed: a page of {} may not open a websocket of host {} (api.origin); a reverse proxy \
         passes the browser's Host header on (nginx: proxy_set_header Host $http_host; Caddy does by default), \
         or api.origin: \"*\" lets every page in",
        quote(header::ORIGIN),
        quote(header::HOST)
    )
}

/// `host[:port]` or `[v6][:port]` without its port (the brackets kept).
fn without_port(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority.find(']').and_then(|end| authority.get(..=end)).unwrap_or(authority);
    }
    authority.rsplit_once(':').map_or(authority, |(host, _)| host)
}

/// The most a socket's writer holds: one message, written whole before the
/// next (every send waits for its flush). A message is a signalling one or an
/// MSE fragment, which is one frame of the core's (`max_frame_bytes`) at
/// most, a G.711 sound as FLAC twice its size, and its boxes: so the bound
/// lets a fragment through rather than limiting what is buffered, which the
/// one-message rule does.
pub(crate) fn max_write_buffer(limits: &WsLimits, max_frame_bytes: usize) -> usize {
    let fragment = max_frame_bytes.saturating_mul(2).saturating_add(64 * 1024);
    limits.max_message_bytes.saturating_mul(4).max(fragment)
}

/// The sockets open, in all and per address.
pub(crate) fn sockets(limits: &WsLimits) -> Slots {
    Slots::new("websocket", limits.max_sockets, limits.max_sockets_per_ip)
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
        let refusal = origin_refusal(&parts.headers);
        if let Some(refused) = state.ws_refused_origins.refused() {
            tracing::warn!(refused, "api: websocket refused: {refusal}");
        }
        return Err(text(StatusCode::FORBIDDEN, refusal));
    }
    let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &state)
        .await
        .map_err(|rejection| text(rejection.status(), rejection.body_text()))?;
    let query = Query::of(&parts.uri)?;
    // oxrtc's `start` for an `mse` of this socket (`cache` or `next`).
    let start = oxrtc_core::Start::from_query(parts.uri.query().unwrap_or_default())
        .map_err(|e| text(StatusCode::BAD_REQUEST, e))?
        .unwrap_or_default();
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
        start,
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
        .max_write_buffer_size(max_write_buffer(&limits, state.hub.limits().max_frame_bytes))
        .on_failed_upgrade(|error| tracing::debug!("api: websocket upgrade failed: {error}"))
        .on_upgrade(move |socket| conn::run(socket, context))
        .into_response())
}

#[cfg(test)]
#[path = "origin_tests.rs"]
mod origin_tests;
