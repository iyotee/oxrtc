//! What the API delegates to the protocol crates, through traits the binary
//! implements (the API depends on neither WebRTC nor FFmpeg).

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use oxrtc_core::{BoxFuture, Stream};
use thiserror::Error;

use crate::config::IceServer;

/// What a replacement of the ICE servers kept of what it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IceSummary {
    /// TURN servers in use.
    pub turn: usize,
    /// STUN servers in use.
    pub stun: usize,
}

/// The WebRTC egress behind `/api/webrtc`: answers a WHEP offer for a stream,
/// and takes new ICE servers while it runs.
pub trait WebRtcAnswerer: Send + Sync + 'static {
    /// Replaces `webrtc.ice_servers` for the sessions made from now on,
    /// without restarting anything or cutting a session (a TURN credential
    /// that rotates). A URL that cannot be used is left out and warned about.
    fn set_ice_servers(&self, servers: Vec<IceServer>) -> IceSummary;

    /// The complete SDP answer (no trickle) to `offer_sdp`, from a viewer at
    /// `remote` when known, whose request said `user_agent` (the session is
    /// listed by it, as go2rtc does).
    fn answer(
        &self,
        stream: Arc<Stream>,
        offer_sdp: String,
        remote: Option<SocketAddr>,
        user_agent: Option<String>,
    ) -> BoxFuture<'static, Result<String, AnswerError>>;
}

/// Why no answer was produced.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AnswerError {
    /// The offer cannot be parsed or used (`400`).
    #[error("bad offer: {0}")]
    BadOffer(String),
    /// The offer shares no codec with the stream (`400`).
    #[error("no matching codec")]
    NoMatchingCodec,
    /// No source of the stream could be opened (`500 streams: ...`).
    #[error("{0}")]
    Unavailable(String),
    /// Too many sessions (`503`).
    #[error("too many sessions")]
    Busy,
    /// Anything else (`500`).
    #[error("internal error: {0}")]
    Internal(String),
}

/// Produces a still picture of a stream.
pub trait Snapshotter: Send + Sync + 'static {
    /// A JPEG of the stream's current picture, scaled to `width` when given.
    fn jpeg(&self, stream: Arc<Stream>, width: Option<u32>) -> BoxFuture<'static, Result<Bytes, SnapshotError>>;
}

/// Why no picture was produced.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SnapshotError {
    /// The stream has no picture to give (`500`).
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Anything else (`500`).
    #[error("internal error: {0}")]
    Internal(String),
}
