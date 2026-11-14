//! WHEP egress of oxrtc.
//!
//! A [`WebRtcServer`] owns **one** UDP socket shared by every WebRTC session.
//! [`WebRtcServer::answer`] turns a browser's SDP offer into a complete SDP
//! answer (host candidates included, no trickle): the codecs the offer lists
//! decide what the session subscribes to (H.265 first when offered, so a
//! browser that decodes H.265 gets the camera's own stream), and the session
//! then forwards the subscription's frames through [`str0m`], which does ICE
//! (lite), DTLS, SRTP and RTP packetization.
//!
//! Everything is bounded: the number of sessions (in all and per caller
//! address), the addresses a session is known by, the frames held while a
//! session connects, and the frames handed to str0m (when it cannot take
//! more, video is dropped until the next keyframe and the loss is counted).
#![forbid(unsafe_code)]

mod candidates;
mod negotiate;
mod offer;
mod payload;
mod server;
mod session;

use std::net::SocketAddr;
use std::time::Duration;

pub use offer::{MAX_OFFER_BYTES, OfferedCodecs};
pub use server::WebRtcServer;

/// Default [`WebRtcConfig::max_sessions`]: WebRTC sessions at once, from every
/// caller together.
pub const DEFAULT_MAX_SESSIONS: usize = 64;

/// Default [`WebRtcConfig::max_sessions_per_ip`]: half the default total, so
/// that one address never takes every session. Behind a proxy (openIVR
/// forwards every viewer's offer from its backend) the proxy's address holds
/// them all: raise it there.
pub const DEFAULT_MAX_SESSIONS_PER_IP: usize = 32;

/// Configuration of the WebRTC egress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebRtcConfig {
    /// The UDP address every session shares, e.g. `0.0.0.0:8555`.
    pub listen: SocketAddr,
    /// Extra host candidates, as `"ip"` (the listen port is used) or
    /// `"ip:port"` (a port forwarded by a NAT).
    pub candidates: Vec<String>,
    /// Sessions at once, negotiating ones included.
    pub max_sessions: usize,
    /// Sessions at once requested from one address (the WHEP caller's; an
    /// IPv6 caller counts for its `/64`; loopback is not exempt).
    pub max_sessions_per_ip: usize,
    /// A session that receives nothing from its peer for this long ends.
    pub session_idle_timeout: Duration,
    /// How long [`WebRtcServer::answer`] waits for the stream's tracks.
    pub answer_timeout: Duration,
}

impl Default for WebRtcConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 8555)),
            candidates: Vec::new(),
            max_sessions: DEFAULT_MAX_SESSIONS,
            max_sessions_per_ip: DEFAULT_MAX_SESSIONS_PER_IP,
            session_idle_timeout: Duration::from_secs(30),
            answer_timeout: Duration::from_secs(10),
        }
    }
}

/// Why an offer could not be answered.
#[derive(Debug, thiserror::Error)]
pub enum WhepError {
    /// The offer is not a usable SDP offer.
    #[error("bad offer: {0}")]
    BadOffer(String),
    /// The offer lists no codec this server sends, or the stream has none of
    /// the codecs it lists.
    #[error("no matching codec")]
    NoMatchingCodec,
    /// The stream's sources did not deliver tracks in time.
    #[error("source unavailable: {0}")]
    SourceUnavailable(String),
    /// [`WebRtcConfig::max_sessions`] reached, or the stream's own consumer
    /// cap (`Limits::max_consumers_per_stream` in the core).
    #[error("too many sessions")]
    TooManySessions,
    /// Anything else (the server is shutting down...).
    #[error("internal error: {0}")]
    Internal(String),
}
