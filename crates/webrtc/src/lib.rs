//! WHEP egress of oxrtc.
//!
//! A [`WebRtcServer`] owns **one** UDP socket shared by every WebRTC session.
//! [`WebRtcServer::answer`] turns a browser's SDP offer into a complete SDP
//! answer (every candidate included, no trickle): the codecs the offer lists
//! decide what the session subscribes to (H.265 first when offered, so a
//! browser that decodes H.265 gets the camera's own stream), and the session
//! then forwards the subscription's frames through [`str0m`], which does ICE,
//! DTLS, SRTP and RTP packetization.
//!
//! The candidates: host ones on the shared socket, ICE-TCP passive ones on a
//! TCP listener at its port, and from [`WebRtcConfig::ice_servers`] a
//! server-reflexive one per STUN server and a relayed one per TURN server,
//! through this crate's own TURN client (D13). ICE-lite with host candidates
//! only, full ICE with any other.
//!
//! Everything is bounded: the number of sessions (in all and per caller
//! address), the addresses a session is known by, the frames held while a
//! session connects, and the frames handed to str0m (when it cannot take
//! more, video is dropped until the next keyframe and the loss is counted).
#![forbid(unsafe_code)]

mod candidates;
mod hevc;
mod mismatch;
mod msid;
mod negotiate;
mod offer;
mod pace;
mod payload;
mod registry;
mod replay;
mod resource;
mod server;
mod session;
mod socket;
mod stun;
mod tcp;
mod turn;
mod unreached;

/// Harnesses for the fuzz targets (`fuzz/`) and their stable property tests.
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzzing;

use std::net::SocketAddr;
use std::time::Duration;

pub use hevc::HevcPtl;
pub use offer::{MAX_NAMED, MAX_OFFER_BYTES, OfferedCodecs};
pub use resource::{Answered, MAX_CANDIDATES_PER_PATCH, MAX_TRICKLED_CANDIDATES, ResourceError};
pub use server::WebRtcServer;
pub use turn::servers::{IceServer, IceSummary};

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
    /// `webrtc.ice_servers`: STUN servers give a server-reflexive candidate,
    /// TURN servers (UDP) a relayed one, for viewers outside the LAN.
    pub ice_servers: Vec<IceServer>,
    /// How long a TURN allocation whose credential was replaced
    /// ([`WebRtcServer::set_ice_servers`]) is kept at least, for the sessions
    /// offered its address to connect; after that it ends once no session
    /// uses it. At least the session idle timeout, or a session that never
    /// connected loses it before it is abandoned.
    pub ice_retire_grace: Duration,
    /// ICE-TCP passive candidates on a TCP listener at the UDP port (go2rtc
    /// listens on both).
    pub tcp: bool,
    /// How the sound and the picture are sent to be played together.
    pub playout: Playout,
}

/// How a session's sound and picture are sent to be played together (D30).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Playout {
    /// The sound leaves on its stream's own clock, evenly spaced, instead of
    /// as the source delivers it. Off by default (D30, W1b review): on L01 it
    /// moved the sound's wait from the browser's buffer to the server (the
    /// sound reached the ear no sooner), the picture was shown earlier only
    /// because the sound was dated when it left, and the server's CPU went
    /// from 0.58 to 1.58 % of a core per viewer, which lost L01's CPU row.
    pub pace_sound: bool,
    /// Sound further behind its stream's clock than this is not sent (the
    /// sound a stalled source delivers at once); `None`: all of it is.
    pub sound_late_max: Option<Duration>,
    /// `true`: the browser holds the picture for the sound (one stream, lip
    /// sync), as go2rtc and MediaMTX. `false`: the picture carries the
    /// `playout-delay` header extension at 0 (shown as soon as decoded), and
    /// the sound trails it by the browser's sound buffer.
    pub lip_sync: bool,
    /// With lip sync on, how much later than the picture that arrived with
    /// it the sound is dated in the sender reports: the browser plays the
    /// sound that far behind the picture, and shows the picture it holds for
    /// the sound that much sooner. At most [`MAX_SOUND_LAG`]; zero: both are
    /// dated when they arrived. Nothing with lip sync off, or for a session
    /// that sends no sound.
    pub sound_lag: Duration,
}

/// Default [`Playout::sound_late_max`] (D30, roadmap W1b).
pub const DEFAULT_SOUND_LATE_MAX: Duration = Duration::from_millis(400);

/// Default [`Playout::sound_lag`] (D30, roadmap W1c). The owner's answer of
/// 2026-11-19 allows 20 to 40 ms, inside the 45 ms ATSC IS-191 gives a sound
/// behind its picture; a browser leaves the sound about 10 ms behind the
/// picture by itself (it stops adjusting within 30 ms of its target), and
/// the lag adds to that. Measured in Edge on the bench's camera (L01), by the
/// beep heard against the flash seen and on the mean of a session's runs: 20
/// ms put the sound 15 to 23 ms further behind than 0 (some 23 to 31 in all,
/// 7 to 14 more than MediaMTX's), 25 ms 36 further (about 44: the edge of
/// the 45), 30 ms 54. One run reads up to 30 ms from its session's mean, so
/// the choice is made on the means: no value keeps every single run inside.
pub const DEFAULT_SOUND_LAG: Duration = Duration::from_millis(20);

/// The longest [`Playout::sound_lag`]: the most the owner's answer allows. A
/// session is never given more, whatever it is told.
pub const MAX_SOUND_LAG: Duration = Duration::from_millis(40);

impl Default for Playout {
    fn default() -> Self {
        Self {
            pace_sound: false,
            sound_late_max: Some(DEFAULT_SOUND_LATE_MAX),
            lip_sync: true,
            sound_lag: DEFAULT_SOUND_LAG,
        }
    }
}

impl Default for WebRtcConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 8555)),
            candidates: Vec::new(),
            max_sessions: DEFAULT_MAX_SESSIONS,
            max_sessions_per_ip: DEFAULT_MAX_SESSIONS_PER_IP,
            session_idle_timeout: Duration::from_secs(30),
            answer_timeout: Duration::from_secs(29),
            ice_servers: Vec::new(),
            ice_retire_grace: Duration::from_secs(60),
            tcp: true,
            playout: Playout::default(),
        }
    }
}

/// Why an offer could not be answered.
#[derive(Debug, thiserror::Error)]
pub enum WhepError {
    /// The offer is not a usable SDP offer.
    #[error("bad offer: {0}")]
    BadOffer(String),
    /// The offer lists no codec this server sends, the stream has none of
    /// the codecs it lists, or its video lists none the stream's picture can
    /// be sent in (D43). The reason names both sides, as go2rtc's matcher
    /// says it: `streams: codecs not matched: video:H264, audio:OPUS =>
    /// video:VP8, audio:OPUS` (the stream's codecs, then the offer's).
    #[error("{0}")]
    NoMatchingCodec(String),
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
