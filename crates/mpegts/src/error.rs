//! Why a track or a frame is refused.

/// A refusal of the writer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TsError {
    /// No track, or more than [`crate::MAX_TRACKS`].
    #[error("{count} tracks: a writer takes 1 to 8")]
    TrackCount {
        /// The tracks given.
        count: usize,
    },
    /// No track of this index.
    #[error("no track {0}")]
    NoTrack(usize),
    /// A track MPEG-TS carries but not like this.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// A frame that cannot be what its track says.
    #[error("bad frame: {0}")]
    BadFrame(&'static str),
    /// A frame over [`crate::MAX_FRAME_BYTES`], or over what its header can say.
    #[error("a frame of {0} bytes is too large")]
    TooLarge(usize),
    /// A time that cannot be written.
    #[error("time: {0}")]
    Time(&'static str),
}
