//! Read-only views of the hub, for an API to serialize.

use std::time::Duration;

use crate::media::TrackInfo;

/// Where a source stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceState {
    /// Not running: nobody needs it.
    Idle,
    /// Connecting (attempt 0 is the first).
    Connecting {
        /// Failed attempts so far.
        attempt: u32,
    },
    /// Delivering media.
    Running,
    /// Waiting before the next attempt.
    Retrying {
        /// Failed attempts so far.
        attempt: u32,
        /// Why the last one failed.
        error: String,
    },
    /// Stopped for a reason retrying cannot fix.
    Failed {
        /// The reason.
        error: String,
    },
}

impl SourceState {
    /// A one-word name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Connecting { .. } => "connecting",
            Self::Running => "running",
            Self::Retrying { .. } => "retrying",
            Self::Failed { .. } => "failed",
        }
    }
}

/// One stream.
#[derive(Debug, Clone)]
pub struct StreamSnapshot {
    /// Its name.
    pub name: String,
    /// Its sources, in order.
    pub sources: Vec<SourceSnapshot>,
    /// Its consumers.
    pub consumers: Vec<ConsumerSnapshot>,
}

/// One source of a stream.
#[derive(Debug, Clone)]
pub struct SourceSnapshot {
    /// The source, redacted.
    pub url: String,
    /// Its state.
    pub state: SourceState,
    /// Its tracks on the current connection.
    pub tracks: Vec<TrackInfo>,
    /// Frames received per track on the current connection, same order as `tracks`.
    pub track_frames: Vec<u64>,
    /// Frames published since the stream was created.
    pub frames: u64,
    /// Bytes published since the stream was created.
    pub bytes: u64,
    /// Frames refused as malformed or oversized.
    pub rejected: u64,
    /// Consumers and probes holding it.
    pub demand: usize,
    /// Failed attempts since the stream was created.
    pub failures: u64,
    /// Why the last attempt failed, until a run delivered media again.
    pub last_error: Option<String>,
    /// How long ago its last frame arrived, on the current connection.
    pub last_frame_age: Option<Duration>,
}

/// One consumer of a stream.
#[derive(Debug, Clone)]
pub struct ConsumerSnapshot {
    /// Unique within the stream.
    pub id: u64,
    /// `webrtc`, `rtsp`, `preload`, `internal`...
    pub kind: String,
    /// The peer's address, when the protocol knows it.
    pub remote: Option<String>,
    /// Frames delivered.
    pub frames: u64,
    /// Bytes delivered.
    pub bytes: u64,
    /// Frames lost because the consumer lagged.
    pub dropped: u64,
}
