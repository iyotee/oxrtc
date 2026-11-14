//! Every bound the core enforces, in one place.

use std::time::Duration;

/// Resource limits and timings. The defaults suit a NVR of a few dozen
/// cameras on a Raspberry Pi 5; each one is a ceiling, not a reservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Streams in a hub.
    pub max_streams: usize,
    /// Sources in one stream.
    pub max_sources_per_stream: usize,
    /// Tracks in one source.
    pub max_tracks_per_source: usize,
    /// Consumers of one stream (viewers, relays, preloads).
    pub max_consumers_per_stream: usize,
    /// Largest frame accepted from a source.
    pub max_frame_bytes: usize,
    /// Items kept per source for consumers that lag. A consumer further behind
    /// than this loses frames and resumes at the next keyframe.
    pub ring_capacity: usize,
    /// Bytes of the current group of pictures kept so that a new consumer
    /// starts at once on the last keyframe. 0 disables the cache.
    pub gop_cache_bytes: usize,
    /// Frames held for a subscription while its tracks are being resolved.
    pub max_backlog_frames: usize,
    /// How long a subscription waits for a source whose codecs are unknown.
    pub probe_timeout: Duration,
    /// A connection that produces no frame within this is restarted.
    pub connect_timeout: Duration,
    /// A running source silent for this long is restarted.
    pub stall_timeout: Duration,
    /// A source with no consumer left is stopped after this.
    pub idle_grace: Duration,
    /// First reconnection delay.
    pub backoff_initial: Duration,
    /// Longest reconnection delay.
    pub backoff_max: Duration,
    /// A connection that delivered media for this long resets the backoff.
    pub healthy_after: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_streams: 512,
            max_sources_per_stream: 8,
            max_tracks_per_source: 8,
            max_consumers_per_stream: 64,
            max_frame_bytes: 8 * 1024 * 1024,
            ring_capacity: 256,
            gop_cache_bytes: 8 * 1024 * 1024,
            max_backlog_frames: 1024,
            probe_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(15),
            stall_timeout: Duration::from_secs(15),
            idle_grace: Duration::from_secs(5),
            backoff_initial: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
            healthy_after: Duration::from_secs(10),
        }
    }
}
