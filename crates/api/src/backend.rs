//! What the API delegates to the protocol crates, through traits the binary
//! implements (the API depends on neither WebRTC nor FFmpeg).

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use oxrtc_core::{BoxFuture, Stream};
use thiserror::Error;

/// Answers a WHEP offer for a stream.
pub trait WebRtcAnswerer: Send + Sync + 'static {
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
