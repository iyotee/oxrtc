//! Every bound the core enforces, in one place.

use std::time::Duration;

use crate::queue::{ConsumerClass, QueueBudget};

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
    /// What a live consumer's queue holds (a WebRTC viewer, a still): beyond
    /// it, frames are dropped and video resumes at a keyframe
    /// ([`crate::queue`]).
    pub live_queue: QueueBudget,
    /// What a recorder's queue holds (an RTSP reader): deep enough to ride
    /// out a reader's pause of several seconds without a hole.
    pub recorder_queue: QueueBudget,
    /// Bytes of the current group of pictures kept so that a new consumer
    /// starts at once on the last keyframe. 0 disables the cache.
    pub gop_cache_bytes: usize,
    /// Frames held for a subscription while its tracks are being resolved.
    pub max_backlog_frames: usize,
    /// How long, from the subscribe call, a subscription waits for its
    /// sources' tracks before it goes on with those it has. Keep it under the
    /// egress's own answer deadline, or that one fires first.
    pub probe_timeout: Duration,
    /// How much longer, in all from the subscribe call, a subscription with
    /// no track yet waits while a source of it is still starting (none has
    /// failed): a transcode of a one-picture-a-second webcam is FFmpeg
    /// probing for 20 s, and go2rtc waits that out. A source that fails is
    /// given up at once, so a camera that is down is not waited for. Keep it
    /// under the egress's own answer deadline (WHEP's, DESCRIBE's: 29 s;
    /// openIVR gives a WHEP offer 30 s).
    pub first_track_timeout: Duration,
    /// A connection that produces no frame within this is restarted.
    pub connect_timeout: Duration,
    /// A running source silent for this long is restarted.
    pub stall_timeout: Duration,
    /// The shortest silence that restarts a source judged by its own cadence
    /// ([`crate::cadence`]): a source that has shown its rhythm is restarted
    /// after [`crate::cadence::SILENCE_FACTOR`] times its longest gap, never
    /// sooner than this, never later than its stall timeout. A source that
    /// reads this server is not judged so: its silence is its input's.
    pub silence_floor: Duration,
    /// A source with no consumer left is stopped after this.
    pub idle_grace: Duration,
    /// First reconnection delay.
    pub backoff_initial: Duration,
    /// Longest reconnection delay (jitter included), so also how late a
    /// camera that comes back can be noticed.
    pub backoff_max: Duration,
    /// A connection that delivered media for this long resets the backoff.
    pub healthy_after: Duration,
    /// The delay between attempts at a source that dropped after it delivered,
    /// while each attempt fails at once ([`crate::SourceDriver::cheap_attempts`],
    /// in under [`crate::Limits::cheap_attempt`]) and for at most
    /// `quick_retry_for` after it dropped: a camera back from an outage is
    /// read within this of its return, where the backoff let it wait up to
    /// `backoff_max`. Refused credentials and a declined request keep their
    /// own backoffs.
    pub quick_retry: Duration,
    /// How long after a source dropped the short cadence lasts.
    pub quick_retry_for: Duration,
    /// An attempt that failed in less than this cost nothing but a refusal.
    pub cheap_attempt: Duration,
    /// First delay after refused credentials, and its ceiling: a camera that
    /// counts failed logins locks the account (for every client) when a
    /// changed password is retried every `backoff_max`.
    pub auth_backoff_initial: Duration,
    /// See `auth_backoff_initial`.
    pub auth_backoff_max: Duration,
    /// First delay after a run the peer declined (`SourceError::Declined`)
    /// when the source has delivered nothing since its demand began, and its
    /// ceiling. The sound transcode of a webcam with no microphone is declined
    /// by its input at every run, for as long as a viewer asks for its sound:
    /// at `backoff_max` that was two FFmpeg runs every 5 s per camera, for
    /// the whole viewing. A source that delivered keeps the fast backoff, so a
    /// camera back from an outage is noticed as quickly as before.
    pub declined_backoff_initial: Duration,
    /// See `declined_backoff_initial`.
    pub declined_backoff_max: Duration,
}

impl Limits {
    /// The queue budget of a consumer of `class`.
    pub fn queue_budget(&self, class: ConsumerClass) -> QueueBudget {
        match class {
            ConsumerClass::Live => self.live_queue,
            ConsumerClass::Recorder => self.recorder_queue,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_streams: 512,
            max_sources_per_stream: 8,
            max_tracks_per_source: 8,
            max_consumers_per_stream: 64,
            max_frame_bytes: 8 * 1024 * 1024,
            // 2 s: a viewer further behind is better served by the latest
            // keyframe. 8 MiB holds those 2 s of a 4K camera with room.
            live_queue: QueueBudget { max_bytes: 8 * 1024 * 1024, max_age: Duration::from_secs(2) },
            // 10 s: a recorder paused that long (a segmenter rotating its
            // file, a reader descheduled on a loaded Pi) loses nothing. 32 MiB
            // is 10 s at about 26 Mbit/s; the old ring of 256 items was about
            // 4 s of a camera's picture and its Opus sound.
            recorder_queue: QueueBudget { max_bytes: 32 * 1024 * 1024, max_age: Duration::from_secs(10) },
            gop_cache_bytes: 8 * 1024 * 1024,
            max_backlog_frames: 1024,
            // 10 s equalled WHEP's answer deadline, which starts first and so
            // always fired first: a missing sound track failed the whole answer.
            probe_timeout: Duration::from_secs(8),
            // go2rtc answered the owner's 1 picture-a-second webcam (FFmpeg
            // probes 20 pictures of it) after 28.6 s; openIVR waits 30 s.
            first_track_timeout: Duration::from_secs(28),
            connect_timeout: Duration::from_secs(15),
            stall_timeout: Duration::from_secs(15),
            // The owner's cameras showed 1.9 s at most between two pictures
            // in 27 five-minute runs (the XM cameras at 14 a second, the
            // HTTPS webcam at one a second: 2.0 s, a picture it skipped);
            // a camera whose rate drops to one a second at night stays well
            // within it (D34).
            silence_floor: Duration::from_secs(5),
            idle_grace: Duration::from_secs(5),
            backoff_initial: Duration::from_millis(500),
            // 30 s made a camera back from a 60 s outage wait 18 s for its
            // next attempt (the bench's R01: 13 s mean recovery, go2rtc 5.5 s),
            // and a NVR records nothing meanwhile. Retrying a camera that is
            // down every 5 s costs it one refused connection.
            backoff_max: Duration::from_secs(5),
            healthy_after: Duration::from_secs(10),
            // Two refused connections a second for two minutes - a camera's
            // or a switch's reboot - then the backoff (D34).
            quick_retry: Duration::from_millis(500),
            quick_retry_for: Duration::from_secs(120),
            cheap_attempt: Duration::from_secs(2),
            auth_backoff_initial: Duration::from_secs(30),
            auth_backoff_max: Duration::from_secs(600),
            declined_backoff_initial: Duration::from_secs(5),
            declined_backoff_max: Duration::from_secs(60),
        }
    }
}
