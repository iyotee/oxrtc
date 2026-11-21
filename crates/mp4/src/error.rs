//! What the writer refuses.

/// Why a track or a frame cannot be written. Nothing is written for a
/// refused frame: the writer's state is the same as before the call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Mp4Error {
    /// The codec has no sample entry here (Motion JPEG), or this variant of it
    /// cannot be carried (Opus with more than two channels).
    #[error("{0} cannot be carried in fragmented MP4 here")]
    Unsupported(&'static str),
    /// The track's codec configuration is missing: the parameter sets of a
    /// video track, the `AudioSpecificConfig` of AAC.
    #[error("the {0} configuration is missing")]
    MissingConfig(&'static str),
    /// A parameter set or a configuration that cannot be read, or that the
    /// box format cannot hold.
    #[error("invalid {what}: {why}")]
    Invalid {
        /// What was being read.
        what: &'static str,
        /// What was wrong with it.
        why: &'static str,
    },
    /// No track of this index.
    #[error("no track {0}")]
    NoTrack(usize),
    /// More tracks than [`crate::MAX_TRACKS`], or none.
    #[error("a writer holds 1 to {max} tracks, not {count}", max = crate::MAX_TRACKS)]
    TrackCount {
        /// The tracks asked for.
        count: usize,
    },
    /// A frame larger than [`crate::MAX_SAMPLE_BYTES`].
    #[error("a frame of {0} bytes is over the sample limit")]
    TooLarge(usize),
    /// A frame its track cannot carry: malformed length prefixes, an Opus
    /// packet with no valid table of contents, PCM that is not whole samples.
    #[error("a frame its track cannot carry: {0}")]
    BadFrame(&'static str),
    /// A time that does not fit its field: a negative decode time, a
    /// composition offset beyond 32 bits.
    #[error("a time out of range: {0}")]
    Time(&'static str),
}

impl Mp4Error {
    pub(crate) const fn invalid(what: &'static str, why: &'static str) -> Self {
        Self::Invalid { what, why }
    }
}
