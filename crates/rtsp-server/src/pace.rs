//! The pace of the pictures a `DESCRIBE` announces (`a=framerate`), and the
//! one reader whose answer waits for it (W4, D36).
//!
//! A reader that is one of this server's own transcodes says so in its URL:
//! the `ffmpeg:` driver writes `source=ffmpeg:...` in the query of a stream it
//! reads by name, as go2rtc does. Its FFmpeg was told not to guess the frame
//! rate (`crates/ffmpeg`, `RELAY_INPUT`), so the rate it encodes at is the
//! one this answer says - or 90000 pictures a second when it says none, which
//! x264 turns into pictures of half the bits (the owner's camera, 14 pictures
//! a second: 3.9 Mbit/s where FFmpeg's own guess gave 7.1). The pace of a
//! source is known [`oxrtc_core::picture_rate::RATE_GAPS`] pictures after its
//! first one ever and kept from then on, so only the first transcode of a
//! source that nothing held finds it unknown: its answer waits for those
//! pictures (0.2 s at 14 a second, 3 s at one), where FFmpeg's guess read
//! twenty. Every other reader is answered at once, as before.

use std::time::Duration;

use oxrtc_core::Subscription;
use tokio::time::Instant;

/// Longest wait for the pace of a transcode's pictures: three gaps of a
/// source of one picture a second, and a second for their jitter. A slower
/// source is answered without a rate after it (FFmpeg's guess needed twenty
/// of its pictures, more than the transcode's start timeout allows).
pub(crate) const PACE_WAIT: Duration = Duration::from_secs(4);

/// How often the wait looks at the source.
const PACE_POLL: Duration = Duration::from_millis(20);

/// Whether the reader of `query` says it is a transcode of this server.
pub(crate) fn reads_for_a_transcode(query: &str) -> bool {
    query.split('&').any(|pair| pair.strip_prefix("source=").is_some_and(|source| source.starts_with("ffmpeg:")))
}

/// The pace of `sub`'s pictures, waited for at most `limit` when it is not
/// known yet. The subscription is not read meanwhile: what its source sends
/// stays in its queue for `PLAY`.
pub(crate) async fn pace_within(sub: &mut Subscription, limit: Duration) -> Option<Duration> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(pace) = sub.picture_interval() {
            return Some(pace);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(PACE_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transcode_says_what_it_is_in_its_query() {
        for query in [
            "video&source=ffmpeg:camera_1_src%23video%3Dh264&queue=live",
            "source=ffmpeg:cam",
            "audio&source=ffmpeg:cam%23audio%3Dopus",
        ] {
            assert!(reads_for_a_transcode(query), "{query}");
        }
        for query in ["", "video", "video&queue=live", "source=rtsp://cam", "video&resource=ffmpeg:cam", "ffmpeg:cam"] {
            assert!(!reads_for_a_transcode(query), "{query}");
        }
    }
}
